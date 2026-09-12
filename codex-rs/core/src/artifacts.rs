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

use codex_utils_output_truncation::approx_bytes_for_tokens;
use codex_utils_output_truncation::approx_tokens_from_byte_count;
use uuid::Uuid;

const ARTIFACT_DIR_NAME: &str = "artifacts";

/// Cap on what a shake may put *into* one artifact. A region larger than this
/// is not savable, so shake leaves it in place rather than promising a recovery
/// it cannot deliver. Not to be confused with [`MAX_READ_BYTES`], which caps
/// what comes back *out* on one recovery read.
pub(crate) const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;

/// Ceiling on the bytes one recovery read returns, regardless of how much
/// context headroom there is. Bounds both the model-facing page and the memory
/// needed for a hostile artifact.
pub(crate) const MAX_READ_BYTES: usize = 3 * 1024;

/// Floor on a recovery page. A page smaller than this is not worth a round
/// trip, so a read with less headroom than this is refused with an actionable
/// error instead of returning a useless sliver.
pub(crate) const MIN_READ_BYTES: usize = 512;

/// Share of the remaining context window that one recovery read may consume.
/// Recovery is a step in a turn, not the whole turn: the model still needs room
/// for its own reasoning and output after reading, and for the next tool call.
const RECOVERY_HEADROOM_PERCENT: i64 = 50;

/// How many bytes one recovery read may return, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecoveryReadBudget {
    pub(crate) max_bytes: usize,
    /// The remaining-context token count, when headroom (rather than
    /// [`MAX_READ_BYTES`]) is what set `max_bytes`.
    pub(crate) clamped_by_remaining_tokens: Option<i64>,
}

/// Resolve the byte budget for one recovery read from the context headroom at
/// the time of the read.
///
/// This is the "readable-out" cap, the counterpart to the "savable-in"
/// [`MAX_ARTIFACT_BYTES`]. Without it a full-size recovery read can push
/// input + requested output past the model's context window at
/// request-construction time and be rejected before generation (oh-my-pi
/// #11365). `remaining_tokens` is `ContextWindowTokenStatus::
/// base_window_tokens_remaining`, i.e. measured against the resolved context
/// window; `None` means the model has no resolved window, in which case there
/// is no headroom to derive a bound from and the ceiling applies.
pub(crate) fn recovery_read_budget(
    remaining_tokens: Option<i64>,
) -> Result<RecoveryReadBudget, String> {
    let Some(remaining_tokens) = remaining_tokens else {
        return Ok(RecoveryReadBudget {
            max_bytes: MAX_READ_BYTES,
            clamped_by_remaining_tokens: None,
        });
    };
    let allowance_tokens = remaining_tokens
        .max(0)
        .saturating_mul(RECOVERY_HEADROOM_PERCENT)
        / 100;
    let allowance_bytes = approx_bytes_for_tokens(usize::try_from(allowance_tokens).unwrap_or(0));
    if allowance_bytes >= MAX_READ_BYTES {
        return Ok(RecoveryReadBudget {
            max_bytes: MAX_READ_BYTES,
            clamped_by_remaining_tokens: None,
        });
    }
    if allowance_bytes < MIN_READ_BYTES {
        return Err(format!(
            "not enough context left to recover an artifact: ~{remaining_tokens} tokens remain, \
             and one recovery page needs at least {MIN_READ_BYTES} bytes (~{min_tokens} tokens) \
             of headroom. Free context first (/shake, /compact, or a fresh thread), then read the \
             artifact again.",
            min_tokens = approx_tokens_from_byte_count(MIN_READ_BYTES),
        ));
    }
    Ok(RecoveryReadBudget {
        max_bytes: allowance_bytes,
        clamped_by_remaining_tokens: Some(remaining_tokens),
    })
}

impl RecoveryReadBudget {
    /// A notice explaining a headroom-clamped page, appended after the store's
    /// `[artifact source: ...]` marker (which already carries the `start_byte`
    /// to continue from). `None` when the ceiling, not headroom, set the bound —
    /// there is nothing context-specific to explain.
    pub(crate) fn truncation_notice(&self) -> Option<String> {
        let remaining_tokens = self.clamped_by_remaining_tokens?;
        Some(format!(
            "[artifact page bounded to {max_bytes} bytes (~{page_tokens} tokens) by the remaining \
             context window (~{remaining_tokens} tokens), below the usual {MAX_READ_BYTES}-byte \
             page. Continue from the start_byte above; free context first if you need the rest in \
             fewer reads.]",
            max_bytes = self.max_bytes,
            page_tokens = approx_tokens_from_byte_count(self.max_bytes),
        ))
    }
}

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

    /// Read one bounded page of an artifact.
    ///
    /// `max_bytes` is the caller's budget for this page — see
    /// [`recovery_read_budget`]. It is clamped into
    /// `[MIN_READ_BYTES, MAX_READ_BYTES]` here so no caller can ask for an
    /// unbounded page.
    pub(crate) fn read(
        &self,
        uri: &str,
        start_byte: Option<u64>,
        max_bytes: usize,
    ) -> Result<String, String> {
        let max_bytes = max_bytes.clamp(MIN_READ_BYTES, MAX_READ_BYTES);
        let id = parse_uri(uri)?;
        let path = self.resolve(id)?;
        let offset = start_byte.unwrap_or(/*default*/ 0);
        let mut file = File::open(path).map_err(|err| format!("failed to open {uri}: {err}"))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|err| format!("failed to seek {uri}: {err}"))?;

        // Read only one small page plus a marker byte. This bounds both the
        // model-facing result and the memory needed for a hostile artifact.
        let mut bytes = Vec::with_capacity(max_bytes + 1);
        file.take((max_bytes + 1) as u64)
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

        let mut output_bytes = bytes.len().min(max_bytes);
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
