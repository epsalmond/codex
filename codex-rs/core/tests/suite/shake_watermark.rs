use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::TurnInputRequest;
use codex_history::RolloutItem;
use codex_history::ShakeHistoryState;
use codex_protocol::models::ExecutedToolCall;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::ToolResultMetadata;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::start_websocket_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

const SEALED_TEST_IMAGE: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

fn edit_sealed_checkpoint_image(rollout_path: &Path) -> Result<()> {
    let contents = fs::read_to_string(rollout_path)?;
    let mut edited = false;
    let mut lines = Vec::new();
    for line in contents.lines() {
        let mut item: Value = serde_json::from_str(line)?;
        if item["type"] == "compacted"
            && item["payload"]["shake_history_state"].is_object()
            && let Some(history) = item["payload"]["replacement_history"].as_array_mut()
        {
            'history: for envelope in history {
                let Some(content) = envelope["content"].as_array_mut() else {
                    continue;
                };
                for content_item in content {
                    if content_item["type"] != "input_image" {
                        continue;
                    }
                    if content_item.get("image_url").is_some() {
                        content_item["image_url"] =
                            json!("https://example.invalid/edited-after-seal.png");
                        edited = true;
                        break 'history;
                    }
                    if content_item.get("file_id").is_some() {
                        content_item["file_id"] = json!("edited-after-seal");
                        edited = true;
                        break 'history;
                    }
                }
            }
        }
        lines.push(serde_json::to_string(&item)?);
    }
    anyhow::ensure!(edited, "checkpoint should contain sealed image media");
    let mut rewritten = lines.join("\n");
    rewritten.push('\n');
    fs::write(rollout_path, rewritten)?;
    Ok(())
}

fn latest_shake_state(rollout_path: &Path) -> Result<ShakeHistoryState> {
    fs::read_to_string(rollout_path)?
        .lines()
        .filter_map(|line| serde_json::from_str::<RolloutItem>(line).ok())
        .filter_map(|item| match item {
            RolloutItem::Compacted(compacted) => compacted.shake_history_state,
            _ => None,
        })
        .next_back()
        .context("rollout should contain a persisted shake watermark")
}

fn rollout_path(codex: &CodexThread) -> Result<PathBuf> {
    codex.rollout_path().context("rollout path")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_preserves_a_verified_shake_seal_after_media_preparation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(start_mock_server()).await;
    let mut builder = test_codex().with_model("gpt-5.2");
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    let history = vec![
        serde_json::from_value(json!({
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "keep the sealed media boundary"},
                {"type": "input_image", "image_url": SEALED_TEST_IMAGE}
            ]
        }))?,
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
    let original_state = latest_shake_state(&rollout_path(&fixture.codex)?)?;
    assert!(original_state.watermark > 0);

    let resumed = Box::pin(builder.restart(&server, &fixture))
        .await
        .context("resume a checkpoint containing sealed remote media")?;
    let resumed_preview = resumed.codex.preview_shake(ShakeMode::Elide).await?;
    Box::pin(resumed.codex.submit(Op::Shake {
        mode: ShakeMode::Elide,
        expected_fingerprint: Some(resumed_preview.fingerprint),
    }))
    .await?;
    Box::pin(wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;
    let rebased_state = latest_shake_state(&rollout_path(&resumed.codex)?)?;
    assert_eq!(rebased_state, original_state);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_rejects_an_edited_sealed_media_checkpoint() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(start_mock_server()).await;
    let mut builder = test_codex().with_model("gpt-5.2");
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    let history = vec![
        serde_json::from_value(json!({
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "keep the sealed media boundary"},
                {"type": "input_image", "image_url": SEALED_TEST_IMAGE}
            ]
        }))?,
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

    let path = rollout_path(&fixture.codex)?;
    let sealed_state = latest_shake_state(&path)?;
    assert!(sealed_state.watermark > 0);
    fixture.codex.ensure_rollout_materialized().await;
    fixture.codex.flush_rollout().await?;
    fixture.codex.shutdown_and_wait().await?;
    edit_sealed_checkpoint_image(&path)?;
    anyhow::ensure!(
        fs::metadata(&path)?.len() > 0,
        "edited checkpoint must not be empty"
    );
    assert_eq!(latest_shake_state(&path)?, sealed_state);

    let resumed = Box::pin(builder.resume(&server, fixture.home.clone(), path)).await?;
    let preview_error = resumed
        .codex
        .preview_shake(ShakeMode::Elide)
        .await
        .expect_err("the edited checkpoint must not be accepted under its old digest");
    assert!(
        preview_error
            .to_string()
            .contains("Stored Shake history seal is invalid")
    );

    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_full_request_preserves_the_sealed_prefix() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_websocket_server(vec![vec![
        vec![ev_response_created("warm-1"), ev_completed("warm-1")],
        vec![
            ev_response_created("response-1"),
            ev_assistant_message("message-1", "the sealed prefix was accepted"),
            ev_completed("response-1"),
        ],
    ]])
    .await;
    let mut builder = test_codex().with_model("gpt-5.4");
    let fixture = Box::pin(builder.build_with_websocket_server(&server)).await?;

    let user_message: ResponseItem = serde_json::from_value(json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": "seal this tool metadata"}]
    }))?;
    let function_call: ResponseItem = serde_json::from_value(json!({
        "type": "function_call",
        "id": "sealed-call-item",
        "call_id": "sealed-tool-metadata",
        "name": "exec_command",
        "arguments": "{}"
    }))?;
    let mut output = ResponseItem::from(ResponseInputItem::FunctionCallOutput {
        call_id: "sealed-tool-metadata".to_string(),
        output: FunctionCallOutputPayload::from_text("small visible result".to_string()),
    });
    let mut executed_call = ExecutedToolCall::new("exec_command".to_string(), json!({}));
    executed_call.set_tool_result_metadata(ToolResultMetadata::new(&json!({"sealed": true})));
    output.append_executed_tool_calls(vec![executed_call]);
    output.mark_tool_calls_complete();
    let mutable_tail = serde_json::from_value(json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "mutable tail ".repeat(6_000)}]
    }))?;
    Box::pin(fixture.codex.inject_response_items(vec![
        user_message,
        function_call,
        output,
        mutable_tail,
    ]))
    .await?;

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
    let sealed_state = latest_shake_state(&rollout_path(&fixture.codex)?)?;
    let stored_history = fixture
        .codex
        .load_history(/*include_archived*/ false)
        .await?;
    let replacement_history = stored_history
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::Compacted(compacted) => compacted.replacement_history.as_ref(),
            _ => None,
        })
        .context("shake should persist replacement history")?;
    let output_index = replacement_history
        .iter()
        .position(|envelope| {
            matches!(
                &envelope.item,
                ResponseItem::FunctionCallOutput { call_id: Some(call_id), .. }
                    if call_id == "sealed-tool-metadata"
            )
        })
        .expect("sealed tool output is in history");
    assert!(usize::try_from(sealed_state.watermark)? > output_index);

    fixture
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "this is a new delta after W".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let result = Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    }))
    .await;
    assert!(matches!(result, EventMsg::TurnComplete(_)));
    let requests = server.single_connection();
    assert_eq!(requests.len(), 2);
    let generation = requests[1].body_json();
    let _input = generation["input"].as_array().expect("full request input");
    let serialized_input = generation["input"].to_string();
    assert!(serialized_input.contains("seal this tool metadata"));
    assert!(serialized_input.contains("sealed-tool-metadata"));
    assert!(serialized_input.contains("exec_command"));
    assert!(serialized_input.contains("this is a new delta after W"));

    fixture.codex.shutdown_and_wait().await?;
    server.shutdown().await;
    Ok(())
}
