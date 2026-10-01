use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::TurnInputRequest;
use codex_history::RolloutItem;
use codex_history::ShakeHistoryState;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::ExecutedToolCall;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::ToolResultMetadata;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_dynamic_inputs_keep_the_shake_watermark_and_start_a_new_cache_epoch() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = Box::pin(start_mock_server()).await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("dynamic-r1"),
                ev_assistant_message("dynamic-m1", "first response"),
                ev_completed("dynamic-r1"),
            ]),
            sse(vec![
                ev_response_created("dynamic-r2"),
                ev_assistant_message("dynamic-m2", "second response"),
                ev_completed("dynamic-r2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.2", |model| {
            model.experimental_supported_tools.clear();
        })
        .with_model_info_override("gpt-5.2-text-only", |model| {
            model.input_modalities = vec![InputModality::Text];
            model.experimental_supported_tools = vec!["request_user_input_async".to_string()];
        })
        .with_model("gpt-5.2")
        .with_config(|config| config.update_plan_enabled = true);
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    let history: Vec<ResponseItem> = serde_json::from_value(json!([
        {
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "the sealed image must stay within the history boundary"},
                {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="}
            ]
        },
        {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "mutable tail ".repeat(6_000)}]
        }
    ]))?;
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
    let state_before = latest_shake_state(&rollout_path(&fixture.codex)?)?;

    fixture
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "first request".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let first_result = Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Error(_) | EventMsg::TurnComplete(_))
    }))
    .await;
    assert!(
        matches!(first_result, EventMsg::TurnComplete(_)),
        "first request failed before a response: {first_result:?}"
    );

    let plan_mode = CollaborationMode {
        mode: ModeKind::Plan,
        settings: Settings {
            model: "gpt-5.2-text-only".to_string(),
            reasoning_effort: None,
            developer_instructions: Some("Changed plan instructions for this request.".to_string()),
        },
    };
    fixture
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "second request with changed tools and modalities".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                collaboration_mode: Some(plan_mode),
                ..Default::default()
            }),
        )
        .await?;
    let second_result = Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Error(_) | EventMsg::TurnComplete(_))
    }))
    .await;
    assert!(
        matches!(second_result, EventMsg::TurnComplete(_)),
        "second request failed before a response: {second_result:?}"
    );

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    let first = requests[0].body_json();
    let second = requests[1].body_json();
    assert_eq!(first["model"], "gpt-5.2");
    assert_eq!(second["model"], "gpt-5.2-text-only");
    let first_input = first["input"].to_string();
    let second_input = second["input"].to_string();
    assert!(!first_input.contains("Changed plan instructions"));
    assert!(second_input.contains("Changed plan instructions"));
    assert_ne!(first["tools"], second["tools"]);
    assert!(first["input"].to_string().contains("input_image"));
    assert!(!second["input"].to_string().contains("input_image"));
    let state_after = latest_shake_state(&rollout_path(&fixture.codex)?)?;
    assert_eq!(state_after, state_before);

    fixture.codex.shutdown_and_wait().await?;
    Ok(())
}
