use anyhow::Context;
use anyhow::Result;
use codex_core::ForkSnapshot;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_history::ShakeHistoryState;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
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
use std::sync::Arc;

/// Regular (non-hidden) `.log` files directly under `dir`, or an empty `Vec`
/// if it does not exist.
fn artifact_log_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .collect()
}

fn shake_states(rollout_path: &Path) -> Result<Vec<ShakeHistoryState>> {
    let mut states = Vec::new();
    let contents = fs::read_to_string(rollout_path)?;
    for line in contents.lines() {
        if let RolloutItem::Compacted(compacted) = serde_json::from_str(line)?
            && let Some(checkpoint_state) = compacted.shake_history_state
        {
            states.push(checkpoint_state);
        }
    }
    Ok(states)
}

fn latest_shake_state(rollout_path: &Path) -> Result<ShakeHistoryState> {
    shake_states(rollout_path)?
        .pop()
        .context("rollout should contain a persisted shake watermark")
}

fn latest_shake_watermark(rollout_path: &Path) -> Result<usize> {
    usize::try_from(latest_shake_state(rollout_path)?.watermark).map_err(Into::into)
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
        let first_watermark = latest_shake_watermark(
            &fixture
                .codex
                .rollout_path()
                .context("rollout path after shake")?,
        )?;
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

        // A no-op mode still seals the untouched tail. The checkpoint must
        // advance W even though no image was present to remove.
        let image_preview = fixture.codex.preview_shake(ShakeMode::Images).await?;
        Box::pin(fixture.codex.submit(Op::Shake {
            mode: ShakeMode::Images,
            expected_fingerprint: Some(image_preview.fingerprint),
        }))
        .await?;
        Box::pin(wait_for_event(&fixture.codex, |event| {
            matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
        }))
        .await;
        let state_after_images = latest_shake_state(
            &fixture
                .codex
                .rollout_path()
                .context("rollout path after Images shake")?,
        )?;
        let watermark_after_images = usize::try_from(state_after_images.watermark)?;
        assert!(watermark_after_images >= first_watermark);

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
        let first_request_input = request.input();

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
        let resumed_request = resumed_response.single_request();
        assert!(resumed_request.body_contains_text(&artifact_path_str));
        let resumed_request_input = resumed_request.input();
        assert!(first_request_input.len() >= watermark_after_images);
        assert!(resumed_request_input.len() >= watermark_after_images);
        assert_eq!(
            &first_request_input[..watermark_after_images],
            &resumed_request_input[..watermark_after_images],
            "the wire prefix through W survives repeated shakes and resume"
        );

        resumed.codex.ensure_rollout_materialized().await;
        resumed
            .codex
            .append_rollout_items(&[RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                ThreadRolledBackEvent { num_turns: 1 },
            ))])
            .await?;
        let rolled_back = Box::pin(builder.restart(&server, &resumed)).await?;
        let rollback_response = mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("artifact-rollback-r1"),
                ev_assistant_message("artifact-rollback-m1", "rolled back"),
                ev_completed("artifact-rollback-r1"),
            ]),
        )
        .await;
        Box::pin(rolled_back.submit_text_turn("continue after rollback")).await?;
        let rollback_request_input = rollback_response.single_request().input();
        assert!(rollback_request_input.len() >= watermark_after_images);
        assert_eq!(
            &resumed_request_input[..watermark_after_images],
            &rollback_request_input[..watermark_after_images],
            "rollback after W preserves the actual Responses prefix through W"
        );

        rolled_back
            .codex
            .submit(Op::ThreadSettings {
                thread_settings: ThreadSettingsOverrides {
                    effort: Some(Some(ReasoningEffort::High)),
                    ..Default::default()
                },
                reply: None,
            })
            .await?;
        let dynamic_response = mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("artifact-dynamic-r1"),
                ev_assistant_message("artifact-dynamic-m1", "reasoning changed"),
                ev_completed("artifact-dynamic-r1"),
            ]),
        )
        .await;
        Box::pin(rolled_back.submit_text_turn("continue after reasoning update")).await?;
        let dynamic_request_input = dynamic_response.single_request().input();
        assert!(dynamic_request_input.len() >= watermark_after_images);
        assert_eq!(
            &rollback_request_input[..watermark_after_images],
            &dynamic_request_input[..watermark_after_images],
            "changing request reasoning starts a new cache epoch but preserves W"
        );

        let source_rollout = rolled_back
            .codex
            .rollout_path()
            .context("source rollout for fork")?;
        let mut ephemeral_fork_config = rolled_back.config.clone();
        ephemeral_fork_config.ephemeral = true;
        let stored_history = rolled_back
            .codex
            .load_history(/*include_archived*/ false)
            .await?;
        let history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: stored_history.thread_id,
            history: Arc::new(stored_history.items),
            rollout_path: Some(source_rollout.clone()),
            last_activity_at: None,
            history_revision: None,
        });
        let ephemeral_fork = Box::pin(rolled_back.thread_manager.fork_thread_from_history(
            ForkSnapshot::Interrupted,
            StartThreadOptions::new(ephemeral_fork_config),
            history.clone(),
        ))
        .await?;
        let ephemeral_fork_artifacts = rolled_back
            .codex_home_path()
            .join("artifacts")
            .join(ephemeral_fork.thread_id.to_string());
        assert!(!ephemeral_fork_artifacts.exists());
        Box::pin(ephemeral_fork.thread.shutdown_and_wait()).await?;

        let forked = Box::pin(rolled_back.thread_manager.fork_thread_from_history(
            ForkSnapshot::Interrupted,
            StartThreadOptions::new(rolled_back.config.clone()),
            history,
        ))
        .await?;
        let fork_artifacts = rolled_back
            .codex_home_path()
            .join("artifacts")
            .join(forked.thread_id.to_string());
        let fork_artifact = fs::read_dir(fork_artifacts)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "log"))
            .context("fork should copy parent artifacts")?;
        assert!(fs::read_to_string(fork_artifact)?.contains("ARTIFACT_RECOVERY_MARKER"));
        let fork_response = mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("artifact-fork-r1"),
                ev_assistant_message("artifact-fork-m1", "forked"),
                ev_completed("artifact-fork-r1"),
            ]),
        )
        .await;
        forked
            .thread
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "continue in fork".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        Box::pin(wait_for_event(&forked.thread, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        }))
        .await;
        let fork_request_input = fork_response.single_request().input();
        assert!(fork_request_input.len() >= watermark_after_images);
        assert_eq!(
            &dynamic_request_input[..watermark_after_images],
            &fork_request_input[..watermark_after_images],
            "the wire prefix through W survives a fork"
        );
        Box::pin(forked.thread.shutdown_and_wait()).await?;

        let compact_response = mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("artifact-compact-r1"),
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "compacted summary"
                    }
                }),
                ev_completed("artifact-compact-r1"),
            ]),
        )
        .await;
        Box::pin(rolled_back.codex.submit(Op::Compact)).await?;
        Box::pin(wait_for_event(&rolled_back.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        }))
        .await;
        rolled_back.codex.ensure_rollout_materialized().await;
        rolled_back.codex.flush_rollout().await?;
        let compact_request_input = compact_response.single_request().input();
        assert!(compact_request_input.len() >= watermark_after_images);
        assert_eq!(
            &dynamic_request_input[..watermark_after_images],
            &compact_request_input[..watermark_after_images],
            "the compaction request preserves the pre-compaction wire prefix through W"
        );
        let compacted_state = latest_shake_state(
            &rolled_back
                .codex
                .rollout_path()
                .context("rollout path after compaction")?,
        )?;
        assert_eq!(
            compacted_state.watermark,
            0,
            "checkpoint states after compaction: {:?}",
            shake_states(
                &rolled_back
                    .codex
                    .rollout_path()
                    .context("rollout path after compaction")?
            )?
            .iter()
            .map(|state| (state.epoch_id.as_str(), state.watermark))
            .collect::<Vec<_>>()
        );
        assert_ne!(compacted_state.epoch_id, state_after_images.epoch_id);
        Ok(())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shake_save_failure_preserves_the_unprocessed_suffix_for_retry() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let mut builder = test_codex().with_model("gpt-5.2");
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    let tail = "recent protected context ".repeat(/*n*/ 5_000);
    let history: Vec<ResponseItem> = vec![
        json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "preserve both outputs until saved"}]
        }),
        json!({
            "type": "function_call",
            "id": "call-one",
            "call_id": "shake-failure-one",
            "name": "exec_command",
            "arguments": "{}"
        }),
        json!({
            "type": "function_call_output",
            "call_id": "shake-failure-one",
            "output": "SHAKE_FAILURE_FIRST_MARKER\n".repeat(/*n*/ 2_000)
        }),
        json!({
            "type": "function_call",
            "id": "call-two",
            "call_id": "shake-failure-two",
            "name": "exec_command",
            "arguments": "{}"
        }),
        json!({
            "type": "function_call_output",
            "call_id": "shake-failure-two",
            "output": "SHAKE_FAILURE_SECOND_MARKER\n".repeat(/*n*/ 2_000)
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
    let preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    assert_eq!(preview.tool_outputs, 2);

    // Making the per-thread artifact directory a regular file forces every save
    // to fail without changing either candidate or the later suffix.
    fs::create_dir_all(artifact_dir.parent().context("artifact root")?)?;
    fs::write(&artifact_dir, "block artifact directory")?;
    Box::pin(fixture.codex.submit(Op::Shake {
        mode: ShakeMode::Elide,
        expected_fingerprint: Some(preview.fingerprint.clone()),
    }))
    .await?;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;
    let after_failure = fixture
        .codex
        .load_history(/*include_archived*/ false)
        .await?;
    let after_failure = serde_json::to_string(&after_failure.items)?;
    assert!(after_failure.contains("SHAKE_FAILURE_FIRST_MARKER"));
    assert!(after_failure.contains("SHAKE_FAILURE_SECOND_MARKER"));
    assert!(after_failure.contains("recent protected context"));
    assert!(!artifact_dir.is_dir());
    let failure_watermark = latest_shake_watermark(
        &fixture
            .codex
            .rollout_path()
            .context("rollout path after failed save")?,
    )?;
    let before_retry_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("shake-failure-r1"),
            ev_assistant_message("shake-failure-m1", "still present"),
            ev_completed("shake-failure-r1"),
        ]),
    )
    .await;
    Box::pin(fixture.submit_text_turn("inspect before retry")).await?;
    let before_retry_input = before_retry_response.single_request().input();

    let retry_preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    assert_eq!(retry_preview.tool_outputs, 2);

    fs::remove_file(&artifact_dir)?;
    Box::pin(fixture.codex.submit(Op::Shake {
        mode: ShakeMode::Elide,
        expected_fingerprint: Some(retry_preview.fingerprint),
    }))
    .await?;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;

    let after_retry = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    assert_eq!((after_retry.tool_outputs, after_retry.text_blocks), (0, 0));
    assert_eq!(artifact_log_files(&artifact_dir).len(), 2);
    let after_retry_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("shake-failure-r2"),
            ev_assistant_message("shake-failure-m2", "retried"),
            ev_completed("shake-failure-r2"),
        ]),
    )
    .await;
    Box::pin(fixture.submit_text_turn("inspect after retry")).await?;
    let after_retry_input = after_retry_response.single_request().input();
    assert!(before_retry_input.len() >= failure_watermark);
    assert!(after_retry_input.len() >= failure_watermark);
    assert_eq!(
        &before_retry_input[..failure_watermark],
        &after_retry_input[..failure_watermark],
        "an artifact retry preserves the wire prefix sealed before the failed envelope"
    );
    Ok(())
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
            "encrypted_content":"A".repeat(/*n*/ 2_048)
        }))?,
    ];
    Box::pin(fixture.codex.inject_response_items(items)).await?;
    let before = fixture.codex.preview_shake(mode).await?;
    assert_eq!(
        (before.images, before.thinking_blocks),
        match mode {
            ShakeMode::Images => (1, 0),
            ShakeMode::Thinking => (0, 1),
            ShakeMode::Elide => unreachable!(),
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
async fn shake_elide_recomputes_reported_token_usage_after_reducing_history() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let fixture = Box::pin(test_codex().build_with_auto_env(&server)).await?;

    // A large elidable tool output plus a plain-text tail (never elided, so it
    // both survives and pushes the tool output past `MANUAL_PROTECT_TOKENS`).
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
    ]
    .into_iter()
    .map(serde_json::from_value)
    .collect::<Result<_, _>>()?;
    Box::pin(fixture.codex.inject_response_items(history)).await?;

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
    assert!(usage_after.last_token_usage.total_tokens > 0);
    Ok(())
}
