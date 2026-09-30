//! codex-shake fork policy: the shared background server follows the CLI.
//!
//! Fork releases reuse upstream version numbers, so the version-named daemon
//! install cannot tell two codex-shake releases apart, and the upstream
//! auto-updater is disabled (see `settings`). Before a start, a packaged fork
//! CLI checks which release the selected daemon package runs; when it is not
//! this CLI's release (an older codex-shake release, or an upstream build left
//! by an earlier auto-update), it selects this CLI's package instead.

use std::path::Path;

use codex_install_context::InstallContext;
use tokio::process::Command;

use crate::Daemon;

const FORK_RELEASE_TAG: Option<&str> = option_env!("CODEX_FORK_RELEASE_TAG");

/// Replace the selected daemon package with this CLI's package when it runs a
/// different codex-shake release. A running daemon is restarted on the new
/// package. Callers must not hold the daemon operation lock.
pub(crate) async fn follow_cli_package(daemon: &Daemon) {
    let Some(release_tag) = FORK_RELEASE_TAG else {
        return;
    };
    if InstallContext::current().package_layout.is_none() {
        return;
    }
    let Ok(managed_codex_bin) = daemon.current_managed_codex_bin() else {
        return;
    };
    // A missing package is seeded from this CLI by the normal start path.
    if !managed_codex_bin.is_file() || runs_release(&managed_codex_bin, release_tag).await {
        return;
    }
    daemon.diagnostic(format_args!(
        "Switching the background server to codex-shake {release_tag}..."
    ));
    if let Err(err) = crate::prepare_install::update_from_cli(|_| Ok(true)).await {
        daemon.diagnostic(format_args!(
            "warning: failed to switch the background server to this codex-shake release: {err:#}"
        ));
    }
}

async fn runs_release(codex_bin: &Path, release_tag: &str) -> bool {
    let Ok(output) = Command::new(codex_bin)
        .arg("--version")
        .kill_on_drop(true)
        .output()
        .await
    else {
        return false;
    };
    let expected = format!("Release tag: {release_tag}");
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line == expected)
}
