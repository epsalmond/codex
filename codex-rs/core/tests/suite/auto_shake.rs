//! Integration coverage for automatic shake at the pre-sampling point.
//!
//! Follows the `shake_artifacts` pattern: a wiremock Responses server, an
//! injected history with one oversized tool output, and assertions on the exact
//! request bodies the model saw.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ShakeMode;
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
    // the protected recent window (`AUTO_PROTECT_TOKENS`, 16_000 tokens for the
    // automatic trigger under test here).
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
            // "tail context " is 13 bytes; 6_000 reps ~= 19_500 tokens at 4
            // bytes/token, comfortably past `AUTO_PROTECT_TOKENS` (16_000) so
            // the oversized tool output above still falls outside the
            // protected tail.
            "content": [{"type": "output_text", "text": "tail context ".repeat(/*n*/ 6_000)}]
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
            bodies[1].contains("[shaken ~"),
            "the elided region should be replaced by a shaken placeholder"
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

/// Same history shape as above, but the oversized output belongs to a
/// protected tool (`skills.read`). Auto-shake must leave it alone, and the
/// manual preview must not count it — otherwise `/shake` would promise savings
/// a shake cannot deliver. Mirrors oh-my-pi's `protectedTools`, which is
/// checked before elision in both the default and aggressive presets.
#[test_case::test_case(Some("skills"), "read"; "skill read")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_shake_never_elides_a_protected_tool_output(
    namespace: Option<&'static str>,
    tool_name: &'static str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
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
        .with_config(move |config| config.model_context_window = Some(TEST_CONTEXT_WINDOW));
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    Box::pin(
        fixture
            .codex
            .inject_response_items(protected_history(namespace, tool_name)?),
    )
    .await?;

    // The manual preview must report nothing to elide either.
    let preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    assert_eq!(
        (preview.tool_outputs, preview.text_blocks),
        (0, 0),
        "a protected tool output must not be counted by the preview"
    );
    assert_eq!(
        preview.tokens_after, preview.tokens_before,
        "a preview that counts nothing must report no savings"
    );

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
        Box::pin(wait_for_event(&fixture.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        }))
        .await;
    }

    let bodies: Vec<String> = request_log
        .requests()
        .iter()
        .map(|request| request.body_json().to_string())
        .collect();
    assert_eq!(bodies.len(), 2, "auto-shake must not add a model request");
    for (index, body) in bodies.iter().enumerate() {
        assert!(
            body.contains(AUTO_SHAKE_MARKER),
            "request {index} must still carry the protected tool output"
        );
    }
    let artifacts = fixture
        .codex_home_path()
        .join("artifacts")
        .join(fixture.session_configured.thread_id.to_string());
    // A preview or a considered-but-skipped shake pass may still create the
    // (empty) per-thread artifact directory as a side effect of resolving its
    // canonical path; what must not happen is an actual artifact file.
    assert!(
        artifact_log_files(&artifacts).is_empty(),
        "nothing was elided, so no artifact should be written"
    );
    Ok(())
}

/// Regular (non-hidden) `.log` files directly under `dir`, or an empty `Vec`
/// if `dir` does not exist. Used instead of `Path::exists` because
/// `ArtifactStore::for_thread` may create the (empty) directory as a side
/// effect of canonicalizing its root, even when no shake ever saves anything
/// into it.
fn artifact_log_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .collect()
}

/// `oversized_history` with the tool call renamed to a protected tool. The
/// output carries no tool name of its own (that is how real history looks), so
/// protection has to resolve it through the call's `call_id`.
fn protected_history(namespace: Option<&str>, tool_name: &str) -> Result<Vec<ResponseItem>> {
    let mut call = json!({
        "type": "function_call",
        "id": "call-item",
        "call_id": "auto-shake-protected",
        "name": tool_name,
        "arguments": "{}"
    });
    if let Some(namespace) = namespace {
        call["namespace"] = json!(namespace);
    }
    Ok(vec![
        serde_json::from_value(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "load the protected thing"}]
        }))?,
        serde_json::from_value(call)?,
        serde_json::from_value(json!({
            "type": "function_call_output",
            "call_id": "auto-shake-protected",
            "output": format!("{AUTO_SHAKE_MARKER}\n").repeat(/*n*/ 8_000)
        }))?,
        serde_json::from_value(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "tail context ".repeat(/*n*/ 6_000)}]
        }))?,
    ])
}

/// Old tool output eliminated by the plain automatic pass (`AUTO_PROTECT_TOKENS`
/// protect window). Byte sizes below are tuned so that, after removing this,
/// `ESCALATION_MID_MARKER`'s output is still inside the automatic 16_000-token
/// protect window (so the first pass must leave it alone) but outside the
/// escalated pass's smaller `MANUAL_PROTECT_TOKENS` window (so the escalated
/// pass elides it too). See `escalation_history` for the exact math.
const ESCALATION_OLD_MARKER: &str = "OLD_SHAKE_MARKER";

/// Mid-aged tool output that only the escalated (second, aggressive) pass
/// should elide.
const ESCALATION_MID_MARKER: &str = "MID_ESCALATION_MARKER";

/// Resolved context window for the escalation test. Deliberately much smaller
/// than `TEST_CONTEXT_WINDOW`: `AUTO_PROTECT_TOKENS` (16_000) is an absolute
/// token count, not a share of the window, so a small window is what makes the
/// protected tail alone able to clear the 60% threshold after just one pass.
const ESCALATION_CONTEXT_WINDOW: i64 = 24_000;

/// A history shaped so that:
/// - The plain automatic pass (protect 16_000) elides `ESCALATION_OLD_MARKER`'s
///   output only (~36_125 tokens), because `ESCALATION_MID_MARKER`'s output
///   plus the tail sum to ~18_123 tokens after it, which is >= 16_000 tokens of
///   protected trailing window and so make the *old* output eligible in turn.
/// - `ESCALATION_MID_MARKER`'s output sits ~6_013 tokens from the end (just the
///   tail behind it), which is inside the automatic 16_000-token protect
///   window (untouched by the first pass) but outside the escalated pass's
///   4_000-token (`MANUAL_PROTECT_TOKENS`) window, so only the escalated pass
///   elides it.
/// - After the first pass, ~18_213 tokens remain (mid + tail + one
///   placeholder) — still above `ESCALATION_CONTEXT_WINDOW`'s 60% threshold
///   (14_400), so escalation must run.
/// - After the escalated pass, ~6_163 tokens remain (just the tail and two
///   placeholders) — comfortably under the ~90% auto-compaction limit
///   (21_600), so compaction must not fire.
fn escalation_history() -> Result<Vec<ResponseItem>> {
    Ok(vec![
        serde_json::from_value(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "run the big command, then a smaller one"}]
        }))?,
        serde_json::from_value(json!({
            "type": "function_call",
            "id": "call-old",
            "call_id": "escalation-old-exec",
            "name": "exec_command",
            "arguments": "{}"
        }))?,
        serde_json::from_value(json!({
            "type": "function_call_output",
            "call_id": "escalation-old-exec",
            "output": format!("{ESCALATION_OLD_MARKER}\n").repeat(/*n*/ 8_500)
        }))?,
        serde_json::from_value(json!({
            "type": "function_call",
            "id": "call-mid",
            "call_id": "escalation-mid-exec",
            "name": "exec_command",
            "arguments": "{}"
        }))?,
        serde_json::from_value(json!({
            "type": "function_call_output",
            "call_id": "escalation-mid-exec",
            "output": format!("{ESCALATION_MID_MARKER}\n").repeat(/*n*/ 2_200)
        }))?,
        serde_json::from_value(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "tail context ".repeat(/*n*/ 1_850)}]
        }))?,
    ])
}

/// management-plane#988: when the plain automatic pass leaves the thread still
/// above the auto-shake threshold, one escalated pass (smaller
/// `MANUAL_PROTECT_TOKENS` protect window, halved `min_savings_tokens`) must
/// run before falling through to full auto-compaction. Exercises
/// `escalation_history`, tuned so a single pass cannot clear the threshold but
/// two passes together do, and compaction never fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_shake_escalates_when_the_first_pass_is_not_enough() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let request_log = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("m1", "first"),
                ev_completed_with_input_tokens("r1", /*input_tokens*/ 20_000),
            ]),
            sse(vec![
                ev_assistant_message("m2", "second"),
                ev_completed_with_input_tokens("r2", /*input_tokens*/ 20_000),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex()
        .with_model("gpt-5.6-luna")
        .with_config(move |config| {
            config.model_context_window = Some(ESCALATION_CONTEXT_WINDOW);
        });
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    Box::pin(fixture.codex.inject_response_items(escalation_history()?)).await?;

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
        if message == "second prompt" {
            // Both passes run back-to-back before the second sampling request,
            // so both warnings must land before `TurnComplete`.
            Box::pin(wait_for_event(&fixture.codex, |event| {
                matches!(event, EventMsg::Warning(warning)
                    if warning.message.starts_with("⛭ shake (auto):"))
            }))
            .await;
            Box::pin(wait_for_event(&fixture.codex, |event| {
                matches!(event, EventMsg::Warning(warning)
                    if warning.message.starts_with("⛭ shake (auto, escalated):"))
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
        "two shake passes must not add a model request, and must not trigger compaction"
    );
    assert!(
        bodies[0].contains(ESCALATION_OLD_MARKER) && bodies[0].contains(ESCALATION_MID_MARKER),
        "the first turn should still carry both full tool outputs"
    );
    assert!(
        !bodies[1].contains(ESCALATION_OLD_MARKER),
        "the plain automatic pass should have elided the old tool output"
    );
    assert!(
        !bodies[1].contains(ESCALATION_MID_MARKER),
        "the escalated pass should have elided the mid tool output the plain pass left alone"
    );
    let shaken_placeholders = bodies[1].matches("[shaken ~").count();
    assert_eq!(
        shaken_placeholders, 2,
        "both elided regions (one per shake pass) should carry a shaken placeholder"
    );

    let artifacts = fixture
        .codex_home_path()
        .join("artifacts")
        .join(fixture.session_configured.thread_id.to_string());
    let saved = fs::read_dir(&artifacts)?.count();
    assert_eq!(
        saved, 2,
        "one artifact per shake pass (old tool output, then mid tool output)"
    );
    Ok(())
}
