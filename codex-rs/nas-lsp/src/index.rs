//! Read-only navigation over an operator-built saved project index.

use crate::config::RootIdentity;
pub use crate::prepared::PreparedIndex;
use crate::prepared::SelectionError;
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
const CARGO_CONFIG_NAMES: [&str; 2] = ["config", "config.toml"];

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    pub id: String,
    pub checkout: PathBuf,
    /// Checkout-relative subproject. An empty path is the checkout itself.
    pub project: PathBuf,
    pub language: String,
    pub database: Option<PathBuf>,
    pub prepared: Option<PreparedIndex>,
    pub inputs: Vec<PathBuf>,
    /// Generated output trees excluded from the operator's source snapshot.
    #[serde(default)]
    pub generated_dirs: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Manifest {
    pub fingerprint: String,
    pub files: BTreeMap<String, String>,
}

impl Manifest {
    pub(crate) fn fingerprint_for(files: &BTreeMap<String, String>) -> String {
        let mut hash = Sha256::new();
        hash.update(b"codex-managed-index-v1\0");
        for (path, digest) in files {
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
            hash.update(digest.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }
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
            || !matches!(self.language.as_str(), "python" | "typescript" | "rust")
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
        if self.language == "rust"
            && (!self.inputs.contains(&self.project)
                || self.inputs.iter().any(|input| {
                    input != &self.project
                        && !self.ancestor_cargo_configs().any(|path| path == *input)
                }))
        {
            return Err(invalid(
                "Rust index inputs must be its project and explicit ancestor Cargo configs",
            ));
        }
        RootIdentity::read(&self.checkout)?;
        let git = fs::symlink_metadata(self.checkout.join(".git"))?;
        if !git.is_dir() && !git.is_file() {
            return Err(invalid("index checkout must have Git worktree metadata"));
        }
        RootIdentity::read(&self.checkout.join(&self.project))?;
        if let Some(prepared) = &self.prepared {
            prepared.validate(&self.checkout)?;
        }
        let Some(database) = &self.database else {
            return if self.prepared.is_some() {
                Ok(())
            } else {
                Err(invalid("configure a database or prepared catalog"))
            };
        };
        let parent = database
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

    fn ancestor_cargo_configs(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.project
            .ancestors()
            .skip(1)
            .flat_map(|ancestor| CARGO_CONFIG_NAMES.map(|name| ancestor.join(".cargo").join(name)))
    }

    pub(crate) fn valid_source_path(&self, path: &str) -> bool {
        let path = Path::new(path);
        normal(path) && self.source(path)
    }

    pub(crate) fn has_source(checkout: &Path, path: &str) -> bool {
        Self::open_source(checkout, Path::new(path))
            .is_ok_and(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()))
    }

    fn checkout_directory(checkout: &Path) -> Result<File, &'static str> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(checkout)
            .map_err(|_| "index_input_unreadable")
    }

    fn open_source(checkout: &Path, path: &Path) -> Result<File, &'static str> {
        open_relative(&Self::checkout_directory(checkout)?, path).map_err(input_error)
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
        let root = Self::checkout_directory(&self.checkout)?;
        let mut scan = Inventory {
            config: self,
            files: BTreeMap::new(),
            entries: 0,
            bytes: 0,
            deadline,
            cancel,
        };
        for input in &self.inputs {
            if self.language == "rust" && input != &self.project {
                continue;
            }
            let mut file = open_relative(&root, input).map_err(input_error)?;
            if file
                .metadata()
                .map_err(|_| "index_input_unreadable")?
                .is_dir()
            {
                scan.directory(&file, input)?;
            } else if self.language != "rust"
                && (self.source(input)
                    || self.metadata(input.file_name().unwrap_or_default())
                    || self.explicit_json(input))
            {
                scan.file(&mut file, input)?;
            } else {
                return Err("invalid_index_input");
            }
        }
        if self.language == "rust" {
            // Cargo also searches ancestor directories. An absent config is
            // allowed, but saving an unregistered one must invalidate lookups.
            for path in self.ancestor_cargo_configs() {
                scan.check_deadline()?;
                match open_relative(&root, &path) {
                    Ok(mut file) if self.inputs.contains(&path) => scan.file(&mut file, &path)?,
                    Ok(_) => return Err("unregistered_cargo_config"),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(input_error(error)),
                }
            }
        }
        Ok(Manifest {
            fingerprint: Manifest::fingerprint_for(&scan.files),
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
                let child = open_child(directory, &name).map_err(input_error)?;
                self.directory(&child, &path)?;
            } else if self.config.language == "rust"
                || self.config.source(&path)
                || self.config.metadata(&name)
            {
                if self.config.language == "rust" && !kind.is_file() {
                    return Err("index_input_unreadable");
                }
                let mut file = open_child(directory, &name).map_err(input_error)?;
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
fn open_relative(parent: &File, path: &Path) -> io::Result<File> {
    let mut file = parent.try_clone()?;
    for part in path.components() {
        file = open_child(&file, part.as_os_str())?;
    }
    Ok(file)
}

fn input_error(error: io::Error) -> &'static str {
    if error.kind() == io::ErrorKind::InvalidInput {
        "index_input_encoding"
    } else {
        "index_input_unreadable"
    }
}

fn open_child(parent: &File, name: &OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn status(state: Status, reason: &'static str) -> NavigationResponse {
    NavigationResponse::status(state, reason)
}

fn open_database(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
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

async fn select_database(
    config: &IndexConfig,
    current: Option<Arc<Manifest>>,
    work: &LookupWork,
) -> Result<(File, PathBuf), NavigationResponse> {
    let not_built = || status(Status::Indexing, "index_not_built");
    let invalid = || status(Status::Unavailable, "invalid_index");
    if let Some(prepared) = &config.prepared {
        let current = current.ok_or_else(invalid)?;
        let prepared = prepared.clone();
        let workspace = config.id.clone();
        let permit = work.permit.clone();
        let can_fallback = config.database.is_some();
        let current_fingerprint = current.fingerprint.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            prepared.select(&workspace, &current, MAX_DATABASE, can_fallback)
        })
        .await
        .map_err(|_| invalid())?;
        match prepared {
            Ok(selected) if selected.fingerprint == current_fingerprint => {
                return Ok((selected.file, selected.checkout));
            }
            Ok(selected) => {
                // A nonmatching candidate can permit fallback only after its
                // own database generation has been verified.
                let (_, generation) = open_generation(&selected.file).await?;
                if generation.0 != config.id
                    || Path::new(&generation.1) != selected.checkout
                    || generation.2 != selected.fingerprint
                {
                    return Err(invalid());
                }
            }
            Err(SelectionError::NotBuilt) if can_fallback => {}
            Err(SelectionError::NotBuilt) => return Err(not_built()),
            Err(SelectionError::NoMatch) => {
                return Err(status(Status::Stale, "index_generation_changed"));
            }
            Err(SelectionError::Invalid) => return Err(invalid()),
        }
    }
    let database = config.database.as_deref().ok_or_else(invalid)?;
    open_database(database)
        .map(|database| (database, config.checkout.clone()))
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                not_built()
            } else {
                invalid()
            }
        })
}

#[expect(
    clippy::disallowed_methods,
    reason = "Read sealed external artifacts via an owned file descriptor, without a mutable state connection"
)]
async fn open_generation(
    database: &File,
) -> Result<(SqliteConnection, (String, String, String)), NavigationResponse> {
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
    let Ok([generation]) = <[_; 1]>::try_from(generations) else {
        return Err(status(Status::Unavailable, "invalid_index_generation"));
    };
    Ok((connection, generation))
}

async fn lookup(
    config: &IndexConfig,
    identity: &RootIdentity,
    request: &NavigationRequest,
    work: &LookupWork,
) -> Result<NavigationResponse, NavigationResponse> {
    if !config.matches_path(&request.path) {
        return Err(status(Status::InvalidRequest, "invalid_source_path"));
    }
    let check_root = || {
        if RootIdentity::read(&config.checkout).ok().as_ref() == Some(identity) {
            Ok(())
        } else {
            Err(status(Status::Stale, "index_checkout_changed"))
        }
    };
    check_root()?;
    let current = if config.prepared.is_some() {
        Some(Arc::new(captured(config, work).await?))
    } else {
        None
    };
    let (database, expected_checkout) = select_database(config, current.clone(), work).await?;
    let (mut connection, (id, checkout, fingerprint)) = open_generation(&database).await?;
    let invalid = |_| status(Status::Unavailable, "invalid_index");
    let current = match current {
        Some(current) => current,
        None => Arc::new(captured(config, work).await?),
    };
    if id != config.id
        || Path::new(&checkout) != expected_checkout
        || fingerprint != current.fingerprint
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
            || !(config.matches_path(&path) || config.explicit_json(Path::new(&path)))
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
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    const PYTHON_SOURCES: (&str, &str) = ("apps/project/importer.py", "apps/project/origin.py");
    const RUST_SOURCES: (&str, &str) = ("codex-rs/src/importer.rs", "codex-rs/src/origin.rs");

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
            database: Some(database_dir.join("project.sqlite3")),
            prepared: None,
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

    fn rust_fixture(root: &Path) -> IndexConfig {
        let mut config = fixture(root);
        config.language = "rust".to_owned();
        config.project = PathBuf::from("codex-rs");
        config.inputs = ["codex-rs", ".cargo/config.toml", ".cargo/config"]
            .map(PathBuf::from)
            .to_vec();
        config.generated_dirs = vec![PathBuf::from("codex-rs/target")];
        for dir in [".cargo", "codex-rs/.cargo", "codex-rs/src"] {
            fs::create_dir_all(config.checkout.join(dir)).unwrap();
        }
        for (path, source) in [
            (".cargo/config.toml", "[build]\n"),
            ("codex-rs/.cargo/config.toml", "[env]\n"),
            ("codex-rs/Cargo.toml", "[workspace]\n"),
            ("codex-rs/Cargo.lock", "version = 4\n"),
            ("codex-rs/rust-toolchain.toml", "[toolchain]\n"),
            ("codex-rs/src/data.bin", "opaque data"),
            (RUST_SOURCES.0, "fn run() { origin::value(); }\n"),
            (RUST_SOURCES.1, "pub fn value() {}\n"),
        ] {
            fs::write(config.checkout.join(path), source).unwrap();
        }
        config.validate().unwrap();
        config
    }

    #[test]
    fn rust_manifest_covers_saved_project_and_ancestor_cargo_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = rust_fixture(dir.path());
        let deadline = Instant::now() + Duration::from_secs(10);
        let before = config.manifest(deadline).unwrap();
        assert_eq!(before.files.len(), 8);
        for path in before.files.keys() {
            let path = config.checkout.join(path);
            let original = fs::read(&path).unwrap();
            fs::write(&path, b"edited").unwrap();
            assert_ne!(before, config.manifest(deadline).unwrap());
            fs::write(path, original).unwrap();
        }
        for folder in ["codex-rs/target", "codex-rs/vendor"] {
            let output = config.checkout.join(folder);
            fs::create_dir(&output).unwrap();
            fs::write(output.join("artifact.rs"), "ignored").unwrap();
        }
        fs::write(config.checkout.join("outside.rs"), "out of scope").unwrap();
        assert_eq!(before, config.manifest(deadline).unwrap());

        let legacy = config.checkout.join(".cargo/config");
        fs::write(&legacy, "[env]\n").unwrap();
        assert_ne!(before, config.manifest(deadline).unwrap());
        config
            .inputs
            .retain(|path| path != Path::new(".cargo/config"));
        assert_eq!(config.manifest(deadline), Err("unregistered_cargo_config"));
        fs::remove_file(legacy).unwrap();
        assert_eq!(before, config.manifest(deadline).unwrap());

        config.inputs.push(PathBuf::from("outside.rs"));
        assert!(config.validate().is_err());
    }

    #[test]
    fn rust_manifest_rejects_large_and_unsafe_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let config = rust_fixture(dir.path());
        let deadline = Instant::now() + Duration::from_secs(10);
        let path = config.checkout.join("codex-rs/src/include.dat");
        File::create(&path)
            .unwrap()
            .set_len(MAX_INPUT_FILE + 1)
            .unwrap();
        assert_eq!(config.manifest(deadline), Err("index_input_limit"));
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(dir.path().join("external.dat"), path).unwrap();
        assert_eq!(config.manifest(deadline), Err("index_input_unreadable"));
    }

    pub(crate) async fn build_database(
        database: &Path,
        workspace: &str,
        checkout: &Path,
        manifest: &Manifest,
    ) {
        build_database_with_sources(database, workspace, checkout, manifest, PYTHON_SOURCES).await;
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "Create one private immutable test artifact for the read-side API"
    )]
    async fn build_database_with_sources(
        database: &Path,
        workspace: &str,
        checkout: &Path,
        manifest: &Manifest,
        sources: (&str, &str),
    ) {
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(database)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "PRAGMA user_version = 1;
             CREATE TABLE generation (workspace TEXT, checkout TEXT, fingerprint TEXT);
             CREATE TABLE documents (path TEXT);
             CREATE TABLE occurrences (path TEXT, start_line INTEGER, start_column INTEGER,
                end_line INTEGER, end_column INTEGER, symbol TEXT, definition INTEGER);",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        sqlx::query("INSERT INTO documents VALUES (?), (?)")
            .bind(sources.0)
            .bind(sources.1)
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO occurrences VALUES
             (?, 1, 20, 1, 25, 'origin:value', 0),
             (?, 1, 1, 1, 6, 'origin:value', 1)",
        )
        .bind(sources.0)
        .bind(sources.1)
        .execute(&mut connection)
        .await
        .unwrap();
        sqlx::query("INSERT INTO generation VALUES (?, ?, ?)")
            .bind(workspace)
            .bind(checkout.to_str().unwrap())
            .bind(&manifest.fingerprint)
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
    }

    fn definition_request(config: &IndexConfig) -> NavigationRequest {
        NavigationRequest {
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
        }
    }

    async fn test_work(deadline: Instant) -> LookupWork {
        LookupWork {
            deadline,
            cancel: CancellationToken::new(),
            permit: Arc::new(Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap()),
        }
    }

    #[tokio::test]
    async fn references_are_checkout_relative_and_fail_closed_after_another_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let config = fixture(dir.path());
        config.validate().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let manifest = config.manifest(deadline).unwrap();
        let database = config.database.as_ref().unwrap();
        build_database(database, &config.id, &config.checkout, &manifest).await;
        fs::set_permissions(database, fs::Permissions::from_mode(0o600)).unwrap();

        let mut request = definition_request(&config);
        let identity = RootIdentity::read(&config.checkout).unwrap();
        let work = test_work(deadline).await;
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

    pub(crate) async fn publish_fixture(
        prepared: &PreparedIndex,
        workspace: &str,
        source_commit: &str,
        manifest: &Manifest,
    ) -> PathBuf {
        publish_fixture_with_sources(prepared, workspace, source_commit, manifest, PYTHON_SOURCES)
            .await
    }

    async fn publish_fixture_with_sources(
        prepared: &PreparedIndex,
        workspace: &str,
        source_commit: &str,
        manifest: &Manifest,
        sources: (&str, &str),
    ) -> PathBuf {
        use serde_json::json;

        let producer = "a".repeat(64);
        let manifest_tool = "b".repeat(64);
        let history = prepared.catalog.join("by-fingerprint");
        let generations = prepared.catalog.join("generations");
        let generation = generations.join(format!(
            "{}-{producer}-{manifest_tool}",
            manifest.fingerprint
        ));
        for directory in [&history, &generations, &generation] {
            fs::create_dir_all(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let database = generation.join("index.sqlite3");
        let manifest_path = generation.join("manifest.json");
        build_database_with_sources(
            &database,
            workspace,
            &prepared.publisher_checkout,
            manifest,
            sources,
        )
        .await;
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        for immutable in [&database, &manifest_path] {
            fs::set_permissions(immutable, fs::Permissions::from_mode(0o400)).unwrap();
        }
        let receipt = serde_json::to_vec(&json!({
            "schema_version": 1, "state": "ready_for_broker", "workspace": workspace,
            "checkout": prepared.publisher_checkout,
            "git_common_dir": prepared.publisher_checkout.join(".git"),
            "source_commit": source_commit,
            "manifest_fingerprint": manifest.fingerprint, "manifest": manifest_path,
            "database": database,
            "database_sha256": format!("{:x}", Sha256::digest(fs::read(&database).unwrap())),
            "database_size_bytes": fs::metadata(&database).unwrap().len(),
            "verified_documents": 2, "manifest_tool_commit": "c".repeat(40),
            "manifest_tool_sha256": manifest_tool, "producer_sha256": producer,
        }))
        .unwrap();
        let by_fingerprint = history.join(format!("{}.json", manifest.fingerprint));
        for path in [&by_fingerprint, &prepared.catalog.join("candidate.json")] {
            fs::write(path, &receipt).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        by_fingerprint
    }

    #[tokio::test]
    async fn rust_lookup_selects_historical_index_by_exact_saved_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = rust_fixture(dir.path());
        let prepared = PreparedIndex {
            catalog: config.database.take().unwrap().parent().unwrap().to_owned(),
            publisher_checkout: dir.path().join("publisher"),
        };
        config.prepared = Some(prepared.clone());
        config.validate().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let old = config.manifest(deadline).unwrap();
        publish_fixture_with_sources(&prepared, &config.id, &"a".repeat(40), &old, RUST_SOURCES)
            .await;
        let cargo_lock = config.checkout.join("codex-rs/Cargo.lock");
        let original_lock = fs::read(&cargo_lock).unwrap();
        fs::write(&cargo_lock, b"version = 5\n").unwrap();
        let current = config.manifest(deadline).unwrap();
        publish_fixture_with_sources(
            &prepared,
            &config.id,
            &"b".repeat(40),
            &current,
            RUST_SOURCES,
        )
        .await;

        let mut request = definition_request(&config);
        request.path = RUST_SOURCES.0.to_owned();
        let identity = RootIdentity::read(&config.checkout).unwrap();
        let work = test_work(deadline).await;
        let current_definition = lookup(&config, &identity, &request, &work).await.unwrap();
        assert_eq!(current_definition.locations[0].path, RUST_SOURCES.1);

        fs::write(&cargo_lock, original_lock).unwrap();
        request.operation = Operation::References;
        let historical_references = lookup(&config, &identity, &request, &work).await.unwrap();
        assert_eq!(historical_references.locations.len(), 2);

        for input in [
            ".cargo/config.toml",
            "codex-rs/src/data.bin",
            RUST_SOURCES.0,
        ] {
            let path = config.checkout.join(input);
            let original = fs::read(&path).unwrap();
            fs::write(&path, b"local edit").unwrap();
            let stale = lookup(&config, &identity, &request, &work)
                .await
                .unwrap_err();
            assert_eq!(stale.status, Status::Stale);
            assert!(stale.locations.is_empty());
            fs::write(path, original).unwrap();
        }
    }

    #[tokio::test]
    async fn rust_lookup_rejects_artifact_locations_outside_its_source_scope() {
        let dir = tempfile::tempdir().unwrap();
        let config = rust_fixture(dir.path());
        let deadline = Instant::now() + Duration::from_secs(10);
        let manifest = config.manifest(deadline).unwrap();
        let identity = RootIdentity::read(&config.checkout).unwrap();
        let work = test_work(deadline).await;
        let mut request = definition_request(&config);
        request.path = RUST_SOURCES.0.to_owned();
        let database = config.database.as_ref().unwrap();
        for bad_location in [
            "codex-rs/src/data.bin",
            "outside.rs",
            "codex-rs/target/example.rs",
        ] {
            build_database_with_sources(
                database,
                &config.id,
                &config.checkout,
                &manifest,
                (RUST_SOURCES.0, bad_location),
            )
            .await;
            fs::set_permissions(database, fs::Permissions::from_mode(0o600)).unwrap();
            let response = lookup(&config, &identity, &request, &work)
                .await
                .unwrap_err();
            assert_eq!(response.reason.as_deref(), Some("invalid_index_location"));
            fs::remove_file(database).unwrap();
        }
    }

    #[tokio::test]
    async fn matching_saved_inputs_select_a_retained_generation_from_an_independent_publisher() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = fixture(dir.path());
        let prepared = PreparedIndex {
            catalog: config.database.take().unwrap().parent().unwrap().to_owned(),
            publisher_checkout: dir.path().join("independent-publisher-clone"),
        };
        config.prepared = Some(prepared.clone());
        config.validate().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let saved = config.manifest(deadline).unwrap();
        let old_receipt = publish_fixture(&prepared, &config.id, &"a".repeat(40), &saved).await;
        let source = config.checkout.join("apps/project/origin.py");
        let original = fs::read(&source).unwrap();
        fs::write(&source, "value = 7\n").unwrap();
        let newer = config.manifest(deadline).unwrap();
        assert_ne!(newer.fingerprint, saved.fingerprint);
        publish_fixture(&prepared, &config.id, &"b".repeat(40), &newer).await;
        fs::write(&source, &original).unwrap();

        let request = definition_request(&config);
        let identity = RootIdentity::read(&config.checkout).unwrap();
        let work = test_work(deadline).await;
        let definition = lookup(&config, &identity, &request, &work).await.unwrap();
        assert_eq!(definition.locations[0].path, "apps/project/origin.py");
        assert!(!prepared.publisher_checkout.exists());

        fs::write(&source, "value = 9\n").unwrap();
        let stale = lookup(&config, &identity, &request, &work)
            .await
            .unwrap_err();
        assert_eq!(stale.status, Status::Stale);
        assert!(stale.locations.is_empty());

        fs::write(&source, &original).unwrap();
        let receipt_bytes = fs::read(&old_receipt).unwrap();
        let mut invalid_receipt: serde_json::Value =
            serde_json::from_slice(&receipt_bytes).unwrap();
        invalid_receipt["checkout"] = serde_json::json!(config.checkout);
        fs::write(old_receipt, serde_json::to_vec(&invalid_receipt).unwrap()).unwrap();
        let invalid = lookup(&config, &identity, &request, &work)
            .await
            .unwrap_err();
        assert_eq!(invalid.status, Status::Unavailable);
        assert!(invalid.locations.is_empty());
    }

    #[tokio::test]
    async fn dual_source_falls_back_only_when_no_valid_prepared_match_exists() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = fixture(dir.path());
        let database = config.database.as_ref().unwrap();
        let catalog = dir.path().join("catalog");
        fs::create_dir(&catalog).unwrap();
        fs::set_permissions(&catalog, fs::Permissions::from_mode(0o700)).unwrap();
        let prepared = PreparedIndex {
            catalog,
            publisher_checkout: dir.path().join("publisher"),
        };
        config.prepared = Some(prepared.clone());
        config.validate().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let saved = config.manifest(deadline).unwrap();
        build_database(database, &config.id, &config.checkout, &saved).await;
        fs::set_permissions(database, fs::Permissions::from_mode(0o600)).unwrap();

        let request = definition_request(&config);
        let identity = RootIdentity::read(&config.checkout).unwrap();
        let work = test_work(deadline).await;
        let definition = || lookup(&config, &identity, &request, &work);
        assert_eq!(definition().await.unwrap().status, Status::Ok);

        let moved_catalog = dir.path().join("moved-catalog");
        fs::rename(&prepared.catalog, &moved_catalog).unwrap();
        let missing_catalog = definition().await.unwrap_err();
        assert_eq!(missing_catalog.reason.as_deref(), Some("invalid_index"));
        std::os::unix::fs::symlink(&moved_catalog, &prepared.catalog).unwrap();
        let unsafe_catalog = definition().await.unwrap_err();
        assert_eq!(unsafe_catalog.reason.as_deref(), Some("invalid_index"));
        fs::remove_file(&prepared.catalog).unwrap();
        fs::rename(&moved_catalog, &prepared.catalog).unwrap();

        let history = prepared.catalog.join("by-fingerprint");
        fs::create_dir(&history).unwrap();
        fs::set_permissions(&history, fs::Permissions::from_mode(0o755)).unwrap();
        let unsafe_history = definition().await.unwrap_err();
        assert_eq!(unsafe_history.reason.as_deref(), Some("invalid_index"));
        fs::remove_dir(&history).unwrap();

        let source = config.checkout.join("apps/project/origin.py");
        fs::write(&source, "value = 7\n").unwrap();
        let newer = config.manifest(deadline).unwrap();
        publish_fixture(&prepared, &config.id, &"a".repeat(40), &newer).await;
        // The older SQLite is stale at this fingerprint. A successful result
        // proves the prepared generation was selected instead.
        assert_eq!(definition().await.unwrap().status, Status::Ok);

        fs::write(&source, "value = 8\n").unwrap();
        assert_eq!(definition().await.unwrap_err().status, Status::Stale);
        fs::write(&source, "value = 1\n").unwrap();
        assert_eq!(definition().await.unwrap().status, Status::Ok);

        let candidate = prepared.catalog.join("candidate.json");
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&candidate).unwrap()).unwrap();
        let published = Path::new(receipt["database"].as_str().unwrap());
        let saved_database = fs::read(published).unwrap();
        fs::set_permissions(published, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(published, vec![0; saved_database.len()]).unwrap();
        fs::set_permissions(published, fs::Permissions::from_mode(0o400)).unwrap();
        // The nonmatching candidate is present with a same-size invalid file.
        assert_eq!(definition().await.unwrap_err().status, Status::Unavailable);

        fs::write(&candidate, b"malformed receipt").unwrap();
        assert_eq!(definition().await.unwrap_err().status, Status::Unavailable);
    }
}
