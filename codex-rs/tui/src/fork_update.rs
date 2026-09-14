//! Build-time identity and update-source logic for Eric's `codex-shake` fork.
//!
//! When `CODEX_FORK_RELEASE_TAG` is not set at build time (the upstream
//! case), `FORK_RELEASE_TAG` is `None` and none of the fork-specific update
//! logic elsewhere in this crate (`updates.rs`, `update_action.rs`,
//! `update_prompt.rs`) takes effect: the binary behaves exactly like
//! upstream Codex.
//!
//! `.github/workflows/fork-release.yml` sets `CODEX_FORK_RELEASE_TAG` to the
//! prepared release tag in the cargo build step that produces a release
//! binary; `~/.bin/codex-fork-release` does the same for local builds.

#[cfg(any(not(debug_assertions), test))]
use serde::Deserialize;

/// Command that reinstalls the latest fork release, exactly as described by
/// `install.sh` at the repo root. Always compiled: `update_action.rs` refers
/// to it unconditionally (`UpdateAction` itself is not restricted to release
/// builds).
pub(crate) const FORK_INSTALL_COMMAND: &str = "curl -fsSL https://raw.githubusercontent.com/epsalmond/codex/eric/local-features/install.sh | bash";

/// The release tag this binary was built from. `None` for upstream/non-fork
/// builds.
pub(crate) const FORK_RELEASE_TAG: Option<&str> = option_env!("CODEX_FORK_RELEASE_TAG");

/// User-facing feature version for the Shake branding. This intentionally
/// stays separate from `FORK_RELEASE_TAG`, which identifies a particular
/// upstream base and release build.
pub(crate) const SHAKE_FEATURE_VERSION: &str = "0.1.0";

#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(debug_assertions, allow(dead_code))]
const CODEX_SHAKE_REPO_ENV: &str = "CODEX_SHAKE_REPO";
#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(debug_assertions, allow(dead_code))]
const DEFAULT_FORK_REPO: &str = "epsalmond/codex";

/// Return the Shake feature version only for binaries built as fork releases.
pub(crate) fn shake_feature_version() -> Option<&'static str> {
    FORK_RELEASE_TAG.map(|_| SHAKE_FEATURE_VERSION)
}

/// Where fork users should look for release notes.
#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(all(debug_assertions, test), allow(dead_code))]
pub(crate) const FORK_RELEASE_NOTES_URL: &str = "https://github.com/epsalmond/codex/releases";

/// Tags cut by `fork-release.yml` all start with this prefix.
#[cfg(any(not(debug_assertions), test))]
const FORK_TAG_PREFIX: &str = "local-features-v";

#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn is_fork_build() -> bool {
    FORK_RELEASE_TAG.is_some()
}

/// Returns whether a cached release value belongs to the fork's release feed.
#[cfg(any(not(debug_assertions), test))]
pub(crate) fn is_fork_release_tag(tag: &str) -> bool {
    fork_release_identity(tag).is_some()
}

/// Return the configured fork repository, falling back to the release repo.
/// The installer persists this environment variable in both generated
/// wrappers, so the updater and installer continue to target the same repo.
#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn fork_repository() -> String {
    std::env::var(CODEX_SHAKE_REPO_ENV)
        .ok()
        .filter(|repo| !repo.is_empty())
        .unwrap_or_else(|| DEFAULT_FORK_REPO.to_string())
}

#[cfg(any(not(debug_assertions), test))]
fn fork_releases_api_url_for_repo(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases?per_page=30")
}

/// Return the releases endpoint for the configured fork repository.
#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn fork_releases_api_url() -> String {
    fork_releases_api_url_for_repo(&fork_repository())
}

#[cfg(any(not(debug_assertions), test))]
fn fork_install_command_for_repo(repo: &str) -> String {
    let script_url =
        format!("https://raw.githubusercontent.com/{repo}/eric/local-features/install.sh");
    let curl_command = shlex::try_join(["curl", "-fsSL", script_url.as_str()])
        .expect("static curl command arguments should be shell-joinable");
    format!("{curl_command} | bash")
}

/// Return the reinstall command for the configured fork repository.
#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn fork_install_command() -> String {
    fork_install_command_for_repo(&fork_repository())
}

#[cfg(any(not(debug_assertions), test))]
#[derive(Deserialize, Debug, Clone)]
struct ForkReleaseEntry {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    draft: bool,
}

#[cfg(any(not(debug_assertions), test))]
#[derive(Debug, Eq, PartialEq)]
struct ReleaseIdentity {
    normalized_counter: Option<u64>,
    counter_precision: Option<u8>,
    source_sequence: Option<u64>,
    tie_break: String,
}

#[cfg(any(not(debug_assertions), test))]
fn valid_fork_version(version: &str) -> bool {
    let version = version.strip_prefix('v').unwrap_or(version);
    let version = version.strip_suffix("-main").unwrap_or(version);
    let mut components = version.split('.');
    components.clone().count() == 3
        && components.all(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|character| character.is_ascii_digit())
        })
}

#[cfg(any(not(debug_assertions), test))]
fn fork_release_identity(tag: &str) -> Option<ReleaseIdentity> {
    let rest = tag.strip_prefix(FORK_TAG_PREFIX)?;
    if let Some(counter_start) = rest.rfind("-r") {
        let version = &rest[..counter_start];
        if !valid_fork_version(version) {
            return None;
        }
        let counter_and_suffix = &rest[counter_start + 2..];
        let digit_count = counter_and_suffix
            .chars()
            .take_while(char::is_ascii_digit)
            .count();
        let counter_digits = &counter_and_suffix[..digit_count];
        let suffix = counter_and_suffix[digit_count..].strip_prefix('.')?;
        if counter_digits.is_empty()
            || suffix.is_empty()
            || !suffix
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return None;
        }
        let (normalized_digits, counter_precision) = if counter_digits.len() == 12 {
            (format!("{counter_digits}00"), 12)
        } else {
            (counter_digits.to_string(), counter_digits.len() as u8)
        };
        let normalized_counter = normalized_digits.parse().ok()?;
        let source_sequence = if suffix.len() == 28 {
            Some(u64::from_str_radix(&suffix[..16], 16).ok()?)
        } else {
            None
        };
        return Some(ReleaseIdentity {
            normalized_counter: Some(normalized_counter),
            counter_precision: Some(counter_precision),
            source_sequence,
            tie_break: format!("{version}:{suffix}").to_ascii_lowercase(),
        });
    }

    let (version, suffix) = rest.rsplit_once('-')?;
    if !valid_fork_version(version)
        || suffix.len() != 12
        || !suffix
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return None;
    }
    Some(ReleaseIdentity {
        normalized_counter: None,
        counter_precision: None,
        source_sequence: None,
        tie_break: format!("{version}:{suffix}").to_ascii_lowercase(),
    })
}

#[cfg(any(not(debug_assertions), test))]
fn compare_fork_release_tags(left: &str, right: &str) -> std::cmp::Ordering {
    match (fork_release_identity(left), fork_release_identity(right)) {
        (Some(left), Some(right)) => left
            .normalized_counter
            .cmp(&right.normalized_counter)
            .then_with(|| left.counter_precision.cmp(&right.counter_precision))
            .then_with(|| left.source_sequence.cmp(&right.source_sequence))
            .then_with(|| left.tie_break.cmp(&right.tie_break)),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => left.cmp(right),
    }
}

/// Parse the JSON body of `GET /repos/epsalmond/codex/releases` and return
/// the newest non-draft valid fork release. The API's creation order is not
/// source order, so select the maximum parsed release identity instead.
#[cfg(any(not(debug_assertions), test))]
pub(crate) fn newest_fork_release_tag(releases_json: &str) -> Option<String> {
    let entries: Vec<ForkReleaseEntry> = serde_json::from_str(releases_json).ok()?;
    entries
        .into_iter()
        .filter(|entry| !entry.draft && fork_release_identity(&entry.tag_name).is_some())
        .map(|entry| entry.tag_name)
        .max_by(|left, right| compare_fork_release_tags(left, right))
}

/// Current tags carry a `-r<UTC timestamp>` counter. Twelve-digit legacy
/// minute counters are normalized to second precision before comparison, so
/// they cannot outrank a new fourteen-digit counter merely because it has more
/// digits. The suffix is a deterministic tie-breaker for distinct releases in
/// the same second. Generated tags use a single hexadecimal suffix containing
/// a fixed-width ancestry sequence followed by the producer SHA; this keeps
/// the format readable by older binaries while making same-second ordering
/// follow source chronology. Bare SHA-suffixed tags predate numeric ordering.
#[cfg(any(not(debug_assertions), test))]
pub(crate) fn fork_tag_is_newer(candidate: &str, current: &str) -> bool {
    if candidate == current {
        return false;
    }
    if fork_release_identity(candidate).is_some() && fork_release_identity(current).is_some() {
        return compare_fork_release_tags(candidate, current).is_gt();
    }
    fork_release_identity(candidate).is_some() && fork_release_identity(current).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const SAMPLE_RELEASES_JSON: &str = r#"[
        {"tag_name": "local-features-v0.155.0-r202609131159.9876543", "draft": false},
        {"tag_name": "local-features-v0.154.0-r202609131200.abcdef1", "draft": false},
        {"tag_name": "local-features-v0.154.0-1122334455aa", "draft": false},
        {"tag_name": "v0.155.0", "draft": false},
        {"tag_name": "local-features-v0.156.0-r202609140000.deadbee", "draft": true},
        {"tag_name": "local-features-vnot-a-release-r99999999999999.bad", "draft": false}
    ]"#;

    #[test]
    fn picks_newest_non_draft_fork_release() {
        assert_eq!(
            newest_fork_release_tag(SAMPLE_RELEASES_JSON).as_deref(),
            Some("local-features-v0.154.0-r202609131200.abcdef1")
        );
    }

    #[test]
    fn ignores_non_fork_and_draft_entries() {
        let json = r#"[
            {"tag_name": "v0.155.0", "draft": false},
            {"tag_name": "local-features-v0.150.0-r1.abc", "draft": true},
            {"draft": false}
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
            "local-features-v0.155.0-r202609131201.abcdef1",
            "local-features-v0.154.0-r202609131200.9876543",
        ));
        assert!(!fork_tag_is_newer(
            "local-features-v0.154.0-r202609131200.9876543",
            "local-features-v0.155.0-r202609131201.abcdef1",
        ));
    }

    #[test]
    fn release_counter_detects_same_upstream_version_builds() {
        assert!(fork_tag_is_newer(
            "local-features-v0.154.0-r202609131201.abcdef1",
            "local-features-v0.154.0-r202609131200.abcdef1",
        ));
    }

    #[test]
    fn normalizes_legacy_minute_counter_before_comparing_seconds() {
        assert!(fork_tag_is_newer(
            "local-features-v0.154.0-main-r20260914000000.abcdef123456",
            "local-features-v0.154.0-r202609140000.987654321000",
        ));
        assert!(fork_tag_is_newer(
            "local-features-v0.154.0-main-r20260914000001.abcdef123456",
            "local-features-v0.154.0-r202609140000.987654321000",
        ));
        assert!(!fork_tag_is_newer(
            "local-features-v0.154.0-r202609140000.987654321000",
            "local-features-v0.154.0-main-r20260914000001.abcdef123456",
        ));
    }

    #[test]
    fn same_second_releases_use_ancestry_sequence_before_producer_sha() {
        assert!(fork_tag_is_newer(
            "local-features-v0.154.0-main-r20260914000000.0000000000000002aaaaaaaaaaaa",
            "local-features-v0.154.0-main-r20260914000000.0000000000000001ffffffffffff",
        ));
        assert!(!fork_tag_is_newer(
            "local-features-v0.154.0-main-r20260914000000.0000000000000001ffffffffffff",
            "local-features-v0.154.0-main-r20260914000000.0000000000000002aaaaaaaaaaaa",
        ));
    }

    #[test]
    fn cached_release_tag_must_belong_to_the_fork() {
        assert!(is_fork_release_tag("local-features-v0.154.0-r1.abc"));
        assert!(is_fork_release_tag(
            "local-features-v0.154.0-main-r20260914000000.0000000000000001aaaaaaaaaaaa"
        ));
        assert!(!is_fork_release_tag(
            "local-features-v0.154.0-main-r20260914000000.0000000000000001.aaaaaaaaaaaa"
        ));
        assert!(!is_fork_release_tag("0.155.0"));
    }

    #[test]
    fn configured_repository_is_used_for_feed_and_install_urls() {
        assert_eq!(
            fork_releases_api_url_for_repo("owner/repo"),
            "https://api.github.com/repos/owner/repo/releases?per_page=30"
        );
        assert_eq!(
            fork_install_command_for_repo("owner/repo"),
            "curl -fsSL https://raw.githubusercontent.com/owner/repo/eric/local-features/install.sh | bash"
        );
    }

    #[test]
    fn custom_repository_command_is_shell_quoted() {
        let command = fork_install_command_for_repo("owner/repo '$x;touch nope'");
        assert_eq!(
            shlex::split(&command).expect("generated command should parse as shell words"),
            vec![
                "curl",
                "-fsSL",
                "https://raw.githubusercontent.com/owner/repo '$x;touch nope'/eric/local-features/install.sh",
                "|",
                "bash",
            ]
        );
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
        assert!(!fork_tag_is_newer(
            "local-features-v0.154.0-1122334455aa",
            "local-features-v0.155.0-r202609131200.abcdef1",
        ));
    }
}
