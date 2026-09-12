//! Integration coverage for automatic shake at the pre-sampling point.
//!
//! Follows the `artifact_recovery` pattern: a wiremock Responses server, an
//! injected history with one oversized tool output, and assertions on the exact
//! request bodies the model saw.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use std::fs;

/// Marker planted in the oversized tool output. Auto-shake must replace it with a
/// recovery placeholder before the next sampling request.
const AUTO_SHAKE_MARKER: &str = "AUTO_SHAKE_ELIDE_ME";

/// Resolved context window for the test model. 60% (the gpt-5.6 default
/// threshold) is 60_000; auto-compaction would not fire until 90_000, so the
/// 70_000-token usage reported below isolates the auto-shake path.
const TEST_CONTEXT_WINDOW: i64 = 100_000;

fn ev_completed_with_input_tokens(id: &str, input_tokens: i64) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": null,
                "output_tokens": 10,
                "output_tokens_details": null,
                "total_tokens": input_tokens + 10
            }
        }
    })
}

fn oversized_history() -> Result<Vec<ResponseItem>> {
    // A large tool output, then a large plain-text assistant tail. Plain text is
    // never elided, so the tail both survives and pushes the tool output past
    // the protected recent window (`MANUAL_PROTECT_TOKENS`).
    Ok(vec![
        serde_json::from_value(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "run the big command"}]
        }))?,
        serde_json::from_value(json!({
            "type": "function_call",
            "id": "call-item",
            "call_id": "auto-shake-exec",
            "name": "exec_command",
            "arguments": "{}"
        }))?,
        serde_json::from_value(json!({
            "type": "function_call_output",
            "call_id": "auto-shake-exec",
            "output": format!("{AUTO_SHAKE_MARKER}\n").repeat(/*n*/ 8_000)
        }))?,
        serde_json::from_value(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "tail context ".repeat(/*n*/ 2_000)}]
        }))?,
    ])
}

/// With auto-shake enabled (the gpt-5.6 default) the second turn's request must
/// no longer carry the oversized tool output, and a recovery artifact must exist.
/// With auto-shake disabled the same thread keeps the full output.
#[test_case::test_case(true; "enabled")]
#[test_case::test_case(false; "disabled")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_shake_elides_before_the_next_sampling_request(enabled: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    // Turn 1 reports usage above the 60% threshold; turn 2 is where auto-shake
    // decides, because pre-sampling reads the usage the previous turn recorded.
    let request_log = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("m1", "first"),
                ev_completed_with_input_tokens("r1", /*input_tokens*/ 70_000),
            ]),
            sse(vec![
                ev_assistant_message("m2", "second"),
                ev_completed_with_input_tokens("r2", /*input_tokens*/ 70_000),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex()
        .with_model("gpt-5.6-luna")
        .with_config(move |config| {
            config.model_context_window = Some(TEST_CONTEXT_WINDOW);
            if !enabled {
                // Global override must win over the built-in gpt-5.6 default
                // (which defers to the global value via `inherit`).
                config.auto_shake.threshold =
                    Some(codex_config::config_toml::AutoShakeThresholdToml::Off);
            }
        });
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    Box::pin(fixture.codex.inject_response_items(oversized_history()?)).await?;

    for message in ["first prompt", "second prompt"] {
        Box::pin(
            fixture
                .codex
                .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                    text: message.to_string(),
                    text_elements: Vec::new(),
                }])),
        )
        .await?;
        if enabled && message == "second prompt" {
            // The auto marker distinguishes this from a manual `/shake` notice.
            Box::pin(wait_for_event(&fixture.codex, |event| {
                matches!(event, EventMsg::Warning(warning)
                    if warning.message.starts_with("⛭ shake (auto):"))
            }))
            .await;
        }
        Box::pin(wait_for_event(&fixture.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        }))
        .await;
    }

    let requests = request_log.requests();
    let bodies: Vec<String> = requests
        .iter()
        .map(|request| request.body_json().to_string())
        .collect();
    assert_eq!(
        bodies.len(),
        2,
        "auto-shake must not add a model request, and must not trigger compaction"
    );
    assert!(
        bodies[0].contains(AUTO_SHAKE_MARKER),
        "the first turn should still carry the full tool output"
    );

    let artifacts = fixture
        .codex_home_path()
        .join("artifacts")
        .join(fixture.session_configured.thread_id.to_string());
    if enabled {
        assert!(
            !bodies[1].contains(AUTO_SHAKE_MARKER),
            "auto-shake should have elided the tool output before the second request"
        );
        assert!(
            bodies[1].contains("recover: artifact://"),
            "the elided region should be replaced by a recovery placeholder"
        );
        let saved = fs::read_dir(&artifacts)?.count();
        assert!(saved > 0, "auto-shake must save a recoverable artifact");
    } else {
        assert!(
            bodies[1].contains(AUTO_SHAKE_MARKER),
            "auto-shake disabled: the tool output must survive"
        );
        assert!(
            !artifacts.exists(),
            "auto-shake disabled: no artifacts should be written"
        );
    }
    Ok(())
}
