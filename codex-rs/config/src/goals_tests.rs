use crate::config_toml::ConfigToml;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn goal_guard_modes_and_replacement_literals_roundtrip() -> anyhow::Result<()> {
    for mode in ["off", "observe", "defer"] {
        let source = format!(
            "[goals]\ncontinuation_guard_mode = \"{mode}\"\nstall_after_no_progress_turns = 5\nstall_waiting_prefixes = [\"I’M   WAITING\", \"é\"]\n"
        );
        let parsed: ConfigToml = toml::from_str(&source)?;
        assert_eq!(
            serde_json::to_value(parsed.goals)?,
            json!({
                "max_goal_token_budget": null,
                "continuation_guard_mode": mode,
                "stall_after_no_progress_turns": 5,
                "stall_waiting_prefixes": ["I’M   WAITING", "é"],
            })
        );
    }
    let source =
        "[goals]\nstall_waiting_prefixes = []\nstall_after_no_progress_turns = 4294967295\n";
    let parsed: ConfigToml = toml::from_str(source)?;
    assert_eq!(
        serde_json::to_value(parsed.goals)?,
        json!({
            "max_goal_token_budget": null,
            "continuation_guard_mode": null,
            "stall_after_no_progress_turns": 4294967295u32,
            "stall_waiting_prefixes": [],
        })
    );
    Ok(())
}

#[test]
fn goal_guard_invalid_values_are_config_errors() -> anyhow::Result<()> {
    for setting in [
        "continuation_guard_mode = \"pause\"",
        "stall_after_no_progress_turns = 0",
        "stall_after_no_progress_turns = -1",
        "stall_after_no_progress_turns = 1.5",
        "stall_after_no_progress_turns = \"3\"",
        "stall_after_no_progress_turns = 4294967296",
        "stall_waiting_prefixes = \"waiting\"",
        "stall_waiting_prefixes = [\"\"]",
        "stall_waiting_prefixes = [\" \t\u{2003}\"]",
    ] {
        let source = format!("[goals]\n{setting}\n");
        assert!(
            toml::from_str::<ConfigToml>(&source).is_err(),
            "invalid goal setting accepted: {setting}"
        );
    }
    for values in [vec!["waiting".to_owned(); 33], vec!["é".repeat(/*n*/ 65)]] {
        let source = format!(
            "[goals]\nstall_waiting_prefixes = {}\n",
            serde_json::to_string(&values)?
        );
        let error = toml::from_str::<ConfigToml>(&source).expect_err("invalid literal bounds");
        assert!(error.to_string().contains("goals.stall_waiting_prefixes"));
    }
    let values = vec!["é".repeat(/*n*/ 64); 32];
    let source = format!(
        "[goals]\nstall_waiting_prefixes = {}\n",
        serde_json::to_string(&values)?
    );
    let parsed: ConfigToml = toml::from_str(&source)?;
    assert_eq!(
        serde_json::to_value(parsed.goals)?["stall_waiting_prefixes"],
        serde_json::to_value(values)?
    );
    Ok(())
}
