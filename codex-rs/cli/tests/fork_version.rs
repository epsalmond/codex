use std::process::Command;

use pretty_assertions::assert_eq;

#[test]
fn fork_version_output_identifies_features() -> anyhow::Result<()> {
    let binary = codex_utils_cargo_bin::cargo_bin("codex")?;
    let long_version = Command::new(&binary).arg("--version").output()?;
    assert!(long_version.status.success());
    let long_version = String::from_utf8(long_version.stdout)?;

    let short_version = Command::new(&binary).arg("-V").output()?;
    assert!(short_version.status.success());
    let short_version = String::from_utf8(short_version.stdout)?;
    let source_version = env!("CARGO_PKG_VERSION");
    let expected_short_version = format!("codex-cli {source_version}\n");
    assert_eq!(short_version, expected_short_version);

    // Runtime variables cannot change the identity compiled into an old CLI.
    let overridden = Command::new(&binary)
        .env("CODEX_FORK_RELEASE_TAG", "different-runtime-tag")
        .env("CODEX_FORK_SOURCE_SHA", "different-runtime-source")
        .arg("--version")
        .output()?;
    assert!(overridden.status.success());
    assert_eq!(String::from_utf8(overridden.stdout)?, long_version);

    if let Some(release_tag) = option_env!("CODEX_FORK_RELEASE_TAG") {
        let lines: Vec<_> = long_version.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], format!("codex-cli {source_version}"));
        assert_eq!(lines[1], "Shake feature version: 0.4.0");
        assert!(lines[2].starts_with("CLI build: "));
        assert_eq!(
            lines[3],
            format!(
                "Source commit: {}",
                option_env!("CODEX_FORK_SOURCE_SHA").unwrap_or("unknown (legacy build)")
            )
        );
        assert_eq!(lines[4], format!("Release tag: {release_tag}"));
    } else {
        assert_eq!(long_version, short_version);
    }

    Ok(())
}
