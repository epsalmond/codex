//! Read-only navigation over an operator-built saved project index.

use crate::config::RootIdentity;
use codex_lsp_protocol::Location;
use codex_lsp_protocol::NavigationRequest;
use codex_lsp_protocol::NavigationResponse;
use codex_lsp_protocol::Operation;
use codex_lsp_protocol::Position;
use codex_lsp_protocol::Status;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use sqlx::ConnectOptions;
use sqlx::Connection;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteConnectOptions;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_FILES: usize = 20_000;
const MAX_ENTRIES: usize = 80_000;
const MAX_INPUT_FILE: u64 = 4 * 1024 * 1024;
const MAX_INPUT_BYTES: u64 = 192 * 1024 * 1024;
const MAX_DATABASE: u64 = 2 * 1024 * 1024 * 1024;
const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    pub id: String,
    pub checkout: PathBuf,
    /// Checkout-relative subproject. An empty path is the checkout itself.
    pub project: PathBuf,
    pub language: String,
    pub database: PathBuf,
    pub inputs: Vec<PathBuf>,
    /// Generated output trees excluded from the operator's source snapshot.
    #[serde(default)]
    pub generated_dirs: Vec<PathBuf>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Manifest {
    pub fingerprint: String,
    pub files: BTreeMap<String, String>,
}

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

fn normal(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(name) if !excluded(name)))
}

fn excluded(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | ".worktrees" | ".venv" | "node_modules" | ".next" | "__pycache__" | "vendor")
    )
}

impl IndexConfig {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !matches!(self.language.as_str(), "python" | "typescript")
            || (!self.project.as_os_str().is_empty() && !normal(&self.project))
            || self.inputs.is_empty()
            || self.inputs.len() > 64
            || self.inputs.iter().any(|input| {
                !(normal(input)
                    || (input.as_os_str().is_empty() && self.project.as_os_str().is_empty()))
            })
            || self.generated_dirs.len() > 64
            || self.generated_dirs.iter().any(|dir| {
                !normal(dir)
                    || !self.inputs.iter().any(|input| dir.starts_with(input))
                    || self.inputs.iter().any(|input| input.starts_with(dir))
                    || self.project.starts_with(dir)
            })
        {
            return Err(invalid("invalid index workspace"));
        }
        RootIdentity::read(&self.checkout)?;
        let git = fs::symlink_metadata(self.checkout.join(".git"))?;
        if !git.is_dir() && !git.is_file() {
            return Err(invalid("index checkout must have Git worktree metadata"));
        }
        RootIdentity::read(&self.checkout.join(&self.project))?;
        let parent = self
            .database
            .parent()
            .filter(|parent| parent.is_absolute() && !parent.starts_with(&self.checkout))
            .ok_or_else(|| invalid("index must be outside its checkout"))?;
        let metadata = fs::symlink_metadata(parent)?;
        if fs::canonicalize(parent)? != parent
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(invalid("index directory must be canonical and private"));
        }
        Ok(())
    }

    pub(crate) fn matches_path(&self, path: &str) -> bool {
        self.valid_source_path(path)
            && !self.generated(Path::new(path))
            && self
                .inputs
                .iter()
                .any(|input| Path::new(path).starts_with(input))
    }

    fn generated(&self, path: &Path) -> bool {
        self.generated_dirs.iter().any(|dir| path.starts_with(dir))
    }

    fn explicit_json(&self, path: &Path) -> bool {
        self.language == "typescript"
            && path.extension() == Some(OsStr::new("json"))
            && self.inputs.iter().any(|input| input == path)
    }

    pub(crate) fn valid_source_path(&self, path: &str) -> bool {
        let path = Path::new(path);
        normal(path) && self.source(path)
    }

    pub(crate) fn has_source(&self, path: &str) -> bool {
        self.open_source(Path::new(path))
            .is_ok_and(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()))
    }

    fn checkout_directory(&self) -> Result<File, &'static str> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&self.checkout)
            .map_err(|_| "index_input_unreadable")
    }

    fn open_source(&self, path: &Path) -> Result<File, &'static str> {
        let mut file = self.checkout_directory()?;
        for part in path.components() {
            file = open_child(&file, part.as_os_str())?;
        }
        Ok(file)
    }

    fn source(&self, path: &Path) -> bool {
        path.extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| crate::config::extensions(&self.language).contains(&extension))
    }

    fn metadata(&self, name: &OsStr) -> bool {
        let Some(name) = name.to_str() else {
            return false;
        };
        match self.language.as_str() {
            "python" => matches!(
                name,
                "pyproject.toml"
                    | "pyrefly.toml"
                    | "pyrightconfig.json"
                    | "poetry.lock"
                    | "uv.lock"
            ),
            "typescript" => {
                matches!(name, "package.json" | "yarn.lock" | "package-lock.json")
                    || (name.ends_with(".json")
                        && (name.starts_with("tsconfig") || name.starts_with("jsconfig")))
            }
            _ => false,
        }
    }

    pub fn manifest(&self, deadline: Instant) -> Result<Manifest, &'static str> {
        self.capture(deadline, &CancellationToken::new())
    }

    fn capture(
        &self,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Manifest, &'static str> {
        let root = self.checkout_directory()?;
        let mut scan = Inventory {
            config: self,
            files: BTreeMap::new(),
            entries: 0,
            bytes: 0,
            deadline,
            cancel,
        };
        for input in &self.inputs {
            let mut file = root.try_clone().map_err(|_| "index_input_unreadable")?;
            for part in input.components() {
                file = open_child(&file, part.as_os_str())?;
            }
            if file
                .metadata()
                .map_err(|_| "index_input_unreadable")?
                .is_dir()
            {
                scan.directory(&file, input)?;
            } else if self.source(input)
                || self.metadata(input.file_name().unwrap_or_default())
                || self.explicit_json(input)
            {
                scan.file(&mut file, input)?;
            } else {
                return Err("invalid_index_input");
            }
        }
        let mut hash = Sha256::new();
        hash.update(b"codex-managed-index-v1\0");
        for (path, digest) in &scan.files {
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
            hash.update(digest.as_bytes());
        }
        Ok(Manifest {
            fingerprint: format!("{:x}", hash.finalize()),
            files: scan.files,
        })
    }
}

struct Inventory<'a> {
    config: &'a IndexConfig,
    files: BTreeMap<String, String>,
    entries: usize,
    bytes: u64,
    deadline: Instant,
    cancel: &'a CancellationToken,
}

impl Inventory<'_> {
    fn directory(&mut self, directory: &File, relative: &Path) -> Result<(), &'static str> {
        if relative.components().count() > 128 {
            return Err("index_input_limit");
        }
        let directory_fd = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        if !relative.as_os_str().is_empty() {
            match fs::symlink_metadata(directory_fd.join(".git")) {
                Ok(_) => return Err("nested_index_checkout"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err("index_input_unreadable"),
            }
        }
        let entries = fs::read_dir(directory_fd).map_err(|_| "index_input_unreadable")?;
        for entry in entries {
            self.check_deadline()?;
            self.entries += 1;
            if self.entries > MAX_ENTRIES {
                return Err("index_input_limit");
            }
            let entry = entry.map_err(|_| "index_input_unreadable")?;
            let name = entry.file_name();
            if excluded(&name) {
                continue;
            }
            let path = relative.join(&name);
            if self.config.generated(&path) {
                continue;
            }
            let kind = entry.file_type().map_err(|_| "index_input_unreadable")?;
            if kind.is_dir() {
                let child = open_child(directory, &name)?;
                self.directory(&child, &path)?;
            } else if self.config.source(&path) || self.config.metadata(&name) {
                let mut file = open_child(directory, &name)?;
                self.file(&mut file, &path)?;
            }
        }
        Ok(())
    }

    fn file(&mut self, file: &mut File, path: &Path) -> Result<(), &'static str> {
        self.check_deadline()?;
        let path = path.to_str().ok_or("index_input_encoding")?;
        if self.files.contains_key(path) {
            return Ok(());
        }
        let metadata = file.metadata().map_err(|_| "index_input_unreadable")?;
        if !metadata.is_file() {
            return Err("index_input_unreadable");
        }
        if self.files.len() >= MAX_FILES || metadata.len() > MAX_INPUT_FILE {
            return Err("index_input_limit");
        }
        let mut hash = Sha256::new();
        let size = io::copy(&mut file.take(MAX_INPUT_FILE + 1), &mut hash)
            .map_err(|_| "index_input_unreadable")?;
        self.bytes += size;
        if size > MAX_INPUT_FILE || self.bytes > MAX_INPUT_BYTES {
            return Err("index_input_limit");
        }
        self.files
            .insert(path.to_owned(), format!("{:x}", hash.finalize()));
        Ok(())
    }

    fn check_deadline(&self) -> Result<(), &'static str> {
        if self.cancel.is_cancelled() {
            Err("cancelled")
        } else if Instant::now() >= self.deadline {
            Err("index_input_timeout")
        } else {
            Ok(())
        }
    }
}

// Every path component is opened relative to a held descriptor. A worktree
// symlink or a concurrent directory replacement cannot redirect the data read.
fn open_child(parent: &File, name: &OsStr) -> Result<File, &'static str> {
    let name = CString::new(name.as_bytes()).map_err(|_| "index_input_encoding")?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err("index_input_unreadable");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn status(state: Status, reason: &'static str) -> NavigationResponse {
    NavigationResponse::status(state, reason)
}

fn database(config: &IndexConfig) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&config.database)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_DATABASE
    {
        return Err(invalid("unsafe index file"));
    }
    Ok(file)
}

async fn captured(config: &IndexConfig, work: &LookupWork) -> Result<Manifest, NavigationResponse> {
    let config = config.clone();
    let work = work.clone();
    tokio::task::spawn_blocking(move || {
        // Retain admission if a client disconnects before the blocking scan ends.
        let _permit = work.permit;
        config.capture(work.deadline, &work.cancel)
    })
    .await
    .map_err(|_| status(Status::Unavailable, "index_input_unreadable"))?
    .map_err(|reason| {
        let state = match reason {
            "cancelled" => Status::Cancelled,
            "index_input_timeout" => Status::Timeout,
            _ => Status::Stale,
        };
        status(state, reason)
    })
}

#[derive(Clone)]
struct LookupWork {
    deadline: Instant,
    cancel: CancellationToken,
    permit: Arc<OwnedSemaphorePermit>,
}

pub(crate) async fn query(
    config: &IndexConfig,
    identity: &RootIdentity,
    request: &NavigationRequest,
    deadline: Instant,
    cancel: CancellationToken,
    permit: OwnedSemaphorePermit,
) -> NavigationResponse {
    let work = LookupWork {
        deadline,
        cancel: cancel.clone(),
        permit: Arc::new(permit),
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(status(Status::Cancelled, "cancelled")),
        _ = tokio::time::sleep_until(deadline) => Err(status(Status::Timeout, "deadline_exceeded")),
        result = lookup(config, identity, request, &work) => result,
    };
    result.unwrap_or_else(|response| response)
}

#[expect(
    clippy::disallowed_methods,
    reason = "Read sealed external artifacts via an owned file descriptor, without a mutable state connection"
)]
async fn lookup(
    config: &IndexConfig,
    identity: &RootIdentity,
    request: &NavigationRequest,
    work: &LookupWork,
) -> Result<NavigationResponse, NavigationResponse> {
    let check_root = || {
        if RootIdentity::read(&config.checkout).ok().as_ref() == Some(identity) {
            Ok(())
        } else {
            Err(status(Status::Stale, "index_checkout_changed"))
        }
    };
    check_root()?;
    let database = database(config).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            status(Status::Indexing, "index_not_built")
        } else {
            status(Status::Unavailable, "invalid_index")
        }
    })?;
    let invalid = |_| status(Status::Unavailable, "invalid_index");
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(format!("/proc/self/fd/{}", database.as_raw_fd()))
            .read_only(true)
            .immutable(true)
            .pragma("trusted_schema", "OFF")
            .disable_statement_logging(),
    )
    .await
    .map_err(invalid)?;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .map_err(invalid)?;
    if version != SCHEMA_VERSION {
        return Err(status(Status::Unavailable, "unsupported_index_schema"));
    }
    let generations: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT workspace, checkout, fingerprint FROM generation
         WHERE length(workspace) <= 128 AND length(checkout) <= 4096 AND length(fingerprint) = 64
         LIMIT 2",
    )
    .fetch_all(&mut connection)
    .await
    .map_err(invalid)?;
    let [(id, checkout, fingerprint)] = generations.as_slice() else {
        return Err(status(Status::Unavailable, "invalid_index_generation"));
    };
    let current = captured(config, work).await?;
    if id != &config.id
        || Path::new(checkout) != config.checkout
        || fingerprint != &current.fingerprint
    {
        return Err(status(Status::Stale, "index_generation_changed"));
    }
    if !current.files.contains_key(&request.path) {
        return Err(status(Status::InvalidRequest, "invalid_source_path"));
    }
    let covered: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM documents WHERE path = ?)")
        .bind(&request.path)
        .fetch_one(&mut connection)
        .await
        .map_err(invalid)?;
    if covered == 0 {
        return Err(status(Status::Unavailable, "index_source_not_covered"));
    }
    let position = request.position;
    let definition_only = request.operation == Operation::Definition;
    let limit = request.max_results.min(codex_lsp_protocol::MAX_RESULTS);
    let rows: Vec<(String, u32, u32, u32, u32)> = sqlx::query_as(
        "WITH selected AS (
             SELECT symbol FROM occurrences WHERE path = ?1
             AND (start_line, start_column) <= (?2, ?3)
             AND (end_line, end_column) > (?2, ?3)
             ORDER BY end_line-start_line, end_column-start_column, symbol LIMIT 1
         )
         SELECT DISTINCT path, start_line, start_column, end_line, end_column
         FROM occurrences WHERE symbol = (SELECT symbol FROM selected)
         AND (?4 = 0 OR definition = 1)
         ORDER BY path, start_line, start_column, end_line, end_column LIMIT ?5",
    )
    .bind(&request.path)
    .bind(i64::from(position.line))
    .bind(i64::from(position.column))
    .bind(definition_only)
    .bind((limit + 1) as i64)
    .fetch_all(&mut connection)
    .await
    .map_err(invalid)?;
    let truncated = rows.len() > limit;
    let mut locations = Vec::with_capacity(rows.len().min(limit));
    for (path, start_line, start_column, end_line, end_column) in rows.into_iter().take(limit) {
        if !current.files.contains_key(&path)
            || !(config.source(Path::new(&path)) || config.explicit_json(Path::new(&path)))
            || start_line == 0
            || start_column == 0
            || end_line == 0
            || end_column == 0
            || (start_line, start_column) >= (end_line, end_column)
        {
            return Err(status(Status::Unavailable, "invalid_index_location"));
        }
        locations.push(Location {
            path,
            start: Position {
                line: start_line,
                column: start_column,
            },
            end: Position {
                line: end_line,
                column: end_column,
            },
        });
    }
    if captured(config, work).await?.fingerprint != current.fingerprint {
        return Err(status(Status::Stale, "index_generation_changed"));
    }
    check_root()?;
    let mut response = NavigationResponse::ok(locations);
    response.truncated = truncated;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    fn fixture(root: &Path) -> IndexConfig {
        let checkout = root.join("checkout");
        let database_dir = root.join("index");
        fs::create_dir_all(checkout.join("apps/project")).unwrap();
        fs::create_dir(checkout.join(".git")).unwrap();
        fs::create_dir(database_dir.as_path()).unwrap();
        fs::set_permissions(&database_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            checkout.join("apps/project/importer.py"),
            "from origin import value\n",
        )
        .unwrap();
        fs::write(checkout.join("apps/project/origin.py"), "value = 1\n").unwrap();
        IndexConfig {
            id: "project".to_owned(),
            checkout,
            project: PathBuf::from("apps/project"),
            language: "python".to_owned(),
            database: database_dir.join("project.sqlite3"),
            inputs: vec![PathBuf::from("apps/project")],
            generated_dirs: vec![PathBuf::from("apps/project/generated")],
        }
    }

    #[tokio::test]
    async fn manifest_skips_dependency_caches_and_detects_saved_input_changes() {
        let dir = tempfile::tempdir().unwrap();
        let config = fixture(dir.path());
        let app = config.checkout.join(&config.project);
        let manifest = || {
            config
                .manifest(Instant::now() + Duration::from_secs(2))
                .unwrap()
        };
        let before = manifest();
        fs::write(app.join(".env"), "CANARY=ignored").unwrap();
        fs::create_dir(app.join(".venv")).unwrap();
        fs::write(app.join(".venv/ignored.py"), "print('ignored')").unwrap();
        fs::create_dir(app.join("generated")).unwrap();
        fs::write(app.join("generated/duplicate.py"), "value = 2\n").unwrap();
        assert!(!config.matches_path("apps/project/generated/duplicate.py"));
        assert_eq!(before, manifest());
        fs::write(app.join("untracked.py"), "from origin import value").unwrap();
        assert_ne!(before, manifest());
        fs::remove_file(app.join("untracked.py")).unwrap();
        assert_eq!(before, manifest());
        fs::write(app.join("origin.py"), "value = 2\n").unwrap();
        assert_ne!(before, manifest());
        std::os::unix::fs::symlink(dir.path().join("external.py"), app.join("link.py")).unwrap();
        assert_eq!(
            config.manifest(Instant::now() + Duration::from_secs(2)),
            Err("index_input_unreadable")
        );
    }

    #[tokio::test]
    async fn explicitly_admitted_typescript_json_invalidates_manifest_when_edited() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = fixture(dir.path());
        config.language = "typescript".to_owned();
        let shared = config.checkout.join("packages/ts-config");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("base.json"), "{}\n").unwrap();
        fs::write(shared.join("other.json"), "{}\n").unwrap();
        config.inputs.extend([
            PathBuf::from("packages/ts-config"),
            PathBuf::from("packages/ts-config/base.json"),
        ]);
        config.validate().unwrap();
        let manifest = || {
            config
                .manifest(Instant::now() + Duration::from_secs(2))
                .unwrap()
        };
        let before = manifest();
        assert!(before.files.contains_key("packages/ts-config/base.json"));
        assert!(!before.files.contains_key("packages/ts-config/other.json"));
        fs::write(shared.join("base.json"), "{\"compilerOptions\": {}}\n").unwrap();
        assert_ne!(before.fingerprint, manifest().fingerprint);
    }

    #[tokio::test]
    #[expect(
        clippy::disallowed_methods,
        reason = "Create one private immutable test artifact for the read-side API"
    )]
    async fn references_are_checkout_relative_and_fail_closed_after_another_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let config = fixture(dir.path());
        config.validate().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let manifest = config.manifest(deadline).unwrap();
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&config.database)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "PRAGMA user_version = 1;
             CREATE TABLE generation (workspace TEXT, checkout TEXT, fingerprint TEXT);
             CREATE TABLE documents (path TEXT);
             CREATE TABLE occurrences (path TEXT, start_line INTEGER, start_column INTEGER,
                end_line INTEGER, end_column INTEGER, symbol TEXT, definition INTEGER);
             INSERT INTO documents VALUES('apps/project/importer.py'), ('apps/project/origin.py');
             INSERT INTO occurrences VALUES
                ('apps/project/importer.py', 1, 20, 1, 25, 'origin:value', 0),
                ('apps/project/origin.py', 1, 1, 1, 6, 'origin:value', 1);",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        sqlx::query("INSERT INTO generation VALUES (?, ?, ?)")
            .bind(&config.id)
            .bind(config.checkout.to_str().unwrap())
            .bind(manifest.fingerprint)
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        fs::set_permissions(&config.database, fs::Permissions::from_mode(0o600)).unwrap();

        let mut request = NavigationRequest {
            version: codex_lsp_protocol::PROTOCOL_VERSION,
            cwd: config.checkout.clone(),
            path: "apps/project/importer.py".to_owned(),
            operation: Operation::Definition,
            position: Position {
                line: 1,
                column: 20,
            },
            max_results: 10,
            max_response_bytes: 2048,
            timeout_ms: 1000,
        };
        let identity = RootIdentity::read(&config.checkout).unwrap();
        let work = LookupWork {
            deadline,
            cancel: CancellationToken::new(),
            permit: Arc::new(Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap()),
        };
        let definition = lookup(&config, &identity, &request, &work).await.unwrap();
        assert_eq!(definition.locations[0].path, "apps/project/origin.py");
        request.operation = Operation::References;
        let references = lookup(&config, &identity, &request, &work).await.unwrap();
        assert_eq!(references.locations.len(), 2);
        fs::write(
            config.checkout.join("apps/project/new_ref.py"),
            "from origin import value\n",
        )
        .unwrap();
        let changed = lookup(&config, &identity, &request, &work)
            .await
            .unwrap_err();
        assert_eq!(changed.status, Status::Stale);
        assert!(changed.locations.is_empty());
    }
}
