use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_mode_code_mode_child_request_keeps_wait_and_clock_but_hides_polling() -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": "code mode child task marker",
        "task_name": "code_worker",
        "model": "koffing",
    }))?;
    let root_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "spawn the code-mode wake child"),
        sse(vec![
            ev_response_created("code-mode-wake-root-start"),
            ev_function_call_with_namespace(
                "code-mode-wake-spawn",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("code-mode-wake-root-start"),
        ]),
    )
    .await;
    let child_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_type(request, "agent_message")
                && body_contains(request, "code mode child task marker")
        },
        sse(vec![
            ev_response_created("code-mode-wake-child-start"),
            ev_assistant_message("code-mode-wake-child-final", "code-mode result marker"),
            ev_completed("code-mode-wake-child-start"),
        ]),
    )
    .await;
    let _root_resume = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_type_with_text(
                request,
                "agent_message",
                "code-mode result marker",
            )
        },
        sse(vec![
            ev_response_created("code-mode-wake-root-resume"),
            ev_assistant_message("code-mode-wake-root-final", "code-mode child incorporated"),
            ev_completed("code-mode-wake-root-resume"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("koffing", |model| {
            model.tool_mode = Some(ToolMode::CodeMode);
        })
        .with_session_source(SessionSource::Cli)
        .with_config(|config| {
            for feature in [
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::CurrentTimeReminder,
                Feature::SleepTool,
                Feature::CodeMode,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("test config should allow feature update");
            }
            config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Disabled;
            config.multi_agent_v2.expose_spawn_agent_model_overrides = true;
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

    test.submit_turn("spawn the code-mode wake child").await?;
    let root_request = wait_for_request_matching(&root_initial, |request| {
        request.body_contains_text("spawn the code-mode wake child")
    })
        .await
        .context("wait for root spawn request")?;
    let root_thread_id = test.session_configured.thread_id;
    assert!(
        namespace_child_tool(
            &root_request.body_json(),
            MULTI_AGENT_V2_NAMESPACE,
            "wait_agent"
        )
        .is_none()
    );
    let child_request = wait_for_request_matching(&child_initial, |request| {
        response_request_has_input_type(request, "agent_message")
            && request.body_contains_text("code mode child task marker")
            && !response_request_has_thread_id(request, root_thread_id)
    })
        .await
        .context("wait for code-mode child request")?;
    let child_thread_id = ThreadId::from_string(
        child_request.body_json()["client_metadata"]["thread_id"]
            .as_str()
            .expect("child thread ID"),
    )?;
    let child_tools = child_request.body_json()["tools"].to_string();
    assert!(child_tools.contains(codex_code_mode::WAIT_TOOL_NAME));
    assert!(child_tools.contains("curr_time"));
    assert!(!child_tools.contains("sleep"));
    assert!(!child_tools.contains("wait_agent"));
    assert_ne!(child_thread_id, root_thread_id);
    Ok(())
}
