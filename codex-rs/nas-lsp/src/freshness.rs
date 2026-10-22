//! Bounded saved-file generations. See `.codex/design/nas-lsp-broker.md`.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::{Instant, timeout_at};

const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_FILES: usize = 2000;
const MAX_ENTRIES: usize = 10000;

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub files: BTreeMap<PathBuf, Arc<Vec<u8>>>,
    digest: [u8; 32],
}

impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
    }
}

/// The walk holds directory descriptors and uses O_NOFOLLOW for every child.
/// A concurrently replaced directory can never redirect this walk outside root.
fn inventory(
    root: &Path,
    _excludes: &[String],
    deadline: Instant,
) -> Result<Snapshot, &'static str> {
    let root_file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)
        .map_err(|_| "workspace_unreadable")?;
    let identity = root_file.metadata().map_err(|_| "workspace_unreadable")?;
    let mut walk = Inventory {
        files: BTreeMap::new(),
        entries: 0,
        bytes: 0,
        deadline,
    };
    walk.directory(&root_file, Path::new(""))?;
    let current = fs::symlink_metadata(root).map_err(|_| "workspace_unreadable")?;
    if current.file_type().is_symlink()
        || current.dev() != identity.dev()
        || current.ino() != identity.ino()
    {
        return Err("workspace_replaced");
    }
    let mut hash = Sha256::new();
    hash.update(identity.dev().to_le_bytes());
    hash.update(identity.ino().to_le_bytes());
    for (path, content) in &walk.files {
        hash.update((path.as_os_str().as_bytes().len() as u64).to_le_bytes());
        hash.update(path.as_os_str().as_bytes());
        hash.update((content.len() as u64).to_le_bytes());
        hash.update(content.as_slice());
    }
    Ok(Snapshot {
        files: walk.files,
        digest: hash.finalize().into(),
    })
}

struct Inventory {
    files: BTreeMap<PathBuf, Arc<Vec<u8>>>,
    entries: usize,
    bytes: usize,
    deadline: Instant,
}

impl Inventory {
    fn directory(&mut self, directory: &File, relative: &Path) -> Result<(), &'static str> {
        if relative.components().count() > 128 {
            return Err("workspace_too_large");
        }
        let entries = fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))
            .map_err(|_| "workspace_unreadable")?;
        for entry in entries {
            if Instant::now() >= self.deadline {
                return Err("inventory_timeout");
            }
            self.entries += 1;
            if self.entries > MAX_ENTRIES {
                return Err("workspace_too_large");
            }
            let entry = entry.map_err(|_| "workspace_unreadable")?;
            let name = entry.file_name();
            let name_c = CString::new(name.as_bytes()).map_err(|_| "workspace_unreadable")?;
            // O_NONBLOCK prevents a malicious/replaced FIFO from blocking this task.
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ELOOP) {
                        "workspace_symlink"
                    } else {
                        "workspace_unreadable"
                    },
                );
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata().map_err(|_| "workspace_unreadable")?;
            // Linked worktrees use a regular .git pointer instead of a directory.
            if name == ".git" {
                continue;
            }
            if metadata.is_dir() {
                // Directory exclusion hints cannot omit semantic inputs: the
                // server may import their source/configuration regardless of
                // those hints. Only Git metadata is outside the saved generation.
                self.directory(&file, &relative.join(&name))?;
            } else if metadata.is_file() {
                if self.files.len() >= MAX_FILES || metadata.len() > MAX_FILE {
                    return Err("workspace_too_large");
                }
                let mut contents = Vec::new();
                file.by_ref()
                    .take(MAX_FILE + 1)
                    .read_to_end(&mut contents)
                    .map_err(|_| "workspace_unreadable")?;
                self.bytes += contents.len();
                if contents.len() as u64 > MAX_FILE || self.bytes > MAX_BYTES {
                    return Err("workspace_too_large");
                }
                self.files.insert(relative.join(name), Arc::new(contents));
            } else {
                return Err("workspace_special_file");
            }
        }
        Ok(())
    }
}

pub(crate) async fn snapshot(
    root: PathBuf,
    excludes: Vec<String>,
    deadline: Instant,
) -> Result<Snapshot, &'static str> {
    timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || inventory(&root, &excludes, deadline)),
    )
    .await
    .map_err(|_| "inventory_timeout")?
    .map_err(|_| "workspace_unreadable")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn content_configs_additions_deletions_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.pyi"), "x: int\n").unwrap();
        let scan = || {
            snapshot(
                dir.path().to_path_buf(),
                vec![],
                Instant::now() + Duration::from_secs(2),
            )
        };
        let first = scan().await.unwrap();
        assert!(first == scan().await.unwrap());
        fs::write(dir.path().join("a.pyi"), "y: int\n").unwrap();
        assert!(first != scan().await.unwrap());
        let second = scan().await.unwrap();
        fs::write(dir.path().join("pyproject.toml"), "[tool.pyrefly]\n").unwrap();
        assert!(second != scan().await.unwrap());
        fs::remove_file(dir.path().join("a.pyi")).unwrap();
        assert!(!scan().await.unwrap().files.contains_key(Path::new("a.pyi")));
        std::os::unix::fs::symlink("/tmp", dir.path().join("link")).unwrap();
        assert!(matches!(scan().await, Err("workspace_symlink")));
    }

    #[tokio::test]
    async fn oversized_and_special_files_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("large"))
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        assert!(matches!(
            snapshot(
                dir.path().to_path_buf(),
                vec![],
                Instant::now() + std::time::Duration::from_secs(2)
            )
            .await,
            Err("workspace_too_large")
        ));
    }

    #[tokio::test]
    async fn exclusion_hints_cannot_hide_mutable_semantic_inputs_or_limits() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["hidden", "target", "node_modules", ".venv"] {
            fs::create_dir(dir.path().join(name)).unwrap();
            fs::write(dir.path().join(name).join("source.py"), "x = 1\n").unwrap();
        }
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/metadata"), "initial").unwrap();
        let scan = || {
            snapshot(
                dir.path().to_path_buf(),
                vec!["hidden".into()],
                Instant::now() + Duration::from_secs(2),
            )
        };
        let first = scan().await.unwrap();
        assert_eq!(first.files.len(), 4);
        fs::write(dir.path().join(".git/metadata"), "changed").unwrap();
        assert!(first == scan().await.unwrap());
        fs::write(dir.path().join("hidden/source.py"), "x = 2\n").unwrap();
        assert!(first != scan().await.unwrap());
        File::create(dir.path().join("target/large"))
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        assert!(matches!(scan().await, Err("workspace_too_large")));
    }
}
