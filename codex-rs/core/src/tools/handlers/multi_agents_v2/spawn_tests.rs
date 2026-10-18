use super::SpawnAgentArgs;
use crate::agent::types::SpawnAgentForkMode;
use pretty_assertions::assert_eq;
use serde_json::json;

fn fork_mode(args: serde_json::Value) -> Option<SpawnAgentForkMode> {
    serde_json::from_value::<SpawnAgentArgs>(args)
        .expect("spawn args should parse")
        .fork_mode()
        .expect("fork_turns should be accepted")
}

#[test]
fn spawn_without_fork_turns_starts_without_parent_history() {
    for args in [
        json!({"message": "task", "task_name": "worker"}),
        json!({"message": "task", "task_name": "worker", "fork_turns": ""}),
        json!({"message": "task", "task_name": "worker", "fork_turns": "  "}),
        json!({"message": "task", "task_name": "worker", "fork_turns": "None"}),
    ] {
        assert_eq!(fork_mode(args.clone()), None, "{args}");
    }
}

#[test]
fn spawn_with_explicit_fork_turns_forks_full_history() {
    for fork_turns in ["all", "ALL", "1", "3"] {
        assert_eq!(
            fork_mode(json!({
                "message": "task",
                "task_name": "worker",
                "fork_turns": fork_turns,
            })),
            Some(SpawnAgentForkMode::FullHistory),
            "fork_turns={fork_turns}"
        );
    }
}
