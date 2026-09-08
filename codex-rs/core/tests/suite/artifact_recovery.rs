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

        Box::pin(fixture.codex.submit(Op::Shake {
            mode: ShakeMode::Elide,
        }))
        .await?;
        let warning = Box::pin(wait_for_event(
        &fixture.codex,
        |event| matches!(event, EventMsg::Warning(warning) if warning.message.contains("shake:")),
    ))
    .await;
        assert!(matches!(warning, EventMsg::Warning(_)));

        let artifact_dir = fixture
            .codex_home_path()
            .join("artifacts")
            .join(fixture.session_configured.thread_id.to_string());
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

        Box::pin(fixture.codex.submit(Op::Shake {
            mode: ShakeMode::Elide,
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
