#![allow(dead_code)]

#[path = "../src/stall.rs"]
mod stall;
#[path = "../src/stall_observation.rs"]
mod stall_observation;

use pretty_assertions::assert_eq;
use serde_json::json;
use stall::Assessment;
use stall::StallDetector;
use stall::Suspicion;
use stall_observation::CallKind;
use stall_observation::CallParent;
use stall_observation::TurnObserver;
use std::num::NonZeroU32;

#[allow(clippy::expect_used)]
fn threshold() -> NonZeroU32 {
    NonZeroU32::new(/*n*/ 3).expect("positive test threshold")
}

fn empty() -> TurnObserver {
    let mut turn = TurnObserver::default();
    turn.automatic();
    turn
}

#[test]
fn repeated_actions_have_a_baseline_then_three_repetitions() {
    let mut detector = StallDetector::default();
    for index in 1..=4 {
        let turn = action(
            &index.to_string(),
            "read status --target worker-a --range 10:20",
            "pending",
            /*exit_code*/ 0,
        );
        let actual = detector.observe(&index.to_string(), turn.finish(), threshold());
        assert_eq!(
            actual,
            if index == 1 {
                Assessment::NotSuspected
            } else {
                Assessment::Suspected {
                    reason: Suspicion::RepeatedActions,
                    streak: index - 1,
                    threshold_reached: index == 4,
                }
            }
        );
    }
    for (index, (command, output, status)) in [
        ("read status --target worker-b --range 10:20", "pending", 0),
        ("read status --target worker-a --range 20:30", "pending", 0),
        (
            "read status --target worker-a --range 20:30",
            "completed",
            0,
        ),
        (
            "read status --target worker-a --range 20:30",
            "completed",
            1,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            detector.observe(
                &format!("changed-{index}"),
                action(command, command, output, status).finish(),
                threshold()
            ),
            Assessment::NotSuspected
        );
    }
}

#[test]
fn incomplete_or_unsupported_inbound_results_remain_unknown() {
    for content in [
        json!(null),
        json!("  "),
        json!([]),
        json!({"status":"pending"}),
        json!([{"type":"input_text"}]),
        json!([{"type":"input_text","text":"visible"},{"type":"input_text"}]),
        json!([{"type":"encrypted_text","text":"opaque"}]),
        json!([{"type":"input_text","text":"Message Type: MESSAGE\nSender: worker\nPayload:\n"}]),
    ] {
        let mut observer = empty();
        observer.external_result(&content);
        let observation = observer.finish();
        assert_eq!(
            (observation.complete, observation.external_result),
            (false, None)
        );
    }
    let mut observer = empty();
    observer.external_result(&json!([{"type":"input_text","text":"New verified receipt"}]));
    let observation = observer.finish();
    assert_eq!(
        (observation.complete, observation.external_result.is_some()),
        (true, true)
    );
}

#[test]
fn missing_and_unlinked_outcomes_remain_unclassified() {
    let mut detector = StallDetector::default();
    let mut missing = empty();
    missing.call(
        "missing",
        "read",
        &json!({"target":"a"}),
        CallKind::Leaf,
        CallParent::Direct,
    );
    assert_eq!(
        detector.observe("missing", missing.finish(), threshold()),
        Assessment::Unclassified
    );
    let mut unlinked = empty();
    unlinked.call(
        "leaf",
        "read",
        &json!({"target":"a"}),
        CallKind::Leaf,
        CallParent::Nested("absent"),
    );
    unlinked.outcome("leaf", "read", &json!("same"));
    assert_eq!(
        detector.observe("unlinked", unlinked.finish(), threshold()),
        Assessment::Unclassified
    );
}

#[test]
fn wrappers_coalesce_only_with_linked_leaf_outcomes() {
    let mut detector = StallDetector::default();
    for index in 1..=4 {
        let mut turn = empty();
        turn.call(
            "outer",
            "exec",
            &json!("return await tools.read({target:'a'})"),
            CallKind::Wrapper,
            CallParent::Direct,
        );
        turn.call(
            "inner",
            "read",
            &json!({"target":"a"}),
            CallKind::Leaf,
            CallParent::Nested("outer"),
        );
        turn.outcome("inner", "read", &json!({"status":"pending"}));
        turn.outcome("outer", "exec", &json!({"status":"pending"}));
        assert_eq!(
            detector.observe(&index.to_string(), turn.finish(), threshold()),
            if index == 1 {
                Assessment::NotSuspected
            } else {
                Assessment::Suspected {
                    reason: Suspicion::RepeatedActions,
                    streak: index - 1,
                    threshold_reached: index == 4,
                }
            }
        );
    }
    let mut turn = empty();
    turn.call(
        "outer",
        "exec",
        &json!("changed_code()"),
        CallKind::Wrapper,
        CallParent::Direct,
    );
    turn.outcome("outer", "exec", &json!("pending"));
    assert_eq!(
        detector.observe("missing-leaf", turn.finish(), threshold()),
        Assessment::Unclassified
    );
}

#[test]
fn self_contained_timer_cells_share_live_and_replay_canonicalization() {
    let mut detector = StallDetector::default();
    for index in 1..=4 {
        let mut turn = empty();
        turn.call(
            "cell",
            "exec",
            &json!("await new Promise(resolve => setTimeout(resolve, 45000)); text(\"finished\");"),
            CallKind::Wrapper,
            CallParent::Direct,
        );
        turn.outcome("cell","exec",&json!([{ "type":"input_text","text":if index%2==0 {format!("Script completed\nWall time {index}.000 seconds (code-mode 0.750 seconds; overhead 0.500 seconds)\nOutput:\n")}else{format!("Script completed\nWall time {index}.0 seconds\nOutput:\n")}},{"type":"input_text","text":"finished"}]));
        assert_eq!(
            detector.observe(&index.to_string(), turn.finish(), threshold()),
            if index == 1 {
                Assessment::NotSuspected
            } else {
                Assessment::Suspected {
                    reason: Suspicion::RepeatedActions,
                    streak: index - 1,
                    threshold_reached: index == 4,
                }
            }
        );
    }
    for code in [
        "await new Promise(resolve => setTimeout(resolve, 45000)); text(await tools.read({target:'new'}));",
        "await new Promise(resolve => setTimeout(resolve, 45000)); text(\"finished\"); await tools.write({target:'new'});",
        "await new Promise(resolve => setTimeout(other, 45000));",
        "await new Promise(resolve => setTimeout(resolve, delay));",
        "await new Promise(resolve => setTimeout(resolve, 45000)); text(eval('work()'));",
    ] {
        let mut turn = empty();
        turn.call(
            "cell",
            "exec",
            &json!(code),
            CallKind::Wrapper,
            CallParent::Direct,
        );
        turn.outcome("cell","exec",&json!([{ "type":"input_text","text":"Script completed\nWall time 1.0 seconds\nOutput:\n"}]));
        assert_eq!(
            detector.observe(code, turn.finish(), threshold()),
            Assessment::Unclassified
        );
    }
}

fn action(id: &str, command: &str, output: &str, exit_code: i32) -> TurnObserver {
    let mut turn = empty();
    turn.call(
        id,
        "exec_command",
        &json!({"cmd":command,"session_id":id,"yield_time_ms":1000}),
        CallKind::Leaf,
        CallParent::Direct,
    );
    turn.outcome(
        id,
        "exec_command",
        &json!({"output":output,"exit_code":exit_code,"chunk_id":id,"wall_time_seconds":0.25}),
    );
    turn
}
