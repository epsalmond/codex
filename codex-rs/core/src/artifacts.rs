//! Durable, per-thread text artifacts used for recoverable history elision.
//!
//! There is no tool that reads an artifact back: a census of 346 local
//! oh-my-pi sessions and 9 shake-bench runs found zero read-backs, and every
//! sandbox mode already grants the model read access to the artifact
//! directory via the shell (see `docs/shake.md`). Shake only ever needs to
//! `save` a region and name the file it landed in.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use uuid::Uuid;

const ARTIFACT_DIR_NAME: &str = "artifacts";

/// Cap on what a shake may put *into* one artifact. A region larger than this
/// is not savable, so shake leaves it in place rather than promising a recovery
/// it cannot deliver.
pub(crate) const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// Resolves the existing `CODEX_HOME` once at construction without creating
    /// the artifact directory, so previewing a Shake remains read-only. Every
    /// placeholder path is absolute when `CODEX_HOME` is an existing relative
    /// path; artifact directories are created only by `save` or `copy_from`.
    pub(crate) fn for_thread(codex_home: &Path, thread_id: impl std::fmt::Display) -> Self {
        let thread_id = thread_id.to_string();
        let root = codex_home.join(ARTIFACT_DIR_NAME).join(&thread_id);
        let root = fs::canonicalize(codex_home)
            .map(|home| home.join(ARTIFACT_DIR_NAME).join(thread_id))
            .unwrap_or(root);
        Self { root }
    }

    /// The on-disk path an artifact with `id` and `label` is (or would be)
    /// saved at. Pure path arithmetic — does not touch the filesystem, so it
    /// can also be used to compute a representative path for a shake preview
    /// that never writes anything.
    pub(crate) fn destination_path(&self, id: &str, label: &str) -> PathBuf {
        self.root
            .join(format!("{id}.{}.log", sanitize_label(label)))
    }

    pub(crate) fn save(&self, content: &str, label: &str) -> io::Result<String> {
        if content.len() as u64 > MAX_ARTIFACT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("artifact is larger than {MAX_ARTIFACT_BYTES} bytes"),
            ));
        }

        fs::create_dir_all(&self.root)?;
        let id = Uuid::new_v4().simple().to_string();
        let destination = self.destination_path(&id, label);
        let temporary = self.root.join(format!(".tmp-{id}.log"));
        let result = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            if file.metadata()?.len() != content.len() as u64 {
                return Err(io::Error::other("artifact size verification failed"));
            }
            fs::rename(&temporary, &destination)?;
            Ok(format!("artifact://{id}"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(crate) fn copy_from(&self, source: &Self) -> io::Result<()> {
        if !source.root.try_exists()? {
            return Ok(());
        }
        fs::create_dir_all(&self.root)?;
        for entry in fs::read_dir(&source.root)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".tmp-") {
                continue;
            }
            let destination = self.root.join(&name);
            if destination.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "artifact destination already exists: {}",
                        destination.display()
                    ),
                ));
            }
            let temporary = self.root.join(format!(".tmp-copy-{}", Uuid::new_v4()));
            fs::copy(entry.path(), &temporary)?;
            OpenOptions::new()
                .write(true)
                .open(&temporary)?
                .sync_all()?;
            fs::rename(temporary, destination)?;
        }
        Ok(())
    }
}

fn sanitize_label(label: &str) -> String {
    let mut sanitized = String::new();
    for character in label.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            sanitized.push(character);
        } else if sanitized.as_bytes().last() != Some(&b'_') {
            sanitized.push('_');
        }
        if sanitized.len() == 64 {
            break;
        }
    }
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "artifact".to_string()
    } else {
        sanitized.to_string()
    }
}

#[cfg(test)]
#[path = "artifacts_tests.rs"]
mod tests;
