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
