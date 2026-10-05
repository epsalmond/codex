use super::Config;
use super::ConfigBuilder;
use super::ConfigOverrides;
use super::GoalContinuationGuardMode;
use codex_config::CONFIG_TOML_FILE;
use codex_config::LoaderOverrides;
use codex_config::config_toml::ConfigToml;
use core_test_support::TempDirExt;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[tokio::test]
async fn goal_guard_loads_effective_defaults_and_literal_replacements() -> anyhow::Result<()> {
    let home = tempdir()?;
    for (source, expected) in [
        ("", (GoalContinuationGuardMode::Defer, 2, None)),
        (
            "[goals]\ncontinuation_guard_mode = \"observe\"\nstall_after_no_progress_turns = 7\nstall_waiting_prefixes = [\"I’M   WAITING\"]\n",
            (
                GoalContinuationGuardMode::Observe,
                7,
                Some(vec!["I’M   WAITING".to_owned()]),
            ),
        ),
        (
            "[goals]\ncontinuation_guard_mode = \"off\"\nstall_waiting_prefixes = []\n",
            (GoalContinuationGuardMode::Off, 2, Some(vec![])),
        ),
    ] {
        let parsed: ConfigToml = toml::from_str(source)?;
        let config = Config::load_from_base_config_with_overrides(
            parsed,
            ConfigOverrides::default(),
            home.abs(),
        )
        .await?;
        assert_eq!(
            (
                config.goal_continuation_guard_mode,
                config.goal_stall_after_no_progress_turns.get(),
                config.goal_stall_waiting_prefixes
            ),
            expected
        );
    }
    Ok(())
}

#[tokio::test]
async fn goal_guard_managed_values_replace_user_values_per_load() -> anyhow::Result<()> {
    let home = tempdir()?;
    let managed = home.path().join("managed_config.toml");
    std::fs::write(
        home.path().join(CONFIG_TOML_FILE),
        "[goals]\ncontinuation_guard_mode = \"off\"\nstall_after_no_progress_turns = 5\nstall_waiting_prefixes = [\"user waiting\"]\n",
    )?;
    std::fs::write(
        &managed,
        "[goals]\ncontinuation_guard_mode = \"observe\"\nstall_after_no_progress_turns = 7\nstall_waiting_prefixes = []\n",
    )?;
    let builder = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::with_managed_config_path_for_tests(
            managed.clone(),
        ));
    let initial = builder.clone().build().await?;
    std::fs::write(
        &managed,
        "[goals]\ncontinuation_guard_mode = \"defer\"\nstall_after_no_progress_turns = 4\nstall_waiting_prefixes = [\"managed waiting\"]\n",
    )?;
    let reloaded = builder.build().await?;
    assert_eq!(
        (
            initial.goal_continuation_guard_mode,
            initial.goal_stall_after_no_progress_turns.get(),
            initial.goal_stall_waiting_prefixes
        ),
        (GoalContinuationGuardMode::Observe, 7, Some(vec![]))
    );
    assert_eq!(
        (
            reloaded.goal_continuation_guard_mode,
            reloaded.goal_stall_after_no_progress_turns.get(),
            reloaded.goal_stall_waiting_prefixes
        ),
        (
            GoalContinuationGuardMode::Defer,
            4,
            Some(vec!["managed waiting".to_owned()])
        )
    );
    Ok(())
}

#[tokio::test]
async fn goal_guard_invalid_literals_report_the_source_key() -> anyhow::Result<()> {
    let home = tempdir()?;
    std::fs::write(
        home.path().join(CONFIG_TOML_FILE),
        "[goals]\nstall_waiting_prefixes = [\"   \"]\n",
    )?;
    let result = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await;
    let error = result.expect_err("invalid waiting literal");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("goals.stall_waiting_prefixes"));
    Ok(())
}
