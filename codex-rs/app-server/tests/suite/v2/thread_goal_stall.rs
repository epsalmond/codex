//! Automatic admission, persistent Active holds, and authorized recovery through JSON-RPC.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_app_server_protocol::WarningNotification;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

#[test_case::test_case("user"; "real_user_releases")]
#[test_case::test_case("activate"; "explicit_activation_releases")]
#[test_case::test_case("objective"; "same_id_objective_releases")]
#[test_case::test_case("off"; "off_load_releases")]
#[test_case::test_case("observe"; "observe_load_releases")]
#[tokio::test]
async fn automatic_stall_holds_active_across_restart_and_recovers(recovery: &str) -> Result<()> {
    let total = if matches!(recovery, "activate" | "objective") {
        4
    } else {
        5
    };
    let scripts = (1..=total)
        .map(|turn| {
            responses::sse(vec![
                responses::ev_response_created(&format!("response-{turn}")),
                responses::ev_assistant_message(&format!("final-{turn}"), ""),
                responses::ev_completed(&format!("response-{turn}")),
            ])
        })
        .collect();
    let server = create_mock_responses_server_sequence(scripts).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.4")
        .enable_feature(Feature::Goals)
        .write(home.path())?;
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
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId":thread.id, "objective":"Finish the work"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(id).await?;
    for _ in 0..2 {
        let _: TurnCompletedNotification = timeout(
            Duration::from_secs(30),
            app.read_notification("turn/completed"),
        )
        .await??;
    }
    wait_for_stall_hold(&mut app).await?;
    let id = app
        .send_raw_request("thread/goal/get", Some(json!({"threadId":thread.id})))
        .await?;
    let state: ThreadGoalGetResponse = app.read_response(id).await?;
    assert_eq!(
        state.goal.expect("active held goal").status,
        ThreadGoalStatus::Active
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("model requests")
            .len(),
        2
    );
    timeout(Duration::from_secs(30), app.shutdown_gracefully()).await??;
    drop(app);

    if matches!(recovery, "off" | "observe") {
        let path = home.path().join("config.toml");
        let config = std::fs::read_to_string(&path)?;
        std::fs::write(
            path,
            format!("{config}\n[goals]\ncontinuation_guard_mode = \"{recovery}\"\n"),
        )?;
    }
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let id = app
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse = app.read_response(id).await?;
    if !matches!(recovery, "off" | "observe") {
        // Repeated maintenance and resumed state reads must leave the persisted hold intact.
        for _ in 0..2 {
            let id = app
                .send_raw_request("thread/goal/get", Some(json!({"threadId":thread.id})))
                .await?;
            let state: ThreadGoalGetResponse = app.read_response(id).await?;
            assert_eq!(
                state.goal.expect("persisted active goal").status,
                ThreadGoalStatus::Active
            );
        }
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("model requests")
                .len(),
            2
        );
        if recovery == "user" {
            app.start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Continue with the next task".to_owned(),
                    text_elements: vec![],
                }],
                ..Default::default()
            })
            .await?;
        } else {
            let change = if recovery == "objective" {
                json!({"threadId":thread.id, "objective":"Finish the revised work"})
            } else {
                json!({"threadId":thread.id, "status":"active"})
            };
            let id = app
                .send_raw_request("thread/goal/set", Some(change))
                .await?;
            let _: ThreadGoalSetResponse = app.read_response(id).await?;
        }
    }
    let remaining = if recovery == "user" { 2 } else { total - 2 };
    for _ in 0..remaining {
        let _: TurnCompletedNotification = timeout(
            Duration::from_secs(30),
            app.read_notification("turn/completed"),
        )
        .await??;
    }
    let id = app
        .send_raw_request("thread/goal/get", Some(json!({"threadId":thread.id})))
        .await?;
    let state: ThreadGoalGetResponse = app.read_response(id).await?;
    assert_eq!(
        state.goal.expect("retained goal").status,
        if matches!(recovery, "off" | "observe") {
            ThreadGoalStatus::Blocked
        } else {
            ThreadGoalStatus::Active
        }
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("model requests")
            .len(),
        total
    );
    server.verify().await;
    Ok(())
}

#[tokio::test]
async fn actual_code_mode_outputs_have_the_same_live_and_replay_hold_boundary() -> Result<()> {
    let code = "await new Promise(done => setTimeout(done, 1)); text(\"ack\");";
    let mut scripts = Vec::new();
    for turn in 1..=3 {
        let call = format!("call-{turn}");
        scripts.push(responses::sse(vec![
            responses::ev_response_created(&call),
            responses::ev_custom_tool_call(&call, "exec", code),
            responses::ev_completed(&call),
        ]));
        let final_id = format!("final-{turn}");
        scripts.push(responses::sse(vec![
            responses::ev_response_created(&final_id),
            responses::ev_assistant_message(&final_id, "Waiting for the current result"),
            responses::ev_completed(&final_id),
        ]));
    }
    let server = create_mock_responses_server_sequence(scripts).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.4")
        .enable_feature(Feature::Goals)
        .enable_feature(Feature::CodeModeOnly)
        .write(home.path())?;
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
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId":thread.id,"objective":"Complete the work"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(id).await?;
    for _ in 0..3 {
        let _: TurnCompletedNotification = timeout(
            Duration::from_secs(30),
            app.read_notification("turn/completed"),
        )
        .await??;
    }
    wait_for_stall_hold(&mut app).await?;
    let requests = server
        .received_requests()
        .await
        .expect("actual model requests");
    assert_eq!(requests.len(), 6);
    let mut replay =
        crate::stall_replay::Replay::new(&Default::default()).map_err(anyhow::Error::msg)?;
    let threshold = std::num::NonZeroU32::MIN.saturating_add(/*rhs*/ 1);
    for turn in 1..=3 {
        let id = turn.to_string();
        let call = format!("call-{turn}");
        let output = requests
            .iter()
            .find_map(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
                body["input"]
                    .as_array()?
                    .iter()
                    .find(|item| {
                        item["type"] == "custom_tool_call_output" && item["call_id"] == call
                    })
                    .cloned()
            })
            .expect("model accepted native Code Mode output");
        replay.record(
            &json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}}),
            threshold,
        );
        replay.record(&json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Changed presentation text"}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["goal.internal_context"]}}}), threshold);
        replay.record(&json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":call,"name":"exec","input":code}}), threshold);
        replay.record(&json!({"type":"response_item","payload":output}), threshold);
        assert_eq!(replay.record(&json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id,"last_agent_message":"Waiting for the current result"}}), threshold),
            Some(if turn == 1 { crate::stall::Assessment::NotSuspected } else { crate::stall::Assessment::Suspected {
                reason:crate::stall::Suspicion::RepeatedActions, streak:turn-1, threshold_reached:turn==3
            } }));
    }
    assert_eq!(replay.stats.unknown, 0);
    server.verify().await;
    Ok(())
}

async fn wait_for_stall_hold(app: &mut TestAppServer) -> Result<()> {
    let notification = timeout(
        Duration::from_secs(30),
        app.read_stream_until_matching_notification("goal stall hold", |notification| {
            notification.method == "warning"
                && notification
                    .params
                    .as_ref()
                    .and_then(|params| params["message"].as_str())
                    .is_some_and(|message| {
                        message.starts_with("Automatic goal continuation is held")
                    })
        }),
    )
    .await??;
    let warning: WarningNotification =
        serde_json::from_value(notification.params.expect("warning parameters"))?;
    assert!(warning.message.contains("goal remains active"));
    Ok(())
}

#[tokio::test]
async fn productive_automatic_failure_edit_validation_and_new_read_complete_without_a_hold()
-> Result<()> {
    use core_test_support::TestTargetOs;
    use core_test_support::test_target_os;
    let (validate, read) = match test_target_os() {
        TestTargetOs::Windows => (
            "if (!(Test-Path repaired.txt)) { exit 1 }",
            "Get-Content repaired.txt",
        ),
        TestTargetOs::Linux | TestTargetOs::MacOs => ("test -f repaired.txt", "cat repaired.txt"),
    };
    let calls = [
        responses::ev_function_call(
            "fail",
            "exec_command",
            &json!({"cmd":"exit 1","yield_time_ms":1000}).to_string(),
        ),
        responses::ev_custom_tool_call(
            "edit",
            "apply_patch",
            "*** Begin Patch\n*** Add File: repaired.txt\n+passed\n*** End Patch",
        ),
        responses::ev_function_call(
            "validate",
            "exec_command",
            &json!({"cmd":validate,"yield_time_ms":1000}).to_string(),
        ),
        responses::ev_function_call(
            "read",
            "exec_command",
            &json!({"cmd":read,"yield_time_ms":1000}).to_string(),
        ),
        responses::ev_function_call("complete", "update_goal", r#"{"status":"complete"}"#),
    ];
    let mut scripts = Vec::new();
    for (index, call) in calls.into_iter().enumerate() {
        let id = index.to_string();
        scripts.push(responses::sse(vec![
            responses::ev_response_created(&id),
            call,
            responses::ev_completed(&id),
        ]));
        scripts.push(responses::sse(vec![
            responses::ev_response_created(&id),
            responses::ev_assistant_message(&id, "Waiting for the next result"),
            responses::ev_completed(&id),
        ]));
    }
    let server = create_mock_responses_server_sequence(scripts).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-6.1-sol")
        .with_sandbox_mode("danger-full-access")
        .enable_feature(Feature::Goals)
        .enable_feature(Feature::UnifiedExec)
        .disable_feature(Feature::CodeModeOnly)
        .disable_feature(Feature::ShellZshFork)
        .disable_feature(Feature::ShellSnapshot)
        .write(home.path())?;
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
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId":thread.id,"objective":"Repair and validate the file"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(id).await?;
    for _ in 0..5 {
        let _: TurnCompletedNotification = timeout(
            Duration::from_secs(30),
            app.read_notification("turn/completed"),
        )
        .await??;
    }
    let id = app
        .send_raw_request("thread/goal/get", Some(json!({"threadId":thread.id})))
        .await?;
    let state: ThreadGoalGetResponse = app.read_response(id).await?;
    assert_eq!(
        state.goal.expect("completed productive goal").status,
        ThreadGoalStatus::Complete
    );
    let requests = server
        .received_requests()
        .await
        .expect("actual model requests");
    assert_eq!(requests.len(), 10);
    let outcomes = ["fail", "validate", "read"].map(|call| {
        let output = requests
            .iter()
            .find_map(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
                body["input"]
                    .as_array()?
                    .iter()
                    .find(|item| item["type"] == "function_call_output" && item["call_id"] == call)
                    .map(|item| item["output"].clone())
            })
            .expect("accepted native command output");
        crate::stall_observation::model_output(
            "exec_command",
            crate::stall_observation::OutputBodyKind::Function,
            output,
        )
    });
    assert_eq!(
        (
            outcomes
                .each_ref()
                .map(|outcome| outcome["exit_code"].as_i64()),
            outcomes[2]["output"].as_str().map(str::trim)
        ),
        ([Some(1), Some(0), Some(0)], Some("passed"))
    );
    server.verify().await;
    Ok(())
}

#[test_case::test_case("web_search"; "provider_work_is_unknown")]
#[test_case::test_case("multiple_final"; "substantive_message_survives_waiting_signoff")]
#[tokio::test]
async fn automatic_provider_and_assistant_evidence_matches_replay(kind: &str) -> Result<()> {
    let mut replay =
        crate::stall_replay::Replay::new(&Default::default()).map_err(anyhow::Error::msg)?;
    let threshold = std::num::NonZeroU32::MIN.saturating_add(/*rhs*/ 1);
    let mut scripts = Vec::new();
    for turn in 1..=2 {
        let id = format!("response-{turn}");
        let evidence = if kind == "web_search" {
            json!({"id":format!("search-{turn}"),"type":"web_search_call","status":"completed","action":{"type":"search","query":format!("new query {turn}")}})
        } else {
            json!({"id":format!("substantive-{turn}"),"type":"message","role":"assistant","content":[{"type":"output_text","text":format!("Verified new evidence {turn}; the next action has changed.")} ]})
        };
        scripts.push(responses::sse(vec![
            responses::ev_response_created(&id),
            json!({"type":"response.output_item.done","item":evidence}),
            responses::ev_assistant_message(
                &format!("final-{turn}"),
                "Waiting for the next result",
            ),
            responses::ev_completed(&id),
        ]));
        let id = turn.to_string();
        replay.record(
            &json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}}),
            threshold,
        );
        replay.record(&json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Automatic goal context"}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["goal.internal_context"]}}}), threshold);
        replay.record(
            &json!({"type":"response_item","payload":evidence}),
            threshold,
        );
        assert_eq!(replay.record(&json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id,"last_agent_message":"Waiting for the next result"}}), threshold),
            Some(if kind == "web_search" { crate::stall::Assessment::Unclassified } else { crate::stall::Assessment::NotSuspected }));
    }
    assert_eq!(
        replay.stats.unknown,
        if kind == "web_search" { 2 } else { 0 }
    );
    scripts.push(responses::sse(vec![
        responses::ev_response_created("complete"),
        responses::ev_function_call("complete-goal", "update_goal", "{\"status\":\"complete\"}"),
        responses::ev_completed("complete"),
    ]));
    scripts.push(responses::sse(vec![
        responses::ev_response_created("final"),
        responses::ev_assistant_message("last", "Done"),
        responses::ev_completed("final"),
    ]));
    let server = create_mock_responses_server_sequence(scripts).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-6.1-sol")
        .enable_feature(Feature::Goals)
        .write(home.path())?;
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
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId":thread.id,"objective":"Use changing evidence to finish"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(id).await?;
    for _ in 0..3 {
        let _: TurnCompletedNotification = timeout(
            Duration::from_secs(30),
            app.read_notification("turn/completed"),
        )
        .await??;
    }
    let id = app
        .send_raw_request("thread/goal/get", Some(json!({"threadId":thread.id})))
        .await?;
    let state: ThreadGoalGetResponse = app.read_response(id).await?;
    assert_eq!(
        state.goal.expect("completed productive goal").status,
        ThreadGoalStatus::Complete
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("actual model requests")
            .len(),
        4
    );
    server.verify().await;
    Ok(())
}
