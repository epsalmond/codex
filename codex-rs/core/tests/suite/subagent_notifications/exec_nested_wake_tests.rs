use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_root_keeps_polling_while_thread_spawn_descendants_wake() -> Result<()> {
    let (grandchild_release_tx, grandchild_release_rx) = oneshot::channel();
    let root_spawn_args = serde_json::to_string(&json!({
        "message": "exec child task marker",
        "task_name": "worker",
    }))?;
    let child_spawn_args = serde_json::to_string(&json!({
        "message": "exec grandchild task marker",
        "task_name": "grandchild",
    }))?;
    let root_id = Arc::new(Mutex::new(None::<String>));
    let child_id = Arc::new(Mutex::new(None::<String>));
    let root_id_for_matcher = Arc::clone(&root_id);
    let child_id_for_matcher = Arc::clone(&child_id);
    let (server, _completions) = start_routed_streaming_sse_server(
        vec![
            vec![vec![
                streaming_event_chunk(ev_response_created("exec-nested-root-start")),
                streaming_event_chunk(ev_function_call_with_namespace(
                    "exec-nested-root-spawn",
                    MULTI_AGENT_V2_NAMESPACE,
                    "spawn_agent",
                    &root_spawn_args,
                )),
                streaming_event_chunk(ev_completed("exec-nested-root-start")),
            ]],
            vec![
                vec![
                    streaming_event_chunk(ev_response_created("exec-nested-root-wait")),
                    streaming_event_chunk(ev_function_call_with_namespace(
                        "exec-nested-wait-agent",
                        MULTI_AGENT_V2_NAMESPACE,
                        "wait_agent",
                        "{}",
                    )),
                    streaming_event_chunk(ev_completed("exec-nested-root-wait")),
                ],
                vec![
                    streaming_event_chunk(ev_response_created("exec-nested-root-resume")),
                    streaming_event_chunk(ev_assistant_message(
                        "exec-nested-root-final",
                        "Exec root incorporated child result",
                    )),
                    streaming_event_chunk(ev_completed("exec-nested-root-resume")),
                ],
            ],
            vec![
                vec![
                    streaming_event_chunk(ev_response_created("exec-nested-child-start")),
                    streaming_event_chunk(ev_function_call_with_namespace(
                        "exec-nested-child-spawn",
                        MULTI_AGENT_V2_NAMESPACE,
                        "spawn_agent",
                        &child_spawn_args,
                    )),
                    streaming_event_chunk(ev_completed("exec-nested-child-start")),
                ],
                vec![
                    streaming_event_chunk(ev_response_created("exec-nested-child-yield")),
                    streaming_event_chunk(ev_assistant_message(
                        "exec-nested-child-waiting",
                        "child waits for grandchild",
                    )),
                    streaming_event_chunk(ev_completed("exec-nested-child-yield")),
                ],
                vec![
                    streaming_event_chunk(ev_response_created("exec-nested-child-resume")),
                    streaming_event_chunk(ev_assistant_message(
                        "exec-nested-child-final",
                        "exec worker result marker",
                    )),
                    streaming_event_chunk(ev_completed("exec-nested-child-resume")),
                ],
            ],
            vec![vec![
                streaming_event_chunk(ev_response_created("exec-nested-grandchild-start")),
                streaming_event_chunk(ev_assistant_message(
                    "exec-nested-grandchild-result",
                    "exec grandchild result marker",
                )),
                gated_streaming_chunk(
                    grandchild_release_rx,
                    vec![ev_completed("exec-nested-grandchild-start")],
                ),
            ]],
        ],
        move |_headers, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let request_thread_id = body["client_metadata"]["thread_id"].as_str()?;
            let body_text = body.to_string();
            let mut root_id = root_id_for_matcher
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if root_id.is_none() && body_text.contains("start the Exec nested wake assignment") {
                *root_id = Some(request_thread_id.to_string());
                return Some(0);
            }
            if root_id.as_deref() == Some(request_thread_id) {
                return Some(1);
            }
            drop(root_id);
            let mut child_id = child_id_for_matcher
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if child_id.as_deref() == Some(request_thread_id) {
                return Some(2);
            }
            if body_text.contains("exec child task marker")
                && !body_text.contains("exec grandchild task marker")
            {
                *child_id = Some(request_thread_id.to_string());
                return Some(2);
            }
            if body_text.contains("exec grandchild task marker") {
                return Some(3);
            }
            None
        },
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
        .build_with_streaming_server(&server)
        .await?;

    let root_thread_id = test.session_configured.thread_id;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "start the Exec nested wake assignment".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let root_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, root_thread_id)
            && request
                .to_string()
                .contains("start the Exec nested wake assignment")
    })
    .await
    .context("waiting for the Exec root spawn request")?;
    assert!(namespace_child_tool(&root_request, MULTI_AGENT_V2_NAMESPACE, "wait_agent").is_some());

    let child_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_input_type_with_text(
            request,
            "agent_message",
            "exec child task marker",
        ) && !streaming_request_has_thread_id(request, root_thread_id)
    })
    .await
    .context("waiting for the nested child request")?;
    let child_thread_id = ThreadId::from_string(
        child_request["client_metadata"]["thread_id"]
            .as_str()
            .expect("child thread ID"),
    )?;
    assert!(namespace_child_tool(&child_request, MULTI_AGENT_V2_NAMESPACE, "wait_agent").is_none());
    assert!(namespace_child_tool(&child_request, "clock", "curr_time").is_some());
    assert!(namespace_child_tool(&child_request, "clock", "sleep").is_none());

    let _root_wait_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, root_thread_id)
            && request.to_string().contains("exec-nested-root-spawn")
            && !request.to_string().contains("exec worker result marker")
    })
    .await
    .context("waiting for the Exec root polling request")?;
    let _child_yield_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, child_thread_id)
            && request.to_string().contains("exec-nested-child-spawn")
            && request.to_string().contains("exec grandchild task marker")
            && !request
                .to_string()
                .contains("exec grandchild result marker")
    })
    .await
    .context("waiting for the nested child to yield")?;
    let _grandchild_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_input_type_with_text(
            request,
            "agent_message",
            "exec grandchild task marker",
        ) && !streaming_request_has_thread_id(request, child_thread_id)
            && !streaming_request_has_thread_id(request, root_thread_id)
    })
    .await
    .context("waiting for the nested grandchild request")?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    timeout(Duration::from_secs(5), async {
        while child_thread.agent_status().await != AgentStatus::Waiting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("waiting for the child to yield for its grandchild")?;
    let _ = grandchild_release_tx.send(());

    let child_resume_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, child_thread_id)
            && streaming_request_has_input_type_with_text(
                request,
                "agent_message",
                "exec grandchild result marker",
            )
    })
    .await
    .context("waiting for the nested child wake")?;
    let root_final_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, root_thread_id)
            && streaming_request_has_input_type_with_text(
                request,
                "agent_message",
                "exec worker result marker",
            )
    })
    .await
    .context("waiting for the Exec root result after wait_agent")?;
    let report_count = |request: &Value, marker: &str| {
        request["input"]
            .as_array()
            .expect("wake request has model input")
            .iter()
            .filter(|item| item["type"] == "agent_message" && item.to_string().contains(marker))
            .count()
    };
    pretty_assertions::assert_eq!(
        report_count(&child_resume_request, "exec grandchild result marker"),
        1
    );
    pretty_assertions::assert_eq!(
        report_count(&root_final_request, "exec worker result marker"),
        1
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}
