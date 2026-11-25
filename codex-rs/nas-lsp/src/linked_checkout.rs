//! Resolve linked checkouts of an explicitly enrolled Git repository.

use crate::config::RootIdentity;
use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

const MAX_METADATA_BYTES: u64 = 8 * 1024;
const MAX_CWD_ANCESTORS: usize = 128;

#[derive(Clone)]
pub(crate) struct CheckoutAnchor {
    root: PathBuf,
    root_identity: RootIdentity,
    common_directory: PathBuf,
    common_identity: RootIdentity,
}

impl CheckoutAnchor {
    pub(crate) fn new(root: &Path) -> io::Result<Self> {
        let common_directory = root.join(".git");
        Ok(Self {
            root: root.to_path_buf(),
            root_identity: owned_directory(root)?,
            common_identity: owned_directory(&common_directory)?,
            common_directory,
        })
    }

    /// The selected root must be the nearest Git boundary to the request cwd.
    pub(crate) fn resolve(&self, checkout: &Path) -> io::Result<RootIdentity> {
        if owned_directory(&self.root)? != self.root_identity
            || owned_directory(&self.common_directory)? != self.common_identity
        {
            return Err(invalid());
        }
        if checkout == self.root {
            return Ok(self.root_identity.clone());
        }
        let checkout_identity = owned_directory(checkout)?;
        let pointer = checkout.join(".git");
        let gitdir_contents = read_metadata(&pointer)?;
        let gitdir = gitdir_contents
            .strip_prefix(b"gitdir: ")
            .ok_or_else(invalid)?;
        let administration = canonical_target(&pointer, gitdir)?;
        let worktrees = self.common_directory.join("worktrees");
        if administration.parent() != Some(worktrees.as_path()) {
            return Err(invalid());
        }
        owned_directory(&worktrees)?;
        owned_directory(&administration)?;
        let anchor = administration.join("commondir");
        let reverse = administration.join("gitdir");
        if canonical_target(&anchor, &read_metadata(&anchor)?)? != self.common_directory
            || canonical_target(&reverse, &read_metadata(&reverse)?)? != pointer
        {
            return Err(invalid());
        }
        Ok(checkout_identity)
    }
}

pub(crate) fn nearest_checkout(cwd: &Path) -> Option<&Path> {
    if !cwd.is_absolute()
        || cwd
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    for ancestor in cwd.ancestors().take(MAX_CWD_ANCESTORS) {
        match fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => return Some(ancestor),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    None
}

fn owned_directory(path: &Path) -> io::Result<RootIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || !owned(&metadata) {
        return Err(invalid());
    }
    RootIdentity::read(path)
}

fn owned(metadata: &fs::Metadata) -> bool {
    metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o002 == 0
        // Ordinary Git worktrees can inherit group write under umask 002.
        // Only the operator's own effective group is trusted in that case.
        && (metadata.mode() & 0o020 == 0 || metadata.gid() == unsafe { libc::getegid() })
}

fn read_metadata(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || !owned(&metadata) {
        return Err(invalid());
    }
    let mut content = Vec::new();
    file.take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut content)?;
    if content.len() as u64 > MAX_METADATA_BYTES || content.pop() != Some(b'\n') {
        return Err(invalid());
    }
    Ok(content)
}

fn canonical_target(pointer: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    if bytes.is_empty() || bytes.contains(&b'\n') || bytes.contains(&0) {
        return Err(invalid());
    }
    let target = Path::new(OsStr::from_bytes(bytes));
    if target.is_absolute() {
        fs::canonicalize(target)
    } else {
        fs::canonicalize(pointer.parent().ok_or_else(invalid)?.join(target))
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "unrecognized linked checkout")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&OsStr]) {
        let result = Command::new("git")
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()
            .unwrap();
        assert!(result.status.success(), "{:?}", result);
    }

    pub(crate) fn make_linked_checkout(directory: &Path, nested: bool) -> (PathBuf, PathBuf) {
        let repository = directory.join("repository");
        let linked = if nested {
            repository.join(".worktrees/feature")
        } else {
            directory.join("elsewhere")
        };
        fs::create_dir(&repository).unwrap();
        git(&repository, &[OsStr::new("init")]);
        git(
            &repository,
            &[
                OsStr::new("-c"),
                OsStr::new("user.name=Test"),
                OsStr::new("-c"),
                OsStr::new("user.email=test@example.invalid"),
                OsStr::new("commit"),
                OsStr::new("--allow-empty"),
                OsStr::new("-m"),
                OsStr::new("initial"),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                linked.as_os_str(),
            ],
        );
        (repository, linked)
    }

    #[test]
    fn linked_checkout_requires_a_bidirectional_link_to_the_enrolled_repository() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, linked) = make_linked_checkout(directory.path(), false);
        let fake = directory.path().join("unregistered");
        let anchor = CheckoutAnchor::new(&repository).unwrap();
        let subdir = linked.join("apps/studio");
        fs::create_dir_all(&subdir).unwrap();
        assert_eq!(nearest_checkout(&subdir), Some(linked.as_path()));
        assert!(anchor.resolve(&repository).is_ok());
        assert!(anchor.resolve(&linked).is_ok());

        fs::create_dir(&fake).unwrap();
        fs::copy(linked.join(".git"), fake.join(".git")).unwrap();
        assert!(anchor.resolve(&fake).is_err());

        fs::create_dir(subdir.join(".git")).unwrap();
        assert_nearest_is_unregistered(&anchor, &subdir);
        fs::set_permissions(linked.join(".git"), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(anchor.resolve(&linked).is_err());
    }

    fn assert_nearest_is_unregistered(anchor: &CheckoutAnchor, cwd: &Path) {
        assert_eq!(nearest_checkout(cwd), Some(cwd));
        assert!(anchor.resolve(cwd).is_err());
    }
}
