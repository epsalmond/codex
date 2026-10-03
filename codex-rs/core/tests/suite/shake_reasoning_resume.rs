use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::fs;

#[derive(Clone, Copy)]
enum Checkpoint {
    Verified,
    Tampered,
}

#[test_case::test_case(Checkpoint::Verified; "verified seal dispatches")]
#[test_case::test_case(Checkpoint::Tampered; "tampered seal blocks dispatch")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_content_representations_preserve_seals_on_resume(
    checkpoint_kind: Checkpoint,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(start_mock_server()).await;
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("resume-message", "resumed successfully"),
            ev_completed("resume-response"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_model("gpt-5.2");
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    let history = vec![
        serde_json::from_value(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "seal the reasoning history"}]
        }))?,
        ResponseItem::Reasoning {
            id: Some(ResponseItemId::with_suffix("rs", "omitted")),
            summary: Vec::new(),
            content: Some(Vec::new()),
            encrypted_content: Some("synthetic-omitted-reasoning".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Reasoning {
            id: Some(ResponseItemId::with_suffix("rs", "null")),
            summary: Vec::new(),
            content: None,
            encrypted_content: Some("synthetic-null-reasoning".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        serde_json::from_value(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "mutable tail ".repeat(6_000)}]
        }))?,
    ];
    Box::pin(fixture.codex.inject_response_items(history)).await?;
    let preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    Box::pin(fixture.codex.submit(Op::Shake {
        mode: ShakeMode::Elide,
        expected_fingerprint: Some(preview.fingerprint),
    }))
    .await?;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;
    fixture.codex.ensure_rollout_materialized().await;
    fixture.codex.flush_rollout().await?;
    let path = fixture.codex.rollout_path().context("rollout path")?;
    fixture.codex.shutdown_and_wait().await?;
    let contents = fs::read_to_string(&path)?;
    let checkpoint: Value = contents
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["type"] == "compacted")
        .context("persisted Shake checkpoint")?;
    let payload = &checkpoint["payload"];
    let watermark = payload["shake_history_state"]["watermark"]
        .as_u64()
        .context("persisted watermark")?;
    let mut sealed_reasoning = Vec::new();
    for (index, item) in payload["replacement_history"]
        .as_array()
        .context("replacement history")?
        .iter()
        .enumerate()
    {
        if item["id"] == "rs_omitted" || item["id"] == "rs_null" {
            assert!(u64::try_from(index)? < watermark);
            sealed_reasoning.push(item.clone());
        }
    }
    assert_eq!(sealed_reasoning.len(), 2);
    assert!(
        !sealed_reasoning[0]
            .as_object()
            .unwrap()
            .contains_key("content")
    );
    assert_eq!(sealed_reasoning[1].get("content"), Some(&Value::Null));

    if matches!(checkpoint_kind, Checkpoint::Tampered) {
        let mut lines = Vec::new();
        for line in contents.lines() {
            let mut record: Value = serde_json::from_str(line)?;
            if record["type"] == "compacted" {
                for item in record["payload"]["replacement_history"]
                    .as_array_mut()
                    .context("replacement history")?
                {
                    if item["id"] == "rs_omitted" {
                        item["encrypted_content"] = json!("edited-after-seal");
                    }
                }
            }
            lines.push(serde_json::to_string(&record)?);
        }
        fs::write(&path, format!("{}\n", lines.join("\n")))?;
    }

    let resumed = Box::pin(builder.resume(&server, fixture.home.clone(), path)).await?;
    resumed
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "continue after resume".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let event = Box::pin(wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::Error(_) | EventMsg::TurnComplete(_))
    }))
    .await;
    match checkpoint_kind {
        Checkpoint::Verified => {
            assert!(matches!(event, EventMsg::TurnComplete(_)), "{event:?}");
            let input = response.single_request().input();
            let reasoning: Vec<Value> = input
                .into_iter()
                .filter(|item| item["id"] == "rs_omitted" || item["id"] == "rs_null")
                .collect();
            assert_eq!(reasoning, sealed_reasoning);
        }
        Checkpoint::Tampered => {
            let EventMsg::Error(error) = event else {
                panic!("expected a seal validation error, got {event:?}");
            };
            assert!(
                error
                    .message
                    .contains("Shake sealed history state is invalid"),
                "unexpected error: {}",
                error.message
            );
            assert!(response.requests().is_empty());
        }
    }
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}
