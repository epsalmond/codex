//! Immutable operator registry. See `.codex/design/nas-lsp-broker.md`.

use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Read;
use std::io::{self};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct WorkspaceConfig {
    pub id: String,
    pub root: PathBuf,
    pub language: String,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub source_extensions: Vec<String>,
    pub exclude_dirs: Vec<String>,
    pub readiness: Readiness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    PyreflyLazyBlocking,
}

#[derive(Clone, Debug)]
pub struct BrokerConfig {
    pub socket_path: PathBuf,
    pub workspaces: Vec<Arc<WorkspaceConfig>>,
    pub limits: PoolLimits,
}

#[derive(Clone, Debug)]
pub struct PoolLimits {
    pub max_workers: usize,
    pub max_queue_per_worker: usize,
    pub max_queries: usize,
    pub max_connections: usize,
    pub idle_ttl: Duration,
    pub memory_max_bytes: u64,
    pub pids_max: u64,
}

impl Default for PoolLimits {
    fn default() -> Self {
        Self {
            max_workers: 2,
            max_queue_per_worker: 4,
            max_queries: 8,
            max_connections: 32,
            idle_ttl: Duration::from_secs(60),
            memory_max_bytes: 1024 * 1024 * 1024,
            pids_max: 64,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    socket_path: PathBuf,
    #[serde(default)]
    limits: LimitsWire,
    workspaces: Vec<WorkspaceWire>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LimitsWire {
    max_workers: usize,
    max_queue_per_worker: usize,
    max_queries: usize,
    max_connections: usize,
    idle_ttl_secs: u64,
    memory_max_bytes: u64,
    pids_max: u64,
}
impl Default for LimitsWire {
    fn default() -> Self {
        let l = PoolLimits::default();
        Self {
            max_workers: l.max_workers,
            max_queue_per_worker: l.max_queue_per_worker,
            max_queries: l.max_queries,
            max_connections: l.max_connections,
            idle_ttl_secs: l.idle_ttl.as_secs(),
            memory_max_bytes: l.memory_max_bytes,
            pids_max: l.pids_max,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceWire {
    id: String,
    root: PathBuf,
    language: String,
    executable: PathBuf,
    args: Vec<String>,
    #[serde(default = "default_extensions")]
    source_extensions: Vec<String>,
    #[serde(default = "default_excludes")]
    exclude_dirs: Vec<String>,
    readiness: String,
}
fn default_extensions() -> Vec<String> {
    vec!["py".into(), "pyi".into()]
}
fn default_excludes() -> Vec<String> {
    [".git", "target", "node_modules", ".venv"]
        .map(String::from)
        .to_vec()
}
fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

/// Only direct children of the sandbox-masked daemon directory are endpoints/configs.
pub fn validate_reserved_path(path: &Path) -> io::Result<()> {
    let directory = codex_uds::shared_daemon_socket_directory()?;
    if !path.is_absolute()
        || path.parent() != Some(directory.as_path())
        || !matches!(path.file_name(), Some(name) if !name.is_empty())
        || path
            .components()
            .any(|p| matches!(p, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("path must be inside reserved daemon directory"));
    }
    Ok(())
}

impl BrokerConfig {
    pub fn load(path: &Path) -> io::Result<Self> {
        validate_reserved_path(path)?;
        codex_uds::prepare_shared_daemon_socket_directory()?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        validate_registry_file(&file)?;
        let mut bytes = Vec::new();
        file.by_ref().take(65_537).read_to_end(&mut bytes)?;
        if bytes.len() > 65_536 {
            return Err(invalid("registry too large"));
        }
        let registry: Registry = toml::from_str(
            std::str::from_utf8(&bytes).map_err(|_| invalid("registry is not UTF-8"))?,
        )
        .map_err(|_| invalid("invalid registry schema"))?;
        let l = registry.limits;
        let workspaces = registry
            .workspaces
            .into_iter()
            .map(|w| {
                if w.readiness != "pyrefly_lazy_blocking" {
                    return Err(invalid("unsupported readiness"));
                }
                Ok(Arc::new(WorkspaceConfig {
                    id: w.id,
                    root: w.root,
                    language: w.language,
                    executable: w.executable,
                    args: w.args,
                    source_extensions: w.source_extensions,
                    exclude_dirs: w.exclude_dirs,
                    readiness: Readiness::PyreflyLazyBlocking,
                }))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let config = Self {
            socket_path: registry.socket_path,
            workspaces,
            limits: PoolLimits {
                max_workers: l.max_workers,
                max_queue_per_worker: l.max_queue_per_worker,
                max_queries: l.max_queries,
                max_connections: l.max_connections,
                idle_ttl: Duration::from_secs(l.idle_ttl_secs),
                memory_max_bytes: l.memory_max_bytes,
                pids_max: l.pids_max,
            },
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> io::Result<()> {
        validate_reserved_path(&self.socket_path)?;
        let l = &self.limits;
        if !(1..=32).contains(&l.max_workers)
            || l.max_queue_per_worker > 64
            || !(1..=256).contains(&l.max_queries)
            || !(1..=32).contains(&l.max_connections)
            || l.idle_ttl.is_zero()
            || l.idle_ttl > Duration::from_secs(86400)
            || l.memory_max_bytes == 0
            || l.pids_max == 0
        {
            return Err(invalid("invalid pool limits"));
        }
        if self.workspaces.is_empty() || self.workspaces.len() > 128 {
            return Err(invalid("invalid workspace count"));
        }
        let mut ids = HashSet::new();
        for (index, w) in self.workspaces.iter().enumerate() {
            if w.id.is_empty() || w.id.len() > 128 || !ids.insert(&w.id) {
                return Err(invalid("invalid workspace id"));
            }
            if !w.root.is_absolute() || fs::canonicalize(&w.root)? != w.root || !w.root.is_dir() {
                return Err(invalid("root must be canonical directory"));
            }
            if self.workspaces[..index]
                .iter()
                .any(|other| w.root.starts_with(&other.root) || other.root.starts_with(&w.root))
            {
                return Err(invalid("overlapping workspace roots"));
            }
            if w.language != "python"
                || w.args != ["lsp", "--indexing-mode", "lazy-blocking", "--threads", "1"]
            {
                return Err(invalid("unsupported language or server arguments"));
            }
            // A missing registered binary is an explicit query-time availability result.
            if !w.executable.is_absolute() {
                return Err(invalid("executable must be absolute"));
            }
            if let Ok(meta) = fs::metadata(&w.executable) {
                if !meta.is_file()
                    || meta.mode() & 0o111 == 0
                    || fs::canonicalize(&w.executable)? != w.executable
                {
                    return Err(invalid("executable must be canonical executable file"));
                }
            } else if w
                .executable
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            {
                return Err(invalid("invalid executable path"));
            }
            if w.source_extensions.is_empty()
                || w.source_extensions.iter().any(|s| s != "py" && s != "pyi")
                || w.exclude_dirs.len() > 64
                || w.exclude_dirs.iter().any(|s| {
                    s.is_empty() || s == ".." || s == "." || s.contains('/') || s.len() > 128
                })
            {
                return Err(invalid("invalid source policy"));
            }
        }
        Ok(())
    }
}
fn validate_registry_file(file: &File) -> io::Result<()> {
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "registry must be owner-owned regular file without group/other write",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RootIdentity {
    device: u64,
    inode: u64,
}
impl RootIdentity {
    pub(crate) fn read(root: &Path) -> io::Result<Self> {
        let m = fs::symlink_metadata(root)?;
        if !m.is_dir() || fs::canonicalize(root)? != root {
            return Err(invalid("root identity changed"));
        }
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
        })
    }
}
impl WorkspaceConfig {
    pub(crate) fn key(&self, identity: &RootIdentity) -> String {
        let mut hash = Sha256::new();
        // Length-delimited JSON prevents argument/policy concatenation collisions.
        hash.update(
            serde_json::to_vec(&(
                &self.root,
                identity.device,
                identity.inode,
                &self.language,
                &self.executable,
                &self.args,
                &self.source_extensions,
                &self.exclude_dirs,
                "pyrefly_lazy_blocking",
            ))
            .expect("configuration serializes"),
        );
        format!("{:x}", hash.finalize())
    }

    pub(crate) fn resolve_source(&self, path: &str) -> io::Result<PathBuf> {
        let relative = Path::new(path);
        if path.is_empty()
            || path.len() > 1024
            || relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(invalid("invalid source path"));
        }
        let canonical = fs::canonicalize(self.root.join(relative))?;
        if !canonical.starts_with(&self.root)
            || !canonical.is_file()
            || !canonical
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| self.source_extensions.iter().any(|s| s == ext))
        {
            return Err(invalid("source outside registered policy"));
        }
        // Symlinked files are unsupported even when their destination is local.
        let mut current = self.root.clone();
        for part in relative.components() {
            current.push(part);
            if fs::symlink_metadata(&current)?.file_type().is_symlink() {
                return Err(invalid("symlink source"));
            }
        }
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    #[test]
    fn registry_fd_rejects_nonregular_or_writable_files() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("registry");
        fs::write(&path, b"registry").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        assert!(validate_registry_file(&File::open(&path).expect("open")).is_ok());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o620)).expect("mode");
        assert!(validate_registry_file(&File::open(&path).expect("open")).is_err());
        assert!(validate_registry_file(&File::open(dir.path()).expect("open directory")).is_err());
    }
    #[test]
    fn registry_schema_load_is_private_immutable_and_rejects_unknown_fields() {
        let directory = codex_uds::prepare_shared_daemon_socket_directory().expect("directory");
        let root = tempfile::tempdir().expect("root");
        let mut registry = tempfile::NamedTempFile::new_in(&directory).expect("registry");
        let executable = fs::canonicalize("/bin/true").expect("executable");
        let text = format!(
            r#"socket_path = "{}/registry-schema-test.sock"
[[workspaces]]
id = "schema"
root = "{}"
language = "python"
executable = "{}"
args = ["lsp", "--indexing-mode", "lazy-blocking", "--threads", "1"]
readiness = "pyrefly_lazy_blocking"
"#,
            directory.display(),
            root.path().display(),
            executable.display()
        );
        use std::io::Write;
        registry.write_all(text.as_bytes()).expect("write registry");
        let config = BrokerConfig::load(registry.path()).expect("parse schema");
        assert_eq!(config.workspaces[0].source_extensions, vec!["py", "pyi"]);
        fs::write(registry.path(), "unexpected = true\n".to_owned() + &text)
            .expect("invalid schema");
        assert!(BrokerConfig::load(registry.path()).is_err());
        assert_eq!(
            config.workspaces[0].id, "schema",
            "already loaded config is immutable"
        );
        fs::write(registry.path(), &text).expect("restore registry");
        fs::set_permissions(registry.path(), fs::Permissions::from_mode(0o620))
            .expect("unsafe mode");
        assert!(BrokerConfig::load(registry.path()).is_err());
    }

    #[test]
    fn source_policy_rejects_absolute_traversal_and_symlink() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("a.py"), "value=1").expect("source");
        symlink(root.path().join("a.py"), root.path().join("alias.py")).expect("symlink");
        let workspace = WorkspaceConfig {
            id: "one".into(),
            root: root.path().to_owned(),
            language: "python".into(),
            executable: PathBuf::from("/unused"),
            args: Vec::new(),
            source_extensions: vec!["py".into()],
            exclude_dirs: Vec::new(),
            readiness: Readiness::PyreflyLazyBlocking,
        };
        assert!(workspace.resolve_source("a.py").is_ok());
        for path in ["/etc/passwd", "../a.py", "alias.py", "a.txt", ""] {
            assert!(workspace.resolve_source(path).is_err(), "{path}");
        }
    }
    #[test]
    fn workspace_key_includes_root_identity_and_argument_boundaries() {
        let root = tempfile::tempdir().expect("root");
        let workspace = WorkspaceConfig {
            id: "one".into(),
            root: root.path().to_owned(),
            language: "python".into(),
            executable: PathBuf::from("/unused"),
            args: vec!["ab".into(), "c".into()],
            source_extensions: vec!["py".into()],
            exclude_dirs: Vec::new(),
            readiness: Readiness::PyreflyLazyBlocking,
        };
        let identity = RootIdentity::read(root.path()).expect("identity");
        let mut changed = workspace.clone();
        changed.args = vec!["a".into(), "bc".into()];
        assert_ne!(workspace.key(&identity), changed.key(&identity));
        assert_ne!(
            workspace.key(&identity),
            workspace.key(&RootIdentity {
                device: identity.device,
                inode: identity.inode + 1
            })
        );
    }
}
