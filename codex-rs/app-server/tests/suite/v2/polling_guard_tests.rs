//! No-goal same-turn intervention through the public app-server API.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::write_models_cache;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::TurnSteerParams;
use codex_app_server_protocol::TurnSteerResponse;
use codex_app_server_protocol::UserInput;
use codex_app_server_protocol::WarningNotification;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;

#[test_case::test_case("direct"; "direct_checks")]
#[test_case::test_case("code"; "nested_code_mode_checks")]
#[test_case::test_case("target"; "changed_target_continues")]
#[test_case::test_case("parallel"; "parallel_code_mode_continues")]
#[test_case::test_case("parallel_direct"; "parallel_direct_continues")]
#[test_case::test_case("malformed"; "malformed_nested_resets")]
#[test_case::test_case("unknown"; "unsupported_code_mode_continues")]
#[test_case::test_case("unregistered"; "unregistered_tool_resets")]
#[test_case::test_case("process"; "real_process_checks")]
#[tokio::test]
async fn polling_guard_same_turn_without_goal(case: &str) -> Result<()> {
    let interrupted = matches!(case, "direct" | "code" | "process");
    let code_mode = matches!(
        case,
        "code" | "parallel" | "unknown" | "process" | "malformed"
    );
    let mut scripts = Vec::new();
    if code_mode {
        let code = match case {
            "code" => {
                "for (let i = 0; i < 4; i++) text(await tools.collaboration__list_agents({}));"
            }
            "parallel" => {
                "text(await Promise.all(Array.from({length: 4}, () => tools.collaboration__list_agents({}))));"
            }
            "unknown" => {
                "text(await tools.collaboration__list_agents({})); text(await tools.collaboration__list_agents({})); text(await tools.exec_command({cmd: 'echo progress'})); text(await tools.collaboration__list_agents({})); text(await tools.collaboration__list_agents({}));"
            }
            "malformed" => {
                "for (let i = 0; i < 3; i++) text(await tools.collaboration__list_agents({})); try { await tools.collaboration__list_agents('invalid'); } catch (error) { text(String(error)); } text(await tools.collaboration__list_agents({}));"
            }
            "process" => {
                #[cfg(windows)]
                {
                    "// @exec: {\"yield_time_ms\":30000}\nconst process = await tools.exec_command({cmd: 'powershell -Command \"Start-Sleep -Seconds 60\"', yield_time_ms: 1}); for (let i = 0; i < 4; i++) text(await tools.write_stdin({session_id: process.session_id, yield_time_ms: 1}));"
                }
                #[cfg(not(windows))]
                {
                    "// @exec: {\"yield_time_ms\":30000}\nconst process = await tools.exec_command({cmd: 'sleep 60', yield_time_ms: 1}); for (let i = 0; i < 4; i++) text(await tools.write_stdin({session_id: process.session_id, yield_time_ms: 1}));"
                }
            }
            _ => unreachable!(),
        };
        let mut call = responses::ev_custom_tool_call("wrapper", "exec", code);
        // Exact host-provided origin is required; absent provenance is Unknown.
        call["item"]["id"] = Value::String("wrapper-item".to_owned());
        scripts.push(responses::sse(vec![
            responses::ev_response_created("code-response"),
            call,
            responses::ev_completed("code-response"),
        ]));
    } else if case == "parallel_direct" {
        let mut batch = (0..4)
            .map(|index| {
                responses::ev_function_call_with_namespace(
                    &format!("poll-{index}"),
                    "collaboration",
                    "list_agents",
                    "{}",
                )
            })
            .collect::<Vec<_>>();
        batch.push(responses::ev_completed("parallel-response"));
        scripts.push(responses::sse(batch));
    } else {
        for index in 0..4 {
            let id = format!("poll-{index}");
            let args = if case == "target" && index >= 2 {
                r#"{"path_prefix":"/root/other"}"#
            } else {
                "{}"
            };
            scripts.push(responses::sse(vec![
                responses::ev_response_created(&id),
                responses::ev_function_call_with_namespace(
                    &id,
                    "collaboration",
                    "list_agents",
                    args,
                ),
                responses::ev_completed(&id),
            ]));
            if case == "unregistered" && index == 2 {
                scripts.push(responses::sse(vec![
                    responses::ev_function_call("unsupported", "unsupported_tool", "{}"),
                    responses::ev_completed("unsupported-response"),
                ]));
            }
        }
    }
    if !interrupted {
        scripts.push(responses::sse(vec![
            responses::ev_assistant_message("final", "Done"),
            responses::ev_completed("final-response"),
        ]));
    }
    let expected_requests = scripts.len();
    let server = create_mock_responses_server_sequence(scripts).await;
    let home = TempDir::new()?;
    let mut config = MockResponsesConfig::new(&server.uri())
        .with_model("gpt-6.1-sol")
        .with_root_config("approvals_reviewer = \"user\"")
        .disable_feature(Feature::Goals);
    if code_mode {
        config = config
            .enable_feature(Feature::CodeModeOnly)
            .with_extra_config(
                "[features.multi_agent_v2]\nenabled = true\nnon_code_mode_only = false",
            );
    }
    config.write(home.path())?;
    write_models_cache(home.path()).await?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let id = app
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(id).await?;
    let started = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Check the existing work".to_owned(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    let _: codex_app_server_protocol::TurnStartResponse = app.read_response(started).await?;
    let completed: TurnCompletedNotification = timeout(
        Duration::from_secs(30),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(
        completed.turn.status,
        if interrupted {
            TurnStatus::Interrupted
        } else {
            TurnStatus::Completed
        }
    );
    // FIFO delivery makes the warning observable before the interrupted terminal event.
    let warnings = app
        .pending_notification_methods()
        .iter()
        .filter(|method| *method == "warning")
        .count();
    let mut polling_warnings = Vec::new();
    for _ in 0..warnings {
        let warning: WarningNotification = app.read_notification("warning").await?;
        if warning
            .message
            .starts_with("Repeated status checks returned no new information")
        {
            polling_warnings.push(warning);
        }
    }
    assert_eq!(polling_warnings.len(), usize::from(interrupted));
    if let Some(warning) = polling_warnings.first() {
        assert_eq!(warning.thread_id, Some(thread.id));
    }
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("model requests")
            .len(),
        expected_requests
    );
    server.verify().await;
    app.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn polling_guard_preserves_input_queued_before_the_fourth_check() -> Result<()> {
    let mut scripts = (0..3)
        .map(|index| {
            vec![StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![
                    responses::ev_function_call_with_namespace(
                        &format!("poll-{index}"),
                        "collaboration",
                        "list_agents",
                        "{}",
                    ),
                    responses::ev_completed(&format!("response-{index}")),
                ]),
            }]
        })
        .collect::<Vec<_>>();
    let (release, gate) = oneshot::channel();
    scripts.push(vec![StreamingSseChunk {
        gate: Some(gate),
        body: responses::sse(vec![
            responses::ev_function_call_with_namespace(
                "poll-4",
                "collaboration",
                "list_agents",
                "{}",
            ),
            responses::ev_completed("response-4"),
        ]),
    }]);
    scripts.push(vec![StreamingSseChunk {
        gate: None,
        body: responses::sse(vec![
            responses::ev_assistant_message("answer", "Preserved"),
            responses::ev_completed("final"),
        ]),
    }]);
    let (server, _) = start_streaming_sse_server(scripts).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(server.uri())
        .with_model("gpt-6.1-sol")
        .disable_feature(Feature::Goals)
        .write(home.path())?;
    write_models_cache(home.path()).await?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let id = app
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(id).await?;
    let id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Check the work".to_owned(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    let started: codex_app_server_protocol::TurnStartResponse = app.read_response(id).await?;
    timeout(Duration::from_secs(30), server.wait_for_request_count(4)).await?;
    let id = app
        .send_turn_steer_request(TurnSteerParams {
            thread_id: thread.id.clone(),
            expected_turn_id: started.turn.id.clone(),
            input: vec![UserInput::Text {
                text: "preserve this queued input".to_owned(),
                text_elements: vec![],
            }],
            client_user_message_id: None,
            responsesapi_client_metadata: None,
            additional_context: None,
        })
        .await?;
    let _: TurnSteerResponse = app.read_response(id).await?;
    release
        .send(())
        .expect("release fourth model response after input is queued");
    let completed: TurnCompletedNotification = timeout(
        Duration::from_secs(30),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    // Accepted input clears the polling episode before the fourth handler starts.
    // The conditional-interruption core tests separately exercise queued-input refusal.
    assert!(
        !app.pending_notification_methods()
            .iter()
            .any(|method| method == "warning")
    );
    let requests = server.requests().await;
    assert_eq!(requests.len(), 5);
    let final_request: Value = serde_json::from_slice(requests.last().unwrap())?;
    assert!(
        final_request["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["role"] == "user"
                && item["content"]
                    .to_string()
                    .contains("preserve this queued input"))
    );
    app.shutdown_gracefully().await?;
    server.shutdown().await;
    Ok(())
}
