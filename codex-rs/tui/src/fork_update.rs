//! Build-time identity and update-source logic for Eric's `codex-shake` fork.
//!
//! When `CODEX_FORK_RELEASE_TAG` is not set at build time (the upstream
//! case), `FORK_RELEASE_TAG` is `None` and none of the fork-specific update
//! logic elsewhere in this crate (`updates.rs`, `update_action.rs`,
//! `update_prompt.rs`) takes effect: the binary behaves exactly like
//! upstream Codex.
//!
//! `.github/workflows/fork-release.yml` sets `CODEX_FORK_RELEASE_TAG` to the
//! release tag (`github.ref_name`) in the cargo build step that produces a
//! release binary; `~/.bin/codex-fork-release` does the same for local
//! builds.

#[cfg(any(not(debug_assertions), test))]
use serde::Deserialize;

/// Command that reinstalls the latest fork release, exactly as described by
/// `install.sh` at the repo root. Always compiled: `update_action.rs` refers
/// to it unconditionally (`UpdateAction` itself is not restricted to release
/// builds).
pub(crate) const FORK_INSTALL_COMMAND: &str = "curl -fsSL https://raw.githubusercontent.com/epsalmond/codex/eric/local-features/install.sh | bash";

/// The release tag this binary was built from. `None` for upstream/non-fork
/// builds. Only read from `updates.rs` / `update_prompt.rs`, which are
/// restricted to release builds, so under a plain `cfg(test)` (debug)
/// build nothing outside this module's own tests consumes it — hence the
/// `allow(dead_code)` on the `test` half of the cfg.
#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(all(debug_assertions, test), allow(dead_code))]
pub(crate) const FORK_RELEASE_TAG: Option<&str> = option_env!("CODEX_FORK_RELEASE_TAG");

/// The fork's own GitHub releases feed, queried instead of upstream's when
/// `FORK_RELEASE_TAG` is present.
#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(all(debug_assertions, test), allow(dead_code))]
pub(crate) const FORK_RELEASES_API_URL: &str =
    "https://api.github.com/repos/epsalmond/codex/releases?per_page=5";

/// Where fork users should look for release notes.
#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(all(debug_assertions, test), allow(dead_code))]
pub(crate) const FORK_RELEASE_NOTES_URL: &str = "https://github.com/epsalmond/codex/releases";

/// Tags cut by `fork-release.yml` all start with this prefix.
#[cfg(any(not(debug_assertions), test))]
const FORK_TAG_PREFIX: &str = "local-features-v";

#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(all(debug_assertions, test), allow(dead_code))]
pub(crate) fn is_fork_build() -> bool {
    FORK_RELEASE_TAG.is_some()
}

#[cfg(any(not(debug_assertions), test))]
#[derive(Deserialize, Debug, Clone)]
struct ForkReleaseEntry {
    tag_name: String,
    #[serde(default)]
    draft: bool,
}

/// Parse the JSON body of `GET /repos/epsalmond/codex/releases` and return
/// the tag of the newest non-draft release whose tag starts with
/// `local-features-v`. GitHub returns releases newest-created-first, so the
/// first matching entry is the newest.
#[cfg(any(not(debug_assertions), test))]
pub(crate) fn newest_fork_release_tag(releases_json: &str) -> Option<String> {
    let entries: Vec<ForkReleaseEntry> = serde_json::from_str(releases_json).ok()?;
    entries
        .into_iter()
        .find(|entry| !entry.draft && entry.tag_name.starts_with(FORK_TAG_PREFIX))
        .map(|entry| entry.tag_name)
}

/// Extract the `-r<digits>` release-counter component from a fork tag, e.g.
/// `local-features-v0.155.0-r202609131200.abcdef1` -> `202609131200`.
#[cfg(any(not(debug_assertions), test))]
fn release_counter(tag: &str) -> Option<u64> {
    let idx = tag.rfind("-r")?;
    let digits: String = tag[idx + 2..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Returns whether `candidate` should be treated as newer than `current`.
///
/// Current-scheme tags both carry a `-r<UTC-yyyymmddHHMM>` release counter;
/// compare those digits numerically. Releases cut before the counter existed
/// used a bare commit-sha suffix instead (`local-features-v0.154.0-<12
/// hex>`); since those predate any ordering scheme, treat any different tag
/// as newer than one of those.
#[cfg(any(not(debug_assertions), test))]
pub(crate) fn fork_tag_is_newer(candidate: &str, current: &str) -> bool {
    if candidate == current {
        return false;
    }
    match (release_counter(candidate), release_counter(current)) {
        (Some(candidate_counter), Some(current_counter)) => candidate_counter > current_counter,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const SAMPLE_RELEASES_JSON: &str = r#"[
        {"tag_name": "local-features-v0.155.0-r202609131200.abcdef1", "draft": false},
        {"tag_name": "local-features-v0.154.0-r202609010800.9876543", "draft": false},
        {"tag_name": "local-features-v0.154.0-1122334455aa", "draft": false},
        {"tag_name": "v0.155.0", "draft": false},
        {"tag_name": "local-features-v0.156.0-r202609140000.deadbee", "draft": true}
    ]"#;

    #[test]
    fn picks_newest_non_draft_fork_release() {
        assert_eq!(
            newest_fork_release_tag(SAMPLE_RELEASES_JSON).as_deref(),
            Some("local-features-v0.155.0-r202609131200.abcdef1")
        );
    }

    #[test]
    fn ignores_non_fork_and_draft_entries() {
        let json = r#"[
            {"tag_name": "v0.155.0", "draft": false},
            {"tag_name": "local-features-v0.150.0-r1.abc", "draft": true}
        ]"#;
        assert_eq!(newest_fork_release_tag(json), None);
    }

    #[test]
    fn empty_or_malformed_json_yields_none() {
        assert_eq!(newest_fork_release_tag("not json"), None);
        assert_eq!(newest_fork_release_tag("[]"), None);
    }

    #[test]
    fn release_counter_orders_newer_tag_as_newer() {
        assert!(fork_tag_is_newer(
            "local-features-v0.155.0-r202609131200.abcdef1",
            "local-features-v0.154.0-r202609010800.9876543",
        ));
        assert!(!fork_tag_is_newer(
            "local-features-v0.154.0-r202609010800.9876543",
            "local-features-v0.155.0-r202609131200.abcdef1",
        ));
    }

    #[test]
    fn identical_tags_are_not_newer() {
        assert!(!fork_tag_is_newer(
            "local-features-v0.155.0-r202609131200.abcdef1",
            "local-features-v0.155.0-r202609131200.abcdef1",
        ));
    }

    #[test]
    fn missing_release_counter_is_treated_as_newer_either_direction() {
        // Old hex-suffixed tags predate the -r<digits> scheme, so we cannot
        // order against them numerically; any difference counts as an update.
        assert!(fork_tag_is_newer(
            "local-features-v0.155.0-r202609131200.abcdef1",
            "local-features-v0.154.0-1122334455aa",
        ));
        assert!(fork_tag_is_newer(
            "local-features-v0.154.0-1122334455aa",
            "local-features-v0.155.0-r202609131200.abcdef1",
        ));
    }
}
