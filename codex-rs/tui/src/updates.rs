#![cfg(not(debug_assertions))]

use crate::legacy_core::config::Config;
use crate::npm_registry;
use crate::npm_registry::NpmPackageInfo;
use crate::update_action;
use crate::update_action::UpdateAction;
use crate::update_versions::extract_version_from_latest_tag;
use crate::update_versions::is_newer;
use crate::update_versions::is_source_build_version;
use crate::updates_cache::VersionInfo;
use crate::updates_cache::fork_version_filepath;
use crate::updates_cache::read_version_info;
use crate::updates_cache::version_filepath;
use chrono::Duration;
use chrono::Utc;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::RouteAwareClientPool;
use codex_login::default_client::default_headers;
use serde::Deserialize;
use std::path::Path;
use std::path::PathBuf;

use crate::version::CODEX_CLI_VERSION;

fn version_filepath_for_install(config: &Config) -> PathBuf {
    if crate::fork_update::FORK_RELEASE_TAG.is_some() {
        fork_version_filepath(config, &crate::fork_update::fork_repository())
    } else {
        version_filepath(config)
    }
}

pub(crate) async fn dismiss_version(config: &Config, version: &str) -> anyhow::Result<()> {
    let version_file = version_filepath_for_install(config);
    crate::updates_cache::dismiss_version_at_path(&version_file, version).await
}

pub fn get_upgrade_version(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let action = update_action::get_update_action();
    let version_file = version_filepath_for_install(config);
    let info = read_version_info(&version_file).ok();

    let refresh_fork_source = crate::fork_update::FORK_RELEASE_TAG.is_some()
        && info
            .as_ref()
            .is_some_and(|info| !crate::fork_update::is_fork_release_tag(&info.latest_version));
    if refresh_fork_source
        || match &info {
            None => true,
            Some(info) => info.last_checked_at < Utc::now() - Duration::hours(20),
        }
    {
        let http_client_factory = config.http_client_factory();
        // Refresh the cached latest version in the background so TUI startup
        // isn’t blocked by a network call. The UI reads the previously cached
        // value (if any) for this run; the next run shows the banner if needed.
        tokio::spawn(async move {
            check_for_update(&version_file, action, http_client_factory)
                .await
                .inspect_err(|e| tracing::error!("Failed to update version: {e}"))
        });
    }

    info.and_then(|info| {
        let is_update = match crate::fork_update::FORK_RELEASE_TAG {
            Some(current_tag) => {
                crate::fork_update::is_fork_release_tag(&info.latest_version)
                    && crate::fork_update::fork_tag_is_newer(&info.latest_version, current_tag)
            }
            None => is_newer(&info.latest_version, CODEX_CLI_VERSION).unwrap_or(false),
        };
        if is_update {
            Some(info.latest_version)
        } else {
            None
        }
    })
}

// We use the latest version from the cask if installation is via homebrew - homebrew does not immediately pick up the latest release and can lag behind.
const HOMEBREW_CASK_API_URL: &str = "https://formulae.brew.sh/api/cask/codex.json";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";

#[derive(Deserialize, Debug, Clone)]
struct ReleaseInfo {
    tag_name: String,
}

#[derive(Deserialize, Debug, Clone)]
struct HomebrewCaskInfo {
    version: String,
}

async fn check_for_update(
    version_file: &Path,
    action: Option<UpdateAction>,
    http_client_factory: HttpClientFactory,
) -> anyhow::Result<()> {
    let client_pool = RouteAwareClientPool::with_chatgpt_cloudflare_cookies(
        http_client_factory,
        ClientRouteClass::Other,
    )
    .with_legacy_custom_ca_fallback();
    let latest_version = if crate::fork_update::FORK_RELEASE_TAG.is_some() {
        fetch_latest_fork_release_tag(&client_pool).await?
    } else {
        match action {
            Some(UpdateAction::Daemon(_)) => return Ok(()),
            Some(UpdateAction::BrewUpgrade) => {
                let HomebrewCaskInfo { version } = client_pool
                    .get(HOMEBREW_CASK_API_URL)
                    .headers(default_headers())
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<HomebrewCaskInfo>()
                    .await?;
                version
            }
            Some(UpdateAction::NpmGlobalLatest)
            | Some(UpdateAction::BunGlobalLatest)
            | Some(UpdateAction::VitePlusGlobalLatest)
            | Some(UpdateAction::PnpmGlobalLatest) => {
                let latest_version = fetch_latest_github_release_version(&client_pool).await?;
                let package_info = client_pool
                    .get(npm_registry::PACKAGE_URL)
                    .headers(default_headers())
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<NpmPackageInfo>()
                    .await?;
                npm_registry::ensure_version_ready(&package_info, &latest_version)?;
                latest_version
            }
            Some(UpdateAction::StandaloneUnix)
            | Some(UpdateAction::StandaloneWindows)
            | Some(UpdateAction::CodexShakeInstallScript)
            | Some(UpdateAction::CodexShakeBrewUpgrade)
            | Some(UpdateAction::CodexShakeDebManual)
            | None => fetch_latest_github_release_version(&client_pool).await?,
        }
    };

    // Preserve any previously dismissed version if present.
    let prev_info = read_version_info(version_file).ok();
    let info = VersionInfo {
        latest_version,
        last_checked_at: Utc::now(),
        dismissed_version: prev_info.and_then(|p| p.dismissed_version),
    };

    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

async fn fetch_latest_github_release_version(
    client_pool: &RouteAwareClientPool,
) -> anyhow::Result<String> {
    let ReleaseInfo {
        tag_name: latest_tag_name,
    } = client_pool
        .get(LATEST_RELEASE_URL)
        .headers(default_headers())
        .send()
        .await?
        .error_for_status()?
        .json::<ReleaseInfo>()
        .await?;
    extract_version_from_latest_tag(&latest_tag_name)
}

async fn fetch_latest_fork_release_tag(
    client_pool: &RouteAwareClientPool,
) -> anyhow::Result<String> {
    let body = client_pool
        .get(crate::fork_update::fork_releases_api_url())
        .headers(default_headers())
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let repository = crate::fork_update::fork_repository();
    crate::fork_update::newest_fork_release_tag(&body)
        .ok_or_else(|| anyhow::anyhow!("no local-features-v* release found in {repository}"))
}

/// Check the fork's release feed for a newer tag right now, bypassing the
/// version-cache throttle. Used by the `codex update` subcommand. Returns
/// `None` when this is not a fork build.
pub async fn check_fork_update_now(
    http_client_factory: HttpClientFactory,
) -> anyhow::Result<Option<String>> {
    if crate::fork_update::FORK_RELEASE_TAG.is_none() {
        return Ok(None);
    }
    let client_pool = RouteAwareClientPool::with_chatgpt_cloudflare_cookies(
        http_client_factory,
        ClientRouteClass::Other,
    )
    .with_legacy_custom_ca_fallback();
    Ok(Some(fetch_latest_fork_release_tag(&client_pool).await?))
}

/// The release tag this binary was built from, or `None` for a non-fork
/// (upstream) build.
pub fn fork_release_tag() -> Option<&'static str> {
    crate::fork_update::FORK_RELEASE_TAG
}

/// Whether `candidate` should be treated as a newer fork release than
/// `current`. See `fork_update::fork_tag_is_newer` for the ordering rules.
pub fn fork_tag_is_newer(candidate: &str, current: &str) -> bool {
    crate::fork_update::fork_tag_is_newer(candidate, current)
}

/// The command that reinstalls the latest fork release.
pub fn fork_install_command() -> String {
    crate::fork_update::fork_install_command()
}

/// Returns the latest version to show in a popup, if it should be shown.
/// This respects the user's dismissal choice for the current latest version.
pub fn get_upgrade_version_for_popup(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let version_file = version_filepath_for_install(config);
    let latest = get_upgrade_version(config)?;
    // If the user dismissed this exact version previously, do not show the popup.
    if let Ok(info) = read_version_info(&version_file)
        && info.dismissed_version.as_deref() == Some(latest.as_str())
    {
        return None;
    }
    Some(latest)
}
