use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_wake_reloads_waiting_parent_after_grandchild_report() -> Result<()> {
    let (grandchild_release_tx, grandchild_release_rx) = oneshot::channel();
    let (child_resume_release_tx, child_resume_release_rx) = oneshot::channel();
    let (pressure_release_tx, pressure_release_rx) = oneshot::channel();
    let root_spawn_args = serde_json::to_string(&json!({
        "message": "child task marker",
        "task_name": "worker",
    }))?;
    let child_spawn_args = serde_json::to_string(&json!({
        "message": "grandchild task marker",
        "task_name": "grandchild",
    }))?;
    let pressure_spawn_args = serde_json::to_string(&json!({
        "message": "capacity pressure task marker",
        "task_name": "pressure",
    }))?;

    let root_id = Arc::new(Mutex::new(None::<String>));
    let child_id = Arc::new(Mutex::new(None::<String>));
    let root_id_for_matcher = Arc::clone(&root_id);
    let child_id_for_matcher = Arc::clone(&child_id);
    let (server, _completions) = start_routed_streaming_sse_server(vec![
        vec![vec![
            streaming_event_chunk(ev_response_created("nested-root-start")),
            streaming_event_chunk(ev_function_call_with_namespace(
                "nested-root-spawn",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &root_spawn_args,
            )),
            streaming_event_chunk(ev_completed("nested-root-start")),
        ]],
        vec![
            vec![
                streaming_event_chunk(ev_response_created("nested-root-after-spawn")),
                streaming_event_chunk(ev_assistant_message(
                    "nested-root-waiting",
                    "root yielded for child results",
                )),
                streaming_event_chunk(ev_completed("nested-root-after-spawn")),
            ],
            vec![
                streaming_event_chunk(ev_response_created("nested-root-pressure")),
                streaming_event_chunk(ev_function_call_with_namespace(
                    "nested-root-spawn-pressure",
                    MULTI_AGENT_V2_NAMESPACE,
                    "spawn_agent",
                    &pressure_spawn_args,
                )),
                streaming_event_chunk(ev_completed("nested-root-pressure")),
            ],
            vec![
                streaming_event_chunk(ev_response_created("nested-root-pressure-done")),
                streaming_event_chunk(ev_assistant_message(
                    "nested-root-pressure-yield",
                    "root delegated pressure task",
                )),
                streaming_event_chunk(ev_completed("nested-root-pressure-done")),
            ],
        ],
        vec![vec![
            streaming_event_chunk(ev_response_created("nested-child-start")),
            streaming_event_chunk(ev_function_call_with_namespace(
                "nested-child-spawn",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &child_spawn_args,
            )),
            streaming_event_chunk(ev_completed("nested-child-start")),
        ]],
        vec![
            vec![
                streaming_event_chunk(ev_response_created("nested-child-after-spawn")),
                streaming_event_chunk(ev_assistant_message(
                    "nested-child-waiting",
                    "child yielded for grandchild result",
                )),
                streaming_event_chunk(ev_completed("nested-child-after-spawn")),
            ],
            vec![
                streaming_event_chunk(ev_response_created("nested-child-resume")),
                streaming_event_chunk(ev_assistant_message(
                    "nested-child-final",
                    "worker result marker",
                )),
                gated_streaming_chunk(
                    child_resume_release_rx,
                    vec![ev_completed("nested-child-resume")],
                ),
            ],
        ],
        vec![vec![
            streaming_event_chunk(ev_response_created("nested-grandchild-start")),
            streaming_event_chunk(ev_assistant_message(
                "nested-grandchild-message",
                "grandchild result marker",
            )),
            gated_streaming_chunk(
                grandchild_release_rx,
                vec![ev_completed("nested-grandchild-start")],
            ),
        ]],
        vec![vec![
            streaming_event_chunk(ev_response_created("nested-pressure-child-start")),
            streaming_event_chunk(ev_assistant_message(
                "nested-pressure-child-progress",
                "pressure child is holding its slot",
            )),
            gated_streaming_chunk(
                pressure_release_rx,
                vec![
                    ev_assistant_message(
                        "nested-pressure-child-result",
                        "pressure child done",
                    ),
                    ev_completed("nested-pressure-child-start"),
                ],
            ),
        ]],
    ], move |_headers, body| {
        let body: Value = serde_json::from_slice(body).ok()?;
        let request_thread_id = body["client_metadata"]["thread_id"].as_str()?;
        let body_text = body.to_string();
        let mut root_id = root_id_for_matcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if root_id.is_none() && body_text.contains("start the nested wake assignment") {
            *root_id = Some(request_thread_id.to_string());
            return Some(0);
        }
        if root_id.as_deref() == Some(request_thread_id) {
            return Some(1);
        }
        drop(root_id);
        if body_text.contains("capacity pressure task marker") {
            return Some(5);
        }
        let mut child_id = child_id_for_matcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if child_id.as_deref() == Some(request_thread_id) {
            return Some(3);
        }
        if body_text.contains("child task marker")
            && !body_text.contains("grandchild task marker")
        {
            *child_id = Some(request_thread_id.to_string());
            return Some(2);
        }
        if body_text.contains("grandchild task marker") {
            return Some(4);
        }
        None
    })
    .await;
    let test = test_codex()
        .with_model("koffing")
        .with_session_source(SessionSource::Cli)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::CurrentTimeReminder)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::SleepTool)
                .expect("test config should allow feature update");
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
    test.submit_turn("start the nested wake assignment").await?;
    let _root_initial_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, root_thread_id)
            && request.to_string().contains("start the nested wake assignment")
    })
    .await
    .context("waiting for the initial root request")?;
    let _root_yield_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, root_thread_id)
            && request.to_string().contains("nested-root-spawn")
            && !request.to_string().contains("worker result marker")
    })
    .await
    .context("waiting for the root request after spawning its child")?;
    let child_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_input_type_with_text(request, "agent_message", "child task marker")
            && !streaming_request_has_thread_id(request, root_thread_id)
    })
    .await
    .context("waiting for the child's initial request")?;
    let child_thread_id = ThreadId::from_string(
        child_request["client_metadata"]["thread_id"]
            .as_str()
            .expect("child thread ID"),
    )?;
    assert_ne!(child_thread_id, root_thread_id);
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let child_spawn_tool_exposed = namespace_child_tool(
        &child_request,
        MULTI_AGENT_V2_NAMESPACE,
        "spawn_agent",
    )
    .is_some();
    assert!(child_spawn_tool_exposed);

    let _child_yield_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, child_thread_id)
            && request.to_string().contains("nested-child-spawn")
            && !request.to_string().contains("grandchild result marker")
    })
    .await
    .context("waiting for the child request after spawning its grandchild")?;
    let grandchild_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_input_type_with_text(
            request,
            "agent_message",
            "grandchild task marker",
        ) && !streaming_request_has_thread_id(request, child_thread_id)
    })
    .await
    .context("waiting for the grandchild's request")?;
    let grandchild_thread_id = ThreadId::from_string(
        grandchild_request["client_metadata"]["thread_id"]
            .as_str()
            .expect("grandchild thread ID"),
    )?;
    assert_ne!(grandchild_thread_id, child_thread_id);
    assert_ne!(grandchild_thread_id, root_thread_id);
    timeout(Duration::from_secs(5), async {
        loop {
            if child_thread.agent_status().await == AgentStatus::Waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("child must wait for the gated grandchild result")?;
    timeout(Duration::from_secs(5), async {
        while test.codex.agent_status().await != AgentStatus::Waiting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("root must be waiting before starting the pressure turn")?;
    drop(child_thread);

    assert!(namespace_child_tool(&child_request, "clock", "curr_time").is_some());
    assert!(namespace_child_tool(&child_request, "clock", "sleep").is_none());
    assert!(
        namespace_child_tool(&child_request, MULTI_AGENT_V2_NAMESPACE, "wait_agent").is_none()
    );

    test.submit_text_turn("start capacity pressure nested wake")
        .await?;
    let _root_pressure_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, root_thread_id)
            && request
                .to_string()
                .contains("start capacity pressure nested wake")
    })
    .await
    .context("waiting for the root pressure request")?;
    let pressure_child_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_input_type_with_text(
            request,
            "agent_message",
            "capacity pressure task marker",
        ) && !streaming_request_has_thread_id(request, root_thread_id)
    })
    .await
    .context("waiting for the first pressure child's request")?;
    let pressure_child_thread_id = ThreadId::from_string(
        pressure_child_request["client_metadata"]["thread_id"]
            .as_str()
            .expect("pressure child thread ID"),
    )?;
    let pressure_child = test
        .thread_manager
        .get_thread(pressure_child_thread_id)
        .await?;
    let grandchild_thread = test.thread_manager.get_thread(grandchild_thread_id).await?;
    assert_eq!(pressure_child.agent_status().await, AgentStatus::Running);
    assert_eq!(grandchild_thread.agent_status().await, AgentStatus::Running);
    assert!(
        test.thread_manager.get_thread(child_thread_id).await.is_err(),
        "capacity pressure should evict the idle Waiting parent"
    );
    test.thread_manager.remove_thread(&root_thread_id).await;
    assert!(test.thread_manager.get_thread(root_thread_id).await.is_err());
    let _ = grandchild_release_tx.send(());

    let child_final_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, child_thread_id)
            && streaming_request_has_input_type_with_text(
                request,
                "agent_message",
                "grandchild result marker",
            )
    })
    .await
    .context("waiting for the reloaded child's report-processing request")?;
    let reloaded_child = test.thread_manager.get_thread(child_thread_id).await?;
    assert_eq!(reloaded_child.agent_status().await, AgentStatus::Running);
    assert_eq!(pressure_child.agent_status().await, AgentStatus::Running);
    let _ = child_resume_release_tx.send(());
    let _ = pressure_release_tx.send(());
    let child_report_count = child_final_request["input"]
        .as_array()
        .expect("child resumed request has model input")
        .iter()
        .filter(|item| {
            item["type"] == "agent_message"
                && item.to_string().contains("grandchild result marker")
        })
        .count();
    assert_eq!(child_report_count, 1);

    assert!(test.thread_manager.get_thread(root_thread_id).await.is_err());
    timeout(Duration::from_secs(5), async {
        loop {
            if reloaded_child.agent_status().await
                == AgentStatus::Completed(Some("worker result marker".to_string()))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("child should finish after accepting the grandchild report")?;
    let _ = test.codex.shutdown_and_wait().await;
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_wake_reports_final_child_result_to_root_once() -> Result<()> {
    let (grandchild_release_tx, grandchild_release_rx) = oneshot::channel();
    let root_spawn_args = serde_json::to_string(&json!({
        "message": "upward child task marker",
        "task_name": "worker",
    }))?;
    let child_spawn_args = serde_json::to_string(&json!({
        "message": "upward grandchild task marker",
        "task_name": "grandchild",
    }))?;
    let worker_result = format!("worker result marker {}", "🧪résultat ".repeat(10_000));
    let root_id = Arc::new(Mutex::new(None::<String>));
    let child_id = Arc::new(Mutex::new(None::<String>));
    let root_id_for_matcher = Arc::clone(&root_id);
    let child_id_for_matcher = Arc::clone(&child_id);
    let (server, _completions) = start_routed_streaming_sse_server(
        vec![
            vec![vec![
                streaming_event_chunk(ev_response_created("upward-root-start")),
                streaming_event_chunk(ev_function_call_with_namespace(
                    "upward-root-spawn",
                    MULTI_AGENT_V2_NAMESPACE,
                    "spawn_agent",
                    &root_spawn_args,
                )),
                streaming_event_chunk(ev_completed("upward-root-start")),
            ]],
            vec![
                vec![
                    streaming_event_chunk(ev_response_created("upward-root-yield")),
                    streaming_event_chunk(ev_assistant_message(
                        "upward-root-waiting",
                        "root waits for the worker result",
                    )),
                    streaming_event_chunk(ev_completed("upward-root-yield")),
                ],
                vec![
                    streaming_event_chunk(ev_response_created("upward-root-resume")),
                    streaming_event_chunk(ev_assistant_message(
                        "upward-root-final",
                        "root accepted the worker result",
                    )),
                    streaming_event_chunk(ev_completed("upward-root-resume")),
                ],
            ],
            vec![
                vec![
                    streaming_event_chunk(ev_response_created("upward-child-start")),
                    streaming_event_chunk(ev_function_call_with_namespace(
                        "upward-child-spawn",
                        MULTI_AGENT_V2_NAMESPACE,
                        "spawn_agent",
                        &child_spawn_args,
                    )),
                    streaming_event_chunk(ev_completed("upward-child-start")),
                ],
                vec![
                    streaming_event_chunk(ev_response_created("upward-child-yield")),
                    streaming_event_chunk(ev_assistant_message(
                        "upward-child-waiting",
                        "child waits for the grandchild result",
                    )),
                    streaming_event_chunk(ev_completed("upward-child-yield")),
                ],
                vec![
                    streaming_event_chunk(ev_response_created("upward-child-resume")),
                    streaming_event_chunk(ev_assistant_message(
                        "upward-child-final",
                        &worker_result,
                    )),
                    streaming_event_chunk(ev_completed("upward-child-resume")),
                ],
            ],
            vec![vec![
                streaming_event_chunk(ev_response_created("upward-grandchild-start")),
                streaming_event_chunk(ev_assistant_message(
                    "upward-grandchild-result",
                    "grandchild result marker",
                )),
                gated_streaming_chunk(
                    grandchild_release_rx,
                    vec![ev_completed("upward-grandchild-start")],
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
            if root_id.is_none() && body_text.contains("start nested upward report") {
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
            if body_text.contains("upward child task marker")
                && !body_text.contains("upward grandchild task marker")
            {
                *child_id = Some(request_thread_id.to_string());
                return Some(2);
            }
            if body_text.contains("upward grandchild task marker") {
                return Some(3);
            }
            None
        },
    )
    .await;
    let test = test_codex()
        .with_model("koffing")
        .with_session_source(SessionSource::Cli)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Disabled;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_streaming_server(&server)
        .await?;
    let root_thread_id = test.session_configured.thread_id;
    test.submit_turn("start nested upward report").await?;
    let child_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_input_type_with_text(request, "agent_message", "upward child task marker")
            && !streaming_request_has_thread_id(request, root_thread_id)
    })
    .await
    .context("waiting for the nested child request")?;
    let child_thread_id = ThreadId::from_string(
        child_request["client_metadata"]["thread_id"]
            .as_str()
            .expect("child thread ID"),
    )?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let grandchild_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_input_type_with_text(
            request,
            "agent_message",
            "upward grandchild task marker",
        )
    })
    .await
    .context("waiting for the nested grandchild request")?;
    let grandchild_thread_id = ThreadId::from_string(
        grandchild_request["client_metadata"]["thread_id"]
            .as_str()
            .expect("grandchild thread ID"),
    )?;
    timeout(Duration::from_secs(5), async {
        while child_thread.agent_status().await != AgentStatus::Waiting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("waiting for the child to yield for its grandchild")?;
    let _ = grandchild_release_tx.send(());
    let child_final_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, child_thread_id)
            && streaming_request_has_input_type_with_text(
                request,
                "agent_message",
                "grandchild result marker",
            )
    })
    .await
    .context("waiting for the child to process the grandchild report")?;
    let root_final_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, root_thread_id)
            && streaming_request_has_input_type_with_text(
                request,
                "agent_message",
                "worker result marker",
            )
    })
    .await
    .context("waiting for the child result in the root request")?;
    let worker_report_count = root_final_request["input"]
        .as_array()
        .expect("root wake request has model input")
        .iter()
        .filter(|item| {
            item["type"] == "agent_message" && item.to_string().contains("worker result marker")
        })
        .count();
    assert_eq!(worker_report_count, 1);
    let grandchild_report_count = child_final_request["input"]
        .as_array()
        .expect("child wake request has model input")
        .iter()
        .filter(|item| {
            item["type"] == "agent_message" && item.to_string().contains("grandchild result marker")
        })
        .count();
    assert_eq!(grandchild_report_count, 1);
    assert_ne!(grandchild_thread_id, root_thread_id);
    assert_ne!(grandchild_thread_id, child_thread_id);
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let root_requests = server
        .requests()
        .await
        .into_iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request).ok())
        .filter(|request| streaming_request_has_thread_id(request, root_thread_id))
        .collect::<Vec<_>>();
    assert_eq!(root_requests.len(), 3);
    let worker_report_count = root_requests
        .iter()
        .flat_map(|request| request["input"].as_array().into_iter().flatten())
        .filter(|item| {
            item["type"] == "agent_message" && item.to_string().contains("worker result marker")
        })
        .count();
    assert_eq!(worker_report_count, 1);
    let worker_report = root_requests
        .iter()
        .flat_map(|request| request["input"].as_array().into_iter().flatten())
        .find(|item| {
            item["type"] == "agent_message" && item.to_string().contains("worker result marker")
        })
        .expect("root request contains the worker report");
    assert!(
        codex_utils_output_truncation::approx_token_count(&worker_report.to_string()) <= 1_000
    );
    let _ = test.codex.shutdown_and_wait().await;
    server.shutdown().await;
    Ok(())
}
