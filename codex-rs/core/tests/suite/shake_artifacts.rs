use anyhow::Context;
use anyhow::Result;
use codex_core::ForkSnapshot;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ShakeMode;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;

/// Regular (non-hidden) `.log` files directly under `dir`, or an empty `Vec`
/// if `dir` does not exist. Used instead of `Path::exists` because
/// `ArtifactStore::for_thread` may create the (empty) per-thread directory as
/// a side effect of canonicalizing its root — even for a preview, or for an
/// ephemeral thread — so directory *existence* no longer implies a shake
/// wrote anything.
fn artifact_log_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .collect()
}

/// A shake writes each elided region to a durable file and replaces it with a
/// placeholder naming that file's absolute path — there is no tool to recover
/// it through (see `docs/shake.md`). This proves the file lands where the
/// placeholder says, holds the original bytes, and survives a resume and a
/// fork.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shake_writes_a_durable_artifact_at_the_placeholder_path() -> Result<()> {
    skip_if_no_network!(Ok(()));

    run_shake_artifact_durability().await
}

fn run_shake_artifact_durability() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        let server = Box::pin(core_test_support::responses::start_mock_server()).await;
        let mut builder = test_codex().with_model("gpt-5.2");
        let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
        let tail = "tail context ".repeat(/*n*/ 2_000);
        let history: Vec<ResponseItem> = vec![
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "produce recoverable output"}]
            }),
            json!({
                "type": "function_call",
                "id": "call-item",
                "call_id": "artifact-exec",
                "name": "exec_command",
                "arguments": "{}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "artifact-exec",
                "output": "ARTIFACT_RECOVERY_MARKER\n".repeat(/*n*/ 2_000)
            }),
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": tail}]
            }),
        ]
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()?;
        Box::pin(fixture.codex.inject_response_items(history)).await?;

        let artifact_dir = fixture
            .codex_home_path()
            .join("artifacts")
            .join(fixture.session_configured.thread_id.to_string());
        let usage_before = fixture.codex.token_usage_info().await;
        let preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
        assert_eq!((preview.tool_outputs, preview.text_blocks), (1, 0));
        assert!(preview.tokens_before > preview.tokens_after);
        assert!(artifact_log_files(&artifact_dir).is_empty());
        assert_eq!(fixture.codex.token_usage_info().await, usage_before);
        assert_eq!(
            fixture.codex.preview_shake(ShakeMode::Elide).await?,
            preview
        );

        // Even with unchanged history, confirming a different mode is stale.
        let other_mode = fixture.codex.preview_shake(ShakeMode::Images).await?;
        Box::pin(fixture.codex.submit(Op::Shake {
            mode: ShakeMode::Elide,
            expected_fingerprint: Some(other_mode.fingerprint),
        }))
        .await?;
        Box::pin(wait_for_event(&fixture.codex, |event| {
            matches!(event, EventMsg::Warning(warning) if warning.message.contains("History changed"))
        })).await;
        assert!(artifact_log_files(&artifact_dir).is_empty());
        assert_eq!(
            fixture.codex.preview_shake(ShakeMode::Elide).await?,
            preview
        );

        Box::pin(fixture.codex.submit(Op::Shake {
            mode: ShakeMode::Elide,
            expected_fingerprint: Some(preview.fingerprint),
        }))
        .await?;
        let warning = Box::pin(wait_for_event(
        &fixture.codex,
        |event| matches!(event, EventMsg::Warning(warning) if warning.message.contains("shake:")),
    ))
    .await;
        assert!(matches!(warning, EventMsg::Warning(_)));

        let after = fixture.codex.preview_shake(ShakeMode::Elide).await?;
        assert_eq!(after.tokens_before, preview.tokens_after);
        assert_eq!((after.tool_outputs, after.text_blocks), (0, 0));
        let artifact_path = fs::read_dir(&artifact_dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "log"))
            .context("shake should save an artifact")?;
        assert!(fs::read_to_string(&artifact_path)?.contains("ARTIFACT_RECOVERY_MARKER"));
        let artifact_path_str = artifact_path
            .canonicalize()
            .context("artifact path should be resolvable")?
            .display()
            .to_string();

        // The placeholder that replaced the elided output must name this
        // exact file, so a model that only needs part of it can reach for a
        // shell tool directly.
        let response = mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("artifact-r4"),
                ev_assistant_message("artifact-m2", "acknowledged"),
                ev_completed("artifact-r4"),
            ]),
        )
        .await;
        Box::pin(fixture.submit_text_turn("what happened to that output")).await?;
        let request = response.single_request();
        assert!(request.body_contains_text(&artifact_path_str));
        assert!(!request.body_contains_text("ARTIFACT_RECOVERY_MARKER"));

        let resumed = Box::pin(builder.restart(&server, &fixture))
            .await
            .context("restart after shake")?;
        assert!(
            fs::read_to_string(&artifact_path)?.contains("ARTIFACT_RECOVERY_MARKER"),
            "the artifact file must survive a resume"
        );
        let resumed_response = mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("artifact-r6"),
                ev_assistant_message("artifact-m3", "resumed"),
                ev_completed("artifact-r6"),
            ]),
        )
        .await;
        Box::pin(resumed.submit_text_turn("still there after resume")).await?;
        assert!(
            resumed_response
                .single_request()
                .body_contains_text(&artifact_path_str)
        );

        let source_rollout = resumed
            .codex
            .rollout_path()
            .context("source rollout for fork")?;
        let mut ephemeral_fork_config = resumed.config.clone();
        ephemeral_fork_config.ephemeral = true;
        let ephemeral_fork = Box::pin(resumed.thread_manager.fork_thread(
            ForkSnapshot::Interrupted,
            ephemeral_fork_config,
            source_rollout.clone(),
            /*thread_source*/ None,
            /*parent_trace*/ None,
        ))
        .await?;
        let ephemeral_fork_artifacts = resumed
            .codex_home_path()
            .join("artifacts")
            .join(ephemeral_fork.thread_id.to_string());
        assert!(!ephemeral_fork_artifacts.exists());
        Box::pin(ephemeral_fork.thread.shutdown_and_wait()).await?;

        let forked = Box::pin(resumed.thread_manager.fork_thread(
            ForkSnapshot::Interrupted,
            resumed.config.clone(),
            source_rollout,
            /*thread_source*/ None,
            /*parent_trace*/ None,
        ))
        .await?;
        let fork_artifacts = resumed
            .codex_home_path()
            .join("artifacts")
            .join(forked.thread_id.to_string());
        let fork_artifact = fs::read_dir(fork_artifacts)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "log"))
            .context("fork should copy parent artifacts")?;
        assert!(fs::read_to_string(fork_artifact)?.contains("ARTIFACT_RECOVERY_MARKER"));
        Box::pin(forked.thread.shutdown_and_wait()).await?;
        Ok(())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ephemeral_shake_elide_preserves_history_without_artifacts() -> Result<()> {
    skip_if_no_network!(Ok(()));

    run_ephemeral_artifact_boundary().await
}

fn run_ephemeral_artifact_boundary() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        let server = Box::pin(core_test_support::responses::start_mock_server()).await;
        let mut builder = test_codex()
            .with_model("gpt-5.2")
            .with_config(|config| config.ephemeral = true);
        let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
        let history: Vec<ResponseItem> = vec![
            json!({
                "type": "function_call",
                "id": "ephemeral-call-item",
                "call_id": "ephemeral-call",
                "name": "exec_command",
                "arguments": "{}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "ephemeral-call",
                "output": "EPHEMERAL_RECOVERY_MARKER\n".repeat(/*n*/ 2_000)
            }),
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "ephemeral tail ".repeat(/*n*/ 2_000)
                }]
            }),
        ]
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()?;
        Box::pin(fixture.codex.inject_response_items(history)).await?;

        let preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
        assert!(preview.unavailable_reason.is_some());
        assert_eq!(preview.tokens_before, preview.tokens_after);

        Box::pin(fixture.codex.submit(Op::Shake {
            mode: ShakeMode::Elide,
            expected_fingerprint: None,
        }))
        .await?;
        let warning = Box::pin(wait_for_event(&fixture.codex, |event| {
            matches!(event, EventMsg::Warning(warning) if warning.message.contains("ephemeral thread"))
        }))
        .await;
        let EventMsg::Warning(warning) = warning else {
            panic!("expected ephemeral shake warning");
        };
        assert!(warning.message.contains("persistent threads"));

        let artifact_dir = fixture
            .codex_home_path()
            .join("artifacts")
            .join(fixture.session_configured.thread_id.to_string());
        assert!(artifact_log_files(&artifact_dir).is_empty());

        let response = mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("ephemeral-r1"),
                ev_assistant_message("ephemeral-m1", "done"),
                ev_completed("ephemeral-r1"),
            ]),
        )
        .await;
        Box::pin(fixture.submit_text_turn("inspect retained history")).await?;
        let request = response.single_request();
        assert!(request.body_contains_text("EPHEMERAL_RECOVERY_MARKER"));
        Ok(())
    })
}

#[test_case::test_case(ShakeMode::Images; "images")]
#[test_case::test_case(ShakeMode::Thinking; "thinking")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shake_preview_measures_non_text_modes(mode: ShakeMode) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let fixture = Box::pin(test_codex().build_with_auto_env(&server)).await?;
    let items = vec![
        serde_json::from_value(json!({
            "type":"message", "role":"user", "content":[
                {"type":"input_text", "text":"inspect this"},
                {"type":"input_image", "image_url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="}
            ]
        }))?,
        serde_json::from_value(json!({
            "type":"reasoning", "summary":[],
            "content":[{"type":"reasoning_text", "text":"reasoning text ".repeat(/*n*/ 1_000)}]
        }))?,
    ];
    Box::pin(fixture.codex.inject_response_items(items)).await?;
    let before = fixture.codex.preview_shake(mode).await?;
    assert_eq!(
        (before.images, before.thinking_blocks),
        match mode {
            ShakeMode::Images => (1, 0),
            ShakeMode::Thinking => (0, 1),
            ShakeMode::Elide | ShakeMode::SmartCompact => unreachable!(),
        }
    );
    assert!(before.tokens_before > before.tokens_after);
    assert_eq!(fixture.codex.preview_shake(mode).await?, before);
    Box::pin(fixture.codex.submit(Op::Shake {
        mode,
        expected_fingerprint: Some(before.fingerprint),
    }))
    .await?;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    })).await;
    let after = fixture.codex.preview_shake(mode).await?;
    assert_eq!(
        (after.tokens_before, after.tokens_after),
        (before.tokens_after, before.tokens_after)
    );
    assert!(
        server
            .received_requests()
            .await
            .context("failed to read mock server requests")?
            .is_empty()
    );
    Ok(())
}

/// Numeric cross-check, in the pattern of oh-my-pi's `tokensFreed` vs.
/// `getContextUsage()` delta assertion: after a real (non-preview) shake, the
/// thread's own reported token usage must drop by approximately the number of
/// tokens the shake's preview said it would free. The fork's tests otherwise
/// only confirm a shake *ran* (marker gone, artifact written); this confirms
/// the resulting usage numbers are actually correct.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shake_elide_drops_reported_token_usage_by_the_freed_estimate() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let fixture = Box::pin(test_codex().build_with_auto_env(&server)).await?;

    // A large elidable tool output plus a plain-text tail (never elided, so it
    // both survives and pushes the tool output past `MANUAL_PROTECT_TOKENS`),
    // and one reasoning item used only to seed a same-scale "before" usage
    // reading below.
    let history: Vec<ResponseItem> = vec![
        json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "produce recoverable output"}]
        }),
        json!({
            "type": "function_call",
            "id": "call-item",
            "call_id": "usage-cross-check-exec",
            "name": "exec_command",
            "arguments": "{}"
        }),
        json!({
            "type": "function_call_output",
            "call_id": "usage-cross-check-exec",
            "output": "USAGE_CROSS_CHECK_MARKER\n".repeat(/*n*/ 2_000)
        }),
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "tail context ".repeat(/*n*/ 2_000)}]
        }),
        json!({
            "type": "reasoning", "summary": [],
            "content": [{"type": "reasoning_text", "text": "throwaway reasoning"}]
        }),
    ]
    .into_iter()
    .map(serde_json::from_value)
    .collect::<Result<_, _>>()?;
    Box::pin(fixture.codex.inject_response_items(history)).await?;

    // `token_usage_info` is only populated by a real (non-preview) shake or a
    // completed turn. Dropping the throwaway reasoning item via a real
    // Thinking-mode shake gives a "before" reading on the exact same
    // estimation scale (`recompute_token_usage`) as the "after" reading below,
    // without touching any of the Elide-eligible content measured next.
    let seed = fixture.codex.preview_shake(ShakeMode::Thinking).await?;
    Box::pin(fixture.codex.submit(Op::Shake {
        mode: ShakeMode::Thinking,
        expected_fingerprint: Some(seed.fingerprint),
    }))
    .await?;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;
    let usage_before = fixture
        .codex
        .token_usage_info()
        .await
        .context("usage should be populated after the seeding shake")?;

    let preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    let expected_freed = preview.tokens_before - preview.tokens_after;
    assert!(
        expected_freed > 0,
        "the shake must free tokens for this cross-check to mean anything"
    );

    Box::pin(fixture.codex.submit(Op::Shake {
        mode: ShakeMode::Elide,
        expected_fingerprint: Some(preview.fingerprint),
    }))
    .await?;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;
    let usage_after = fixture
        .codex
        .token_usage_info()
        .await
        .context("usage should be populated after the elide shake")?;

    let actual_drop =
        usage_before.last_token_usage.total_tokens - usage_after.last_token_usage.total_tokens;
    // Both readings come from `recompute_token_usage`, which sums the same
    // per-item estimator the preview uses (plus a constant base-instructions
    // offset that cancels out in the delta), so this should land almost
    // exactly on the preview's own freed estimate.
    let tolerance = (expected_freed / 20).max(50);
    assert!(
        (actual_drop - expected_freed).abs() <= tolerance,
        "expected usage to drop by ~{expected_freed} tokens (+/- {tolerance}), got {actual_drop}"
    );
    Ok(())
}
