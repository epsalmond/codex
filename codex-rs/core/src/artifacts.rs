//! Durable, per-thread text artifacts used for recoverable history elision.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use uuid::Uuid;

const ARTIFACT_DIR_NAME: &str = "artifacts";
const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_READ_BYTES: usize = 3 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub(crate) fn for_thread(codex_home: &Path, thread_id: impl std::fmt::Display) -> Self {
        Self {
            root: codex_home
                .join(ARTIFACT_DIR_NAME)
                .join(thread_id.to_string()),
        }
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
        let destination = self
            .root
            .join(format!("{id}.{}.log", sanitize_label(label)));
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

    pub(crate) fn read(&self, uri: &str, start_byte: Option<u64>) -> Result<String, String> {
        let id = parse_uri(uri)?;
        let path = self.resolve(id)?;
        let offset = start_byte.unwrap_or(/*default*/ 0);
        let mut file = File::open(path).map_err(|err| format!("failed to open {uri}: {err}"))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|err| format!("failed to seek {uri}: {err}"))?;

        // Read only one small page plus a marker byte. This bounds both the
        // model-facing result and the memory needed for a hostile artifact.
        let mut bytes = Vec::with_capacity(MAX_READ_BYTES + 1);
        file.take((MAX_READ_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|err| format!("failed to read {uri}: {err}"))?;
        if bytes.is_empty() {
            return Err(format!("artifact {uri} has no content at byte {offset}"));
        }
        if offset > 0 && (bytes[0] & 0b1100_0000) == 0b1000_0000 {
            return Err(format!(
                "artifact {uri} start_byte {offset} is not a UTF-8 character boundary"
            ));
        }

        let mut output_bytes = bytes.len().min(MAX_READ_BYTES);
        while output_bytes > 0
            && output_bytes < bytes.len()
            && (bytes[output_bytes] & 0b1100_0000) == 0b1000_0000
        {
            output_bytes -= 1;
        }
        let output = String::from_utf8(bytes[..output_bytes].to_vec())
            .map_err(|_| format!("artifact {uri} contains invalid UTF-8 text"))?;
        if output.is_empty() {
            return Err(format!(
                "artifact {uri} has no UTF-8 content at byte {offset}"
            ));
        }

        if output_bytes < bytes.len() {
            let next_offset = offset + output_bytes as u64;
            Ok(format!(
                "{output}\n[artifact source: {uri}; more content; use start_byte={next_offset}]"
            ))
        } else {
            Ok(format!("{output}\n[artifact source: {uri}; final page]"))
        }
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

    fn resolve(&self, id: &str) -> Result<PathBuf, String> {
        let entries = fs::read_dir(&self.root)
            .map_err(|err| format!("artifact {id} is unavailable: {err}"))?;
        let prefix = format!("{id}.");
        for entry in entries {
            let entry = entry.map_err(|err| format!("failed to inspect artifact {id}: {err}"))?;
            let file_type = entry
                .file_type()
                .map_err(|err| format!("failed to inspect artifact {id}: {err}"))?;
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(&prefix) {
                return Ok(entry.path());
            }
        }
        Err(format!("artifact://{id} was not found in this thread"))
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

fn parse_uri(uri: &str) -> Result<&str, String> {
    let Some(id) = uri.strip_prefix("artifact://") else {
        return Err(format!("expected an artifact:// URI, got {uri}"));
    };
    if id.is_empty()
        || id.contains('/')
        || id.contains('\\')
        || id.contains("..")
        || Uuid::parse_str(id).is_err()
    {
        return Err(format!("invalid artifact URI: {uri}"));
    }
    Ok(id)
}

#[cfg(test)]
#[path = "artifacts_tests.rs"]
mod tests;
