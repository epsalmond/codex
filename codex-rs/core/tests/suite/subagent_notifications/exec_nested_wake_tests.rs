use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_root_keeps_polling_while_thread_spawn_descendants_wake() -> Result<()> {
    let server = start_mock_server().await;
    let root_spawn_args = serde_json::to_string(&json!({
        "message": "exec child task marker",
        "task_name": "worker",
    }))?;
    let child_spawn_args = serde_json::to_string(&json!({
        "message": "exec grandchild task marker",
        "task_name": "grandchild",
    }))?;
    let root_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "start the Exec nested wake assignment"),
        sse(vec![
            ev_response_created("exec-nested-root-start"),
            ev_function_call_with_namespace(
                "exec-nested-root-spawn",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &root_spawn_args,
            ),
            ev_completed("exec-nested-root-start"),
        ]),
    )
    .await;
    let root_wait = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "exec-nested-root-spawn")
                && !body_contains(request, "exec worker result marker")
        },
        sse(vec![
            ev_response_created("exec-nested-root-wait"),
            ev_function_call_with_namespace(
                "exec-nested-wait-agent",
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                "{}",
            ),
            ev_completed("exec-nested-root-wait"),
        ]),
    )
    .await;
    let child_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_type(request, "agent_message")
                && body_contains(request, "exec child task marker")
        },
        sse(vec![
            ev_response_created("exec-nested-child-start"),
            ev_function_call_with_namespace(
                "exec-nested-child-spawn",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &child_spawn_args,
            ),
            ev_completed("exec-nested-child-start"),
        ]),
    )
    .await;
    let child_yield = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "exec-nested-child-spawn")
                && body_contains(request, "exec grandchild task marker")
                && !body_contains(request, "exec grandchild result marker")
        },
        sse(vec![
            ev_response_created("exec-nested-child-yield"),
            ev_assistant_message("exec-nested-child-waiting", "child waits for grandchild"),
            ev_completed("exec-nested-child-yield"),
        ]),
    )
    .await;
    let grandchild_initial = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_type_with_text(
                request,
                "agent_message",
                "exec grandchild task marker",
            )
        },
        sse_response(sse(vec![
            ev_response_created("exec-nested-grandchild-start"),
            ev_assistant_message("exec-nested-grandchild-result", "exec grandchild result marker"),
            ev_completed("exec-nested-grandchild-start"),
        ]))
        .set_delay(Duration::from_millis(100)),
    )
    .await;
    let child_resume = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_type_with_text(
                request,
                "agent_message",
                "exec grandchild result marker",
            )
        },
        sse(vec![
            ev_response_created("exec-nested-child-resume"),
            ev_assistant_message("exec-nested-child-final", "exec worker result marker"),
            ev_completed("exec-nested-child-resume"),
        ]),
    )
    .await;
    let root_resume = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_type_with_text(
                request,
                "agent_message",
                "exec worker result marker",
            )
        },
        sse(vec![
            ev_response_created("exec-nested-root-resume"),
            ev_assistant_message("exec-nested-root-final", "Exec root incorporated child result"),
            ev_completed("exec-nested-root-resume"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_model("koffing")
        .with_session_source(SessionSource::Exec)
        .with_config(|config| {
            for feature in [
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::CurrentTimeReminder,
                Feature::SleepTool,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("test config should allow feature update");
            }
            config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Disabled;
            config.multi_agent_v2.max_concurrent_threads_per_session = 3;
            config.current_time_reminder = Some(CurrentTimeReminderConfig {
                sleep_tool: true,
                ..CurrentTimeReminderConfig::default()
            });
            config.sleep_tool_mode = codex_features::SleepToolMode::AlwaysOn;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;

    test.submit_turn("start the Exec nested wake assignment")
        .await?;
    let root_request = wait_for_request_matching(&root_initial, |request| {
        request.body_contains_text("start the Exec nested wake assignment")
    })
        .await
        .context("wait for Exec root spawn request")?;
    let root_thread_id = test.session_configured.thread_id;
    assert!(namespace_child_tool(
        &root_request.body_json(),
        MULTI_AGENT_V2_NAMESPACE,
        "wait_agent"
    )
    .is_some());
    let child_request = wait_for_request_matching(&child_initial, |request| {
        response_request_has_input_type(request, "agent_message")
            && request.body_contains_text("exec child task marker")
    })
        .await
        .context("wait for nested child request")?;
    let child_thread_id = ThreadId::from_string(
        child_request.body_json()["client_metadata"]["thread_id"]
            .as_str()
            .expect("child thread ID"),
    )?;
    assert!(namespace_child_tool(
        &child_request.body_json(),
        MULTI_AGENT_V2_NAMESPACE,
        "wait_agent"
    )
    .is_none());
    assert!(namespace_child_tool(&child_request.body_json(), "clock", "curr_time").is_some());
    assert!(namespace_child_tool(&child_request.body_json(), "clock", "sleep").is_none());
    let _root_wait_request = wait_for_request_matching(&root_wait, |request| {
        response_request_has_thread_id(request, root_thread_id)
            && request.body_contains_text("exec-nested-root-spawn")
            && !request.body_contains_text("exec worker result marker")
    })
        .await
        .context("wait for Exec root polling request")?;
    let _child_yield_request = wait_for_request_matching(&child_yield, |request| {
        response_request_has_thread_id(request, child_thread_id)
            && request.body_contains_text("exec-nested-child-spawn")
            && request.body_contains_text("exec grandchild task marker")
            && !request.body_contains_text("exec grandchild result marker")
    })
        .await
        .context("wait for nested child to yield")?;
    let _grandchild_request = wait_for_request_matching(&grandchild_initial, |request| {
        response_request_has_input_type(request, "agent_message")
            && !response_request_has_thread_id(request, child_thread_id)
            && !response_request_has_thread_id(request, root_thread_id)
            && request.body_contains_text("exec grandchild task marker")
    })
        .await
        .context("wait for nested grandchild request")?;
    let _child_resume_request = wait_for_request_matching(&child_resume, |request| {
        response_request_has_thread_id(request, child_thread_id)
            && request.body_contains_text("exec grandchild result marker")
    })
        .await
        .context("wait for nested child wake")?;
    let root_final_request = wait_for_request_matching(&root_resume, |request| {
        response_request_has_thread_id(request, root_thread_id)
            && request.body_contains_text("exec worker result marker")
    })
        .await
        .context("wait for Exec root result after wait_agent")?;
    let report_count = root_final_request.body_json()["input"]
        .as_array()
        .expect("Exec root resume has model input")
        .iter()
        .filter(|item| {
            item["type"] == "agent_message" && item.to_string().contains("exec worker result marker")
        })
        .count();
    assert_eq!(report_count, 1);
    assert_eq!(
        child_resume
            .requests()
            .iter()
            .filter(|request| {
                response_request_has_thread_id(request, child_thread_id)
                    && request.body_contains_text("exec grandchild result marker")
            })
            .count(),
        1
    );
    Ok(())
}
