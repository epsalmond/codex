use crate::legacy_core::config::Config;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct VersionInfo {
    pub(crate) latest_version: String,
    // ISO-8601 timestamp (RFC3339)
    pub(crate) last_checked_at: DateTime<Utc>,
    #[serde(default)]
    pub(crate) dismissed_version: Option<String>,
}

const VERSION_FILENAME: &str = "version.json";
const FORK_VERSION_FILENAME_PREFIX: &str = "version-fork-";

pub(crate) fn version_filepath(config: &Config) -> PathBuf {
    config.codex_home.join(VERSION_FILENAME).into_path_buf()
}

/// Return a cache path isolated from the upstream updater and other fork
/// repositories. Percent encoding keeps repository names safe as filenames,
/// including when an installer override contains shell metacharacters.
pub(crate) fn fork_version_filepath(config: &Config, repository: &str) -> PathBuf {
    let repository = urlencoding::encode(repository);
    config
        .codex_home
        .join(format!("{FORK_VERSION_FILENAME_PREFIX}{repository}.json"))
        .into_path_buf()
}

pub(crate) fn read_version_info(version_file: &Path) -> anyhow::Result<VersionInfo> {
    let contents = std::fs::read_to_string(version_file)?;
    Ok(serde_json::from_str(&contents)?)
}

/// Persist a dismissal for the current latest version so we don't show
/// the update popup again for this version.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn dismiss_version(config: &Config, version: &str) -> anyhow::Result<()> {
    let version_file = version_filepath(config);
    dismiss_version_at_path(&version_file, version).await
}

pub(crate) async fn dismiss_version_at_path(
    version_file: &Path,
    version: &str,
) -> anyhow::Result<()> {
    let mut info = match read_version_info(version_file) {
        Ok(info) => info,
        Err(_) => VersionInfo {
            latest_version: version.to_string(),
            last_checked_at: DateTime::<Utc>::UNIX_EPOCH,
            dismissed_version: None,
        },
    };
    info.dismissed_version = Some(version.to_string());
    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

#[cfg(test)]
#[path = "updates_cache_tests.rs"]
mod tests;
