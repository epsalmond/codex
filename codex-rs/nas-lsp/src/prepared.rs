//! Select immutable artifacts published by a configured saved-index producer.

use crate::index::Manifest;
use serde::Deserialize;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

const MAX_RECEIPT: u64 = 32 * 1024;
const MAX_MANIFEST: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedIndex {
    pub catalog: PathBuf,
    /// Exact producer checkout recorded by the trusted publisher. It can move
    /// to a newer revision independently of its already published artifacts.
    pub publisher_checkout: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    state: String,
    workspace: String,
    checkout: PathBuf,
    git_common_dir: PathBuf,
    source_commit: String,
    manifest_fingerprint: String,
    manifest: PathBuf,
    database: PathBuf,
    database_sha256: String,
    database_size_bytes: u64,
    verified_documents: usize,
    manifest_tool_commit: String,
    manifest_tool_sha256: String,
    producer_sha256: String,
}

pub(crate) enum SelectionError {
    NotBuilt,
    NoMatch,
    Invalid,
}

pub(crate) struct SelectedDatabase {
    pub file: File,
    pub checkout: PathBuf,
}

impl PreparedIndex {
    pub(crate) fn validate(&self, checkout: &Path) -> io::Result<()> {
        if !self.publisher_checkout.is_absolute()
            || self.publisher_checkout == Path::new("/")
            || self
                .publisher_checkout
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            || self.catalog.starts_with(checkout)
            || self.catalog.starts_with(&self.publisher_checkout)
        {
            return Err(invalid());
        }
        private_directory(&self.catalog)
    }

    pub(crate) fn select(
        &self,
        workspace: &str,
        current: &Manifest,
        max_database: u64,
    ) -> Result<SelectedDatabase, SelectionError> {
        use SelectionError::*;

        private_directory(&self.catalog).map_err(|_| Invalid)?;
        let history = self.catalog.join("by-fingerprint");
        let historical = (|| {
            private_directory(&history)?;
            read_receipt(&history.join(format!("{}.json", current.fingerprint)))
        })();
        let (receipt, historical) = match historical {
            Ok(receipt) => (receipt, true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let receipt =
                    read_receipt(&self.catalog.join("candidate.json")).map_err(|error| {
                        if error.kind() == io::ErrorKind::NotFound {
                            NotBuilt
                        } else {
                            Invalid
                        }
                    })?;
                (receipt, false)
            }
            Err(_) => return Err(Invalid),
        };
        if receipt.schema_version != 1
            || receipt.state != "ready_for_broker"
            || receipt.workspace != workspace
            || receipt.checkout != self.publisher_checkout
            || receipt.git_common_dir != self.publisher_checkout.join(".git")
            || !commit(&receipt.source_commit)
            || !commit(&receipt.manifest_tool_commit)
            || !digest(&receipt.manifest_fingerprint)
            || !digest(&receipt.database_sha256)
            || !digest(&receipt.manifest_tool_sha256)
            || !digest(&receipt.producer_sha256)
            || receipt.verified_documents == 0
            || receipt.database_size_bytes == 0
            || receipt.database_size_bytes > max_database
        {
            return Err(Invalid);
        }
        if receipt.manifest_fingerprint != current.fingerprint {
            return Err(if historical { Invalid } else { NoMatch });
        }
        let generations = self.catalog.join("generations");
        let generation = generations.join(format!(
            "{}-{}-{}",
            receipt.manifest_fingerprint, receipt.producer_sha256, receipt.manifest_tool_sha256,
        ));
        if receipt.manifest != generation.join("manifest.json")
            || receipt.database != generation.join("index.sqlite3")
        {
            return Err(Invalid);
        }
        private_directory(&generations).map_err(|_| Invalid)?;
        private_directory(&generation).map_err(|_| Invalid)?;
        let (manifest_file, _) =
            private_file(&receipt.manifest, MAX_MANIFEST, true).map_err(|_| Invalid)?;
        let manifest: Manifest = read_json(manifest_file, MAX_MANIFEST).map_err(|_| Invalid)?;
        if manifest != *current || receipt.verified_documents > manifest.files.len() {
            return Err(Invalid);
        }
        let (database, size) =
            private_file(&receipt.database, max_database, true).map_err(|_| Invalid)?;
        if size != receipt.database_size_bytes {
            return Err(Invalid);
        }
        // The private publisher verifies and records the SQLite digest before
        // publication. The broker holds the selected immutable file open and
        // independently matches its generation row against the saved manifest.
        Ok(SelectedDatabase {
            file: database,
            checkout: receipt.checkout,
        })
    }
}

fn private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !path.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || fs::canonicalize(path)? != path
    {
        return Err(invalid());
    }
    Ok(())
}

fn private_file(path: &Path, max: u64, immutable: bool) -> io::Result<(File, u64)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || (immutable && metadata.mode() & 0o200 != 0)
        || metadata.len() > max
    {
        return Err(invalid());
    }
    Ok((file, metadata.len()))
}

fn read_receipt(path: &Path) -> io::Result<Receipt> {
    let (file, _) = private_file(path, MAX_RECEIPT, false)?;
    read_json(file, MAX_RECEIPT)
}

fn read_json<T: serde::de::DeserializeOwned>(file: File, max: u64) -> io::Result<T> {
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(invalid());
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn commit(value: &str) -> bool {
    (value.len() == 40 && hex(value)) || digest(value)
}

fn digest(value: &str) -> bool {
    value.len() == 64 && hex(value)
}

fn hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid prepared generation")
}
