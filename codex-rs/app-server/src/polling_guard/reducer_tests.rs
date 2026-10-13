use codex_protocol::models::FunctionCallOutputPayload;
use codex_tools::ToolCallSource;
use pretty_assertions::assert_eq;

use super::*;
use crate::polling_guard::observation;

fn start(id: &str, kind: Kind) -> Start {
    Start {
        key: observation::call_key(id, &ToolCallSource::Direct),
        kind,
        nested: None,
    }
}

fn begin(state: &mut Episode, call: Start) -> bool {
    let intent = match call.kind {
        Kind::Check(_) if call.nested.is_some() => Intent::NestedStatus,
        Kind::Check(_) => Intent::Status,
        Kind::Wrapper { .. } => Intent::Wrapper,
        Kind::Unknown => Intent::Unknown,
    };
    state.dispatch(call.key, intent);
    state.start(call)
}

fn output(text: &str) -> ResponseInputItem {
    ResponseInputItem::FunctionCallOutput {
        call_id: "ignored".to_owned(),
        output: FunctionCallOutputPayload {
            body: codex_protocol::models::FunctionCallOutputBody::Text(text.to_owned()),
            success: Some(true),
        },
    }
}

fn empty_process(session: i32) -> ResponseInputItem {
    output(&format!(
        "Chunk ID: volatile\nWall time: 0.0010 seconds\nProcess running with session ID {session}\nOriginal token count: 0\nOutput:\n"
    ))
}

fn complete(state: &mut Episode, id: &str, check: Check, result: &ResponseInputItem) -> bool {
    let intervention = begin(state, start(id, Kind::Check(check)));
    let key = observation::call_key(id, &ToolCallSource::Direct);
    state.finish(key, /*success*/ true);
    state.output(key, result);
    intervention
}

#[test]
fn live_and_replay_incident_intervene_once_at_the_fourth_start() {
    let incident = (0..8)
        .map(|index| (index.to_string(), empty_process(7)))
        .collect::<Vec<_>>();
    let run = || {
        let mut state = Episode::default();
        incident
            .iter()
            .map(|(id, output)| complete(&mut state, id, Check::Process(7), output))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        run(),
        vec![false, false, false, true, false, false, false, false]
    );
    assert_eq!(run(), run());
}

#[test]
fn changed_target_changed_result_completion_and_input_reset_evidence() {
    for change in [
        "target",
        "result",
        "completion",
        "input",
        "failure",
        "unknown",
    ] {
        let mut state = Episode::default();
        for id in ["1", "2"] {
            assert!(!complete(
                &mut state,
                id,
                Check::Process(7),
                &empty_process(7)
            ));
        }
        match change {
            "target" => {
                complete(&mut state, "changed", Check::Process(8), &empty_process(8));
            }
            "result" => {
                complete(
                    &mut state,
                    "changed",
                    Check::Process(7),
                    &output(
                        "Chunk ID: x\nWall time: 0.01 seconds\nProcess running with session ID 7\nOutput:\nprogress",
                    ),
                );
            }
            "completion" => {
                complete(
                    &mut state,
                    "changed",
                    Check::Process(7),
                    &output(
                        "Chunk ID: x\nWall time: 0.01 seconds\nProcess exited with code 0\nOutput:\n",
                    ),
                );
            }
            "input" => state.reset(),
            "failure" => {
                begin(&mut state, start("changed", Kind::Check(Check::Process(7))));
                state.finish(
                    observation::call_key("changed", &ToolCallSource::Direct),
                    false,
                );
            }
            "unknown" => {
                begin(&mut state, start("changed", Kind::Unknown));
                state.finish(
                    observation::call_key("changed", &ToolCallSource::Direct),
                    true,
                );
                state.output(
                    observation::call_key("changed", &ToolCallSource::Direct),
                    &output("read/edit/test"),
                );
            }
            _ => unreachable!(),
        }
        assert!(
            !complete(&mut state, "fresh-1", Check::Process(7), &empty_process(7)),
            "{change}"
        );
        assert!(
            !complete(&mut state, "fresh-2", Check::Process(7), &empty_process(7)),
            "{change}"
        );
        assert!(
            !complete(&mut state, "fresh-3", Check::Process(7), &empty_process(7)),
            "{change}"
        );
        assert!(
            begin(&mut state, start("fresh-4", Kind::Check(Check::Process(7)))),
            "{change}"
        );
    }
}

#[test]
fn parallel_calls_and_long_helpers_need_a_fresh_serial_episode() {
    let mut state = Episode::default();
    complete(&mut state, "1", Check::Process(7), &empty_process(7));
    begin(&mut state, start("helper", Kind::Unknown));
    for id in ["parallel-1", "parallel-2", "parallel-3", "parallel-4"] {
        assert!(!complete(
            &mut state,
            id,
            Check::Process(7),
            &empty_process(7)
        ));
    }
    let key = observation::call_key("helper", &ToolCallSource::Direct);
    state.finish(key, /*success*/ true);
    state.output(key, &output("test passed"));
    for id in ["fresh-1", "fresh-2", "fresh-3"] {
        assert!(!complete(
            &mut state,
            id,
            Check::Process(7),
            &empty_process(7)
        ));
    }
    assert!(begin(
        &mut state,
        start("fresh-4", Kind::Check(Check::Process(7)))
    ));
}

#[test]
fn dispatch_intent_suppresses_serialized_parallel_and_unmatched_rejections() {
    for mode in ["parallel", "malformed"] {
        let mut state = Episode::default();
        if mode == "parallel" {
            for id in ["1", "2", "3", "4"] {
                state.dispatch(start(id, Kind::Unknown).key, Intent::Status);
            }
            for id in ["1", "2", "3", "4"] {
                assert!(!state.start(start(id, Kind::Check(Check::Process(7)))));
                let key = start(id, Kind::Unknown).key;
                state.finish(key, /*success*/ true);
                state.output(key, &empty_process(7));
            }
        } else {
            state.dispatch(start("malformed", Kind::Unknown).key, Intent::Status);
            for id in ["1", "2", "3", "4", "5", "6"] {
                assert!(!complete(
                    &mut state,
                    id,
                    Check::Process(7),
                    &empty_process(7)
                ));
            }
            assert_eq!(state.dispatched.len(), 1);
        }
    }
}

#[test]
fn dispatch_during_an_active_handler_suppresses_later_serialized_completion() {
    let mut state = Episode::default();
    for pair in 0..4 {
        let first = start(&format!("first-{pair}"), Kind::Check(Check::Process(7)));
        let second = start(&format!("second-{pair}"), Kind::Check(Check::Process(7)));
        let first_key = first.key;
        let second_key = second.key;
        assert!(!begin(&mut state, first));
        state.dispatch(second_key, Intent::Status);
        state.finish(first_key, /*success*/ true);
        state.output(first_key, &empty_process(7));
        assert!(!state.start(second));
        state.finish(second_key, /*success*/ true);
        state.output(second_key, &empty_process(7));
    }
    assert_eq!(state.candidate, None);
}

#[test]
fn unrelated_dispatch_crossing_a_wrapper_handoff_cannot_inherit_its_evidence() {
    for mode in ["wrapper", "direct"] {
        let mut state = Episode::default();
        let first_origin = observation::digest(b"first-item");
        let second_origin = observation::digest(b"second-item");
        begin(
            &mut state,
            start(
                "first-wrapper",
                Kind::Wrapper {
                    origin: first_origin,
                    timer: false,
                },
            ),
        );
        for index in 0..3 {
            let mut leaf = start(&index.to_string(), Kind::Check(Check::Process(7)));
            leaf.nested = Some((first_origin, observation::digest(b"first-cell")));
            let key = leaf.key;
            assert!(!begin(&mut state, leaf));
            state.finish(key, /*success*/ true);
            state.output(key, &empty_process(7));
        }
        let next = if mode == "wrapper" {
            Kind::Wrapper {
                origin: second_origin,
                timer: false,
            }
        } else {
            Kind::Check(Check::Process(7))
        };
        let next = start("second", next);
        state.dispatch(
            next.key,
            if mode == "wrapper" {
                Intent::Wrapper
            } else {
                Intent::Status
            },
        );
        let first_key = start("first-wrapper", Kind::Unknown).key;
        state.finish(first_key, /*success*/ true);
        state.output(first_key, &output("three completed leaves"));
        assert!(!state.start(next));
        if mode == "wrapper" {
            let mut leaf = start("second-leaf", Kind::Check(Check::Process(7)));
            leaf.nested = Some((second_origin, observation::digest(b"second-cell")));
            assert!(!begin(&mut state, leaf));
        }
    }
}

#[test]
fn exact_code_mode_origin_counts_leaves_without_counting_wrapper() {
    let mut state = Episode::default();
    let origin = observation::digest(b"item");
    let cell = observation::digest(b"cell");
    assert!(!begin(
        &mut state,
        start(
            "wrapper",
            Kind::Wrapper {
                origin,
                timer: false
            }
        )
    ));
    let decisions = (0..4)
        .map(|index| {
            let mut leaf = start(&index.to_string(), Kind::Check(Check::Process(7)));
            leaf.nested = Some((origin, cell));
            let key = leaf.key;
            let decision = begin(&mut state, leaf);
            state.finish(key, /*success*/ true);
            state.output(key, &empty_process(7));
            decision
        })
        .collect::<Vec<_>>();
    assert_eq!(decisions, vec![false, false, false, true]);
}

#[test]
fn missing_origin_unsupported_output_caps_and_duplicate_calls_never_arm() {
    for mode in ["origin", "oversized", "duplicate", "inflight"] {
        let mut state = Episode::default();
        for index in 0..8 {
            let mut call = start(&index.to_string(), Kind::Check(Check::Process(7)));
            if mode == "origin" {
                call.nested = Some((
                    observation::digest(b"missing"),
                    observation::digest(b"cell"),
                ));
            }
            let key = call.key;
            assert!(!begin(&mut state, call));
            if mode == "duplicate" {
                assert!(!begin(
                    &mut state,
                    start(&index.to_string(), Kind::Check(Check::Process(7)))
                ));
            }
            if mode != "inflight" {
                state.finish(key, /*success*/ true);
                state.output(
                    key,
                    &if mode == "oversized" {
                        output(&"x".repeat(observation::MAX_BYTES + 1))
                    } else {
                        empty_process(7)
                    },
                );
            }
        }
        assert!(!begin(
            &mut state,
            Start {
                key: None,
                kind: Kind::Check(Check::Process(7)),
                nested: None
            }
        ));
        assert_eq!(state.pending.len(), 0);
    }
}

#[test]
fn strict_action_and_output_grammar_preserves_real_targets_and_progress() {
    assert_eq!(
        observation::check("write_stdin", r#"{"session_id":7}"#),
        Some(Check::Process(7))
    );
    assert_eq!(
        observation::check("write_stdin", r#"{"session_id":8,"chars":""}"#),
        Some(Check::Process(8))
    );
    for args in [
        r#"{"session_id":7,"chars":"a"}"#,
        r#"{"session_id":7,"yield_time_ms":60001}"#,
        r#"{"session_id":7,"unknown":0}"#,
    ] {
        assert_eq!(observation::check("write_stdin", args), None);
    }
    for name in [
        "exec_command",
        "read_file",
        "rg",
        "wait",
        "wait_agent",
        "mcp__provider__status",
    ] {
        assert_eq!(observation::check(name, "{}"), None);
    }
    let action = observation::check("list_agents", "{}").unwrap();
    let running =
        output(r#"{"agents":[{"agent_name":"worker","agent_status":"running","context":null}]}"#);
    let done = output(
        r#"{"agents":[{"agent_name":"worker","agent_status":{"completed":"done"},"context":null}]}"#,
    );
    let telemetry = output(
        r#"{"agents":[{"context":{"active_tokens":9999,"observed_at":10},"agent_status":"running","agent_name":"worker"}]}"#,
    );
    assert_eq!(
        observation::outcome(action, &running),
        observation::outcome(action, &telemetry)
    );
    assert_ne!(
        observation::outcome(action, &running),
        observation::outcome(action, &done)
    );
    assert_eq!(
        observation::outcome(
            action,
            &output(
                r#"{"agents":[{"agent_name":"worker","agent_status":"surprise","context":null}]}"#
            )
        ),
        None
    );
    assert_eq!(
        observation::outcome(Check::Process(8), &empty_process(7)),
        None
    );
}
