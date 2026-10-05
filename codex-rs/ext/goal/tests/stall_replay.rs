#![allow(dead_code)]

#[path = "../src/stall.rs"]
mod stall;
#[path = "../src/stall_observation.rs"]
mod stall_observation;
#[path = "../src/stall_replay.rs"]
mod stall_replay;
#[path = "../src/stall_settings.rs"]
mod stall_settings;

use pretty_assertions::assert_eq;
use serde_json::json;
use stall::Assessment;
use stall_replay::Replay;
use std::num::NonZeroU32;

#[allow(clippy::expect_used)]
fn threshold() -> NonZeroU32 {
    NonZeroU32::new(/*n*/ 3).expect("positive test threshold")
}

#[test]
#[ignore = "requires a private local rollout manifest"]
fn real_local_episode_replay() -> anyhow::Result<()> {
    let path = std::env::var("CODEX_STALL_CORPUS_MANIFEST")?;
    let manifest: serde_json::Value = serde_json::from_reader(std::fs::File::open(path)?)?;
    let mut results = Vec::new();
    let mut failures = Vec::new();
    let threshold = NonZeroU32::new(manifest["threshold"].as_u64().unwrap_or(/*default*/ 3) as u32)
        .expect("positive threshold");
    for episode in manifest["episodes"].as_array().expect("episode manifest") {
        let mut replay = if episode["counterfactual_auto"].as_bool().unwrap_or(false) {
            Replay::counterfactual_auto()
        } else {
            Replay::default()
        };
        let mut classified = 0;
        let mut suspicious = 0;
        let mut deferred = false;
        let mut first_hold_line = None;
        let path = episode["path"].as_str().expect("local source path");
        use std::io::BufRead;
        let start = episode["start_line"].as_u64().unwrap_or(1);
        let end = episode["end_line"].as_u64().unwrap_or(u64::MAX);
        for (index, line) in std::io::BufReader::new(std::fs::File::open(path)?)
            .lines()
            .enumerate()
        {
            let index = index as u64 + 1;
            if index < start {
                continue;
            }
            if index > end {
                break;
            }
            let record: serde_json::Value = serde_json::from_str(&line?)?;
            if let Some(assessment) = replay.record(&record, threshold) {
                classified += usize::from(!matches!(assessment, Assessment::Unclassified));
                if let Assessment::Suspected {
                    threshold_reached, ..
                } = assessment
                {
                    suspicious += 1;
                    deferred |= threshold_reached;
                    if threshold_reached && first_hold_line.is_none() {
                        first_hold_line = Some(index);
                    }
                }
            }
        }
        results.push(json!({"episode":episode["id"],"assessed_turns":classified,"complete_observation_turns":replay.stats.turns-replay.stats.unknown,"suspicious_turns":suspicious,"would_defer":deferred,"first_hold_line":first_hold_line,"expected_stall":episode["expected_stall"],"counterfactual_auto":episode["counterfactual_auto"].as_bool().unwrap_or(false),"automatic_turns":replay.stats.automatic,"completed_turns":replay.stats.turns,"unknown_observation_turns":replay.stats.unknown,"capped_turns":replay.stats.capped,"max_streak":replay.stats.max_streak}));
        if deferred
            != episode["expected_stall"]
                .as_bool()
                .expect("separate evaluation label")
        {
            failures.push(episode["id"].clone());
        }
    }
    println!("CORPUS_REPORT {}", serde_json::to_string(&results)?);
    assert_eq!(
        failures,
        Vec::<serde_json::Value>::new(),
        "actual corpus acceptance at threshold {}",
        threshold
    );
    Ok(())
}

#[test]
fn sanitized_rollout_events_exercise_actual_extractor_and_reset_boundaries() {
    let mut replay = Replay::default();
    for index in 1..=4 {
        let id = format!("turn-{index}");
        let mut records = vec![
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal."}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["goal.internal_context"]}}}),
        ];
        if index == 3 {
            records.push(json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"New user work"}]}}));
        }
        records.push(json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id,"last_agent_message":"I’M STILL WAITING."}}));
        let assessments: Vec<_> = records
            .iter()
            .filter_map(|record| replay.record(record, threshold()))
            .collect();
        assert_eq!(assessments.len(), 1);
        assert_eq!(
            assessments[0],
            if index == 3 {
                Assessment::NotSuspected
            } else {
                Assessment::Suspected {
                    reason: stall::Suspicion::WaitingFinal,
                    streak: if index == 4 { 1 } else { index },
                    threshold_reached: false,
                }
            }
        );
    }
}

#[test]
fn productive_automatic_calls_preserve_changed_targets_and_outcomes() {
    let mut replay = Replay::default();
    for index in 0..6 {
        let id = format!("turn-{index}");
        let records = [
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal."}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["goal.internal_context"]}}}),
            json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"exec_command","arguments":json!({"cmd":format!("read source --target module-{index} --range {index}:20")}).to_string()}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call","output":json!({"exit_code":0,"output":format!("new source result {index}"),"chunk_id":id,"wall_time_seconds":index}).to_string()}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id,"last_agent_message":"waiting for the next result"}}),
        ];
        assert_eq!(
            records
                .iter()
                .filter_map(|record| replay.record(
                    record,
                    NonZeroU32::new(/*n*/ 2).expect("positive test threshold")
                ))
                .collect::<Vec<_>>(),
            vec![Assessment::NotSuspected]
        );
    }
    assert_eq!(replay.stats.automatic, 6);
}

#[test]
fn counterfactual_origin_keeps_real_inbound_reset_and_compacted_turn_boundary() {
    let mut replay = Replay::counterfactual_auto();
    let records = [
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"first"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"original admission"}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["user.text"]}}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"new actual input"}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["user.text"]}}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"first","last_agent_message":"waiting"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"second"}}),
        json!({"type":"compacted","payload":{}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"second","last_agent_message":"waiting"}}),
    ];
    assert_eq!(
        records
            .iter()
            .filter_map(|record| replay.record(record, threshold()))
            .collect::<Vec<_>>(),
        vec![Assessment::NotSuspected, Assessment::Unclassified]
    );
    assert_eq!(replay.stats.turns, 2);
}

#[test]
fn unsupported_assistant_parts_are_not_empty_final_evidence() {
    for content in [
        json!([{"type":"output_text"}]),
        json!([{"type":"output_audio","audio":"opaque"}]),
    ] {
        let mut replay = Replay::default();
        let records = [
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal."}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["goal.internal_context"]}}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final","content":content}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn","last_agent_message":""}}),
        ];
        assert_eq!(
            records
                .iter()
                .filter_map(|record| replay.record(record, threshold()))
                .collect::<Vec<_>>(),
            vec![Assessment::Unclassified]
        );
    }
}

#[test]
fn qualified_opaque_wrappers_remain_unknown() {
    let mut replay = Replay::default();
    for index in 0..4 {
        let id = format!("turn-{index}");
        let records = [
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal."}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["goal.internal_context"]}}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":"call","name":"functions.exec","input":"opaque_work()"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call","output":"pending"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id,"last_agent_message":"waiting"}}),
        ];
        assert_eq!(
            records
                .iter()
                .filter_map(|record| replay.record(record, threshold()))
                .collect::<Vec<_>>(),
            vec![Assessment::Unclassified]
        );
    }
}
