use std::process::Command;

use pretty_assertions::assert_eq;

#[test]
fn fork_version_output_identifies_features() -> anyhow::Result<()> {
    let binary = codex_utils_cargo_bin::cargo_bin("codex")?;
    let long_version = Command::new(&binary).arg("--version").output()?;
    assert!(long_version.status.success());
    let long_version = String::from_utf8(long_version.stdout)?;

    let short_version = Command::new(binary).arg("-V").output()?;
    assert!(short_version.status.success());
    let short_version = String::from_utf8(short_version.stdout)?;
    let source_version = env!("CARGO_PKG_VERSION");
    let expected_short_version = format!("codex-cli {source_version}\n");
    assert_eq!(short_version, expected_short_version);

    if let Some(release_tag) = option_env!("CODEX_FORK_RELEASE_TAG") {
        let normalized_long_version = long_version
            .replace(release_tag, "<release-tag>")
            .replace(env!("CARGO_PKG_VERSION"), "<codex-version>");
        insta::assert_snapshot!(&normalized_long_version, @r"
codex-cli <codex-version>
Shake feature version: 0.3.0
Release tag: <release-tag>
");
    } else {
        assert_eq!(long_version, short_version);
    }

    Ok(())
}
