use anyhow::Context;
use anyhow::Result;
use codex_core::ForkSnapshot;
use codex_core::TurnInputRequest;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::future::Future;
use std::pin::Pin;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shake_saves_and_reads_artifact_after_resume() -> Result<()> {
    skip_if_no_network!(Ok(()));

    run_artifact_recovery().await
}

fn run_artifact_recovery() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
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
        assert!(!artifact_dir.exists());
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
        assert!(!artifact_dir.exists());
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
        let artifact_id = artifact_path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.split_once('.'))
            .map(|(id, _)| format!("artifact://{id}"))
            .context("artifact filename should contain its id")?;

        let read_call_id = "artifact-read";
        let read_requests = Box::pin(mount_sse_sequence(
            &server,
            vec![
                sse(vec![
                    ev_response_created("artifact-r4"),
                    ev_function_call(
                        read_call_id,
                        "read_artifact",
                        &json!({"artifact": artifact_id}).to_string(),
                    ),
                    ev_completed("artifact-r4"),
                ]),
                sse(vec![
                    ev_assistant_message("artifact-m2", "recovered"),
                    ev_completed("artifact-r5"),
                ]),
            ],
        ))
        .await;
        Box::pin(fixture.submit_text_turn("recover that output")).await?;
        let post_shake_request = read_requests
            .requests()
            .into_iter()
            .next()
            .context("post-shake request")?;
        assert!(post_shake_request.body_contains_text(&artifact_id));
        assert!(!post_shake_request.body_contains_text("ARTIFACT_RECOVERY_MARKER"));
        assert!(
            read_requests
                .function_call_output_text(read_call_id)
                .context("read_artifact output should be sent to the model")?
                .contains("ARTIFACT_RECOVERY_MARKER")
        );

        let resumed = Box::pin(builder.restart(&server, &fixture))
            .await
            .context("restart after shake")?;
        let resumed_requests = Box::pin(mount_sse_sequence(
            &server,
            vec![
                sse(vec![
                    ev_response_created("artifact-r6"),
                    ev_function_call(
                        "artifact-read-resumed",
                        "read_artifact",
                        &json!({"artifact": artifact_id}).to_string(),
                    ),
                    ev_completed("artifact-r6"),
                ]),
                sse(vec![
                    ev_assistant_message("artifact-m3", "resumed recovery"),
                    ev_completed("artifact-r7"),
                ]),
            ],
        ))
        .await;
        Box::pin(resumed.submit_text_turn("recover it after resume")).await?;
        let resumed_request = resumed_requests
            .requests()
            .into_iter()
            .next()
            .context("post-resume request")?;
        assert!(resumed_request.body_contains_text(&artifact_id));
        assert!(
            resumed_requests
                .function_call_output_text("artifact-read-resumed")
                .context("resumed read_artifact output should be sent to the model")?
                .contains("ARTIFACT_RECOVERY_MARKER")
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
        let fork_requests = Box::pin(mount_sse_sequence(
            &server,
            vec![
                sse(vec![
                    ev_response_created("artifact-r8"),
                    ev_function_call(
                        "artifact-read-fork",
                        "read_artifact",
                        &json!({"artifact": artifact_id}).to_string(),
                    ),
                    ev_completed("artifact-r8"),
                ]),
                sse(vec![
                    ev_assistant_message("artifact-m4", "fork recovery"),
                    ev_completed("artifact-r9"),
                ]),
            ],
        ))
        .await;
        Box::pin(
            forked
                .thread
                .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                    text: "recover it in the fork".to_string(),
                    text_elements: Vec::new(),
                }])),
        )
        .await?;
        Box::pin(wait_for_event(&forked.thread, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        }))
        .await;
        assert!(
            fork_requests
                .function_call_output_text("artifact-read-fork")
                .context("fork read_artifact output should be sent to the model")?
                .contains("ARTIFACT_RECOVERY_MARKER")
        );
        let fork_rollout = forked
            .thread
            .rollout_path()
            .context("fork rollout for restart")?;
        Box::pin(forked.thread.shutdown_and_wait()).await?;
        let fork_resumed = Box::pin(builder.resume(&server, resumed.home.clone(), fork_rollout))
            .await
            .context("restart fork after shutdown")?;
        let fork_resumed_requests = Box::pin(mount_sse_sequence(
            &server,
            vec![
                sse(vec![
                    ev_response_created("artifact-r10"),
                    ev_function_call(
                        "artifact-read-fork-resumed",
                        "read_artifact",
                        &json!({"artifact": artifact_id}).to_string(),
                    ),
                    ev_completed("artifact-r10"),
                ]),
                sse(vec![
                    ev_assistant_message("artifact-m5", "fork resumed recovery"),
                    ev_completed("artifact-r11"),
                ]),
            ],
        ))
        .await;
        Box::pin(fork_resumed.submit_text_turn("recover it after fork resume")).await?;
        assert!(
            fork_resumed_requests
                .function_call_output_text("artifact-read-fork-resumed")
                .context("fork resumed read_artifact output should be sent to the model")?
                .contains("ARTIFACT_RECOVERY_MARKER")
        );
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
        assert!(!artifact_dir.exists());

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
        let body = request.body_json();
        let tools = body
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .context("model request should include a tools array")?;
        assert!(!tools.iter().any(|tool| tool["name"] == "read_artifact"));
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
    assert!(expected_freed > 0, "the shake must free tokens for this cross-check to mean anything");

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

/// Artifact id for the pre-planted oversized artifact below. Valid as a UUID
/// (32 hex digits) so `artifact://` parsing accepts it.
const BOUNDED_ARTIFACT_ID: &str = "11111111111111111111111111111111";

fn ev_completed_with_input_tokens(id: &str, input_tokens: i64) -> serde_json::Value {
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

/// A recovery read must not be able to push the next request over the context
/// window (oh-my-pi #11365). With a tight remaining window the `read_artifact`
/// page is clamped below the usual ceiling, the model is told why and where to
/// continue, and the recovered text is not re-elided into a loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_artifact_bounds_its_page_to_the_remaining_context() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        // Resolved window 4_000 => auto-compact limit 3_600 and an effective
        // window of 3_800. The 2_800 input tokens reported below therefore
        // leave ~800 tokens of headroom, well under a full 3 KiB page.
        config.model_context_window = Some(4_000);
        // Isolate the recovery path: auto-shake would otherwise measure at 70%
        // of this tiny window.
        config.auto_shake.threshold = Some(codex_config::config_toml::AutoShakeThresholdToml::Off);
    });
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;

    // Plant an artifact far larger than one page, without going through a
    // shake: this test is about the read path only.
    let artifact_dir = fixture
        .codex_home_path()
        .join("artifacts")
        .join(fixture.session_configured.thread_id.to_string());
    fs::create_dir_all(&artifact_dir)?;
    fs::write(
        artifact_dir.join(format!("{BOUNDED_ARTIFACT_ID}.tool.log")),
        "z".repeat(/*n*/ 64 * 1024),
    )?;
    let artifact_uri = format!("artifact://{BOUNDED_ARTIFACT_ID}");

    let read_call_id = "bounded-artifact-read";
    let requests = Box::pin(mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("bounded-m1", "ready"),
                ev_completed_with_input_tokens("bounded-r1", /*input_tokens*/ 2_800),
            ]),
            sse(vec![
                ev_response_created("bounded-r2"),
                ev_function_call(
                    read_call_id,
                    "read_artifact",
                    &json!({"artifact": artifact_uri}).to_string(),
                ),
                ev_completed_with_input_tokens("bounded-r2", /*input_tokens*/ 2_800),
            ]),
            sse(vec![
                ev_assistant_message("bounded-m2", "recovered"),
                ev_completed_with_input_tokens("bounded-r3", /*input_tokens*/ 2_800),
            ]),
        ],
    ))
    .await;

    Box::pin(fixture.submit_text_turn("warm the usage counters")).await?;
    Box::pin(fixture.submit_text_turn("recover that artifact")).await?;

    let output = requests
        .function_call_output_text(read_call_id)
        .context("read_artifact output should be sent to the model")?;

    let body = output
        .split_once("\n[artifact source:")
        .map(|(body, _)| body)
        .context("an oversized recovery must carry a continuation marker")?;
    assert!(
        body.len() < 3 * 1024,
        "page should be clamped below the 3 KiB ceiling, got {} bytes",
        body.len()
    );
    assert!(
        output.contains(&format!(
            "[artifact source: {artifact_uri}; more content; use start_byte={}]",
            body.len()
        )),
        "the notice must say where to continue: {output}"
    );
    assert!(
        output.contains("bounded to") && output.contains("remaining"),
        "the notice must explain the context-derived bound: {output}"
    );

    // The recovered region must not be re-elidable: a manual shake that could
    // elide it would only mint another artifact, and could repeat forever.
    // `TurnComplete` can land a moment before `active_turn` is cleared, so give
    // the preview a few tries before treating a refusal as a failure.
    let mut preview = None;
    for _ in 0..50 {
        match fixture.codex.preview_shake(ShakeMode::Elide).await {
            Ok(value) => {
                preview = Some(value);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    let preview = preview.context("preview should become available once the turn settles")?;
    assert_eq!(
        preview.tool_outputs, 0,
        "a recovery read's output must not be a shake candidate"
    );
    Ok(())
}
