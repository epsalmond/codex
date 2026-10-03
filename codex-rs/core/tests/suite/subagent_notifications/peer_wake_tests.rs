use super::*;
use codex_extension_api::ExtensionRegistryBuilder;
use core_test_support::ThreadIdle;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_worker_queues_message_until_followup_without_replaying_it() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const START_PROMPT: &str = "start completed worker regression";
    const INITIAL_TASK: &str = "completed worker initial task";
    const QUEUED_MESSAGE: &str = "completed worker queued payload";
    const FOLLOWUP_TASK: &str = "completed worker followup task";
    const NEXT_TASK: &str = "completed worker next task";
    let (initial_release_tx, initial_release_rx) = oneshot::channel();
    let (followup_release_tx, followup_release_rx) = oneshot::channel();
    let (next_release_tx, next_release_rx) = oneshot::channel();
    let spawn = serde_json::to_string(&json!({
        "message": INITIAL_TASK, "task_name": "worker", "fork_turns": "none",
    }))?;
    let send = serde_json::to_string(&json!({
        "target": "worker", "message": QUEUED_MESSAGE,
    }))?;
    let followup = serde_json::to_string(&json!({
        "target": "worker", "message": FOLLOWUP_TASK,
    }))?;
    let next = serde_json::to_string(&json!({
        "target": "worker", "message": NEXT_TASK,
    }))?;
    let root_responses = [
        ("completed-spawn", "spawn_agent", spawn),
        ("completed-send", "send_message", send),
        ("completed-followup", "followup_task", followup),
        ("completed-next", "followup_task", next),
    ]
    .into_iter()
    .flat_map(|(call, tool, arguments)| {
        vec![
            vec![StreamingSseChunk {
                gate: None,
                body: sse(peer_call(call, call, tool, &arguments)),
            }],
            vec![StreamingSseChunk {
                gate: None,
                body: sse(peer_message(
                    &format!("{call}-accepted"),
                    "request accepted",
                )),
            }],
        ]
    })
    .collect();
    let worker_responses = [
        (
            initial_release_rx,
            "completed-worker-initial",
            "completed worker initial result",
        ),
        (
            followup_release_rx,
            "completed-worker-followup",
            "completed worker followup result",
        ),
        (
            next_release_rx,
            "completed-worker-next",
            "completed worker next result",
        ),
    ]
    .into_iter()
    .map(|(gate, response, message)| {
        vec![gated_streaming_chunk(gate, peer_message(response, message))]
    })
    .collect();
    let (server, _) = start_routed_streaming_sse_server(
        vec![root_responses, worker_responses],
        |_headers, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let text = body.to_string();
            if text.contains(START_PROMPT) {
                Some(0)
            } else if text.contains(INITIAL_TASK) {
                Some(1)
            } else {
                None
            }
        },
    )
    .await;
    let base_url = format!("{}/v1", server.uri());
    let mock_server = start_mock_server().await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            for feature in [Feature::Collab, Feature::MultiAgentV2] {
                config
                    .features
                    .enable(feature)
                    .expect("enable test feature");
            }
            config.multi_agent_v2.agent_polling = AgentPolling::Disabled;
            config.model_provider.base_url = Some(base_url);
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&mock_server)
        .await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.submit_turn(START_PROMPT).await?;
    ThreadIdle::wait(&test.codex).await;
    let worker_thread_id = created.recv().await?;
    let worker = test.thread_manager.get_thread(worker_thread_id).await?;
    initial_release_tx
        .send(())
        .expect("finish initial worker task after root is idle");
    wait_for_event(worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    ThreadIdle::wait(&worker).await;
    let completed_status =
        AgentStatus::Completed(Some("completed worker initial result".to_string()));
    assert_eq!(worker.agent_status().await, completed_status);

    test.submit_turn("queue mail for completed worker").await?;
    ThreadIdle::wait(&test.codex).await;
    let receipt_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, test.session_configured.thread_id)
            && request["input"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["type"] == "function_call_output" && item["call_id"] == "completed-send"
                })
            })
    })
    .await?;
    let receipt = receipt_request["input"]
        .as_array()
        .expect("request input")
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "completed-send")
        .expect("send_message receipt");
    insta::assert_snapshot!(receipt["output"].as_str().expect("receipt text"), @"Message accepted into the agent's queue. This acceptance does not confirm a new turn started; use followup_task to request work from an idle agent.");
    // A replied operation runs after the queued message in the worker's serial loop.
    // This barrier ensures the status and request count observe mailbox processing.
    worker
        .update_thread_settings(ThreadSettingsOverrides::default())
        .await?;
    assert_eq!(worker.agent_status().await, completed_status);
    assert_eq!(
        server
            .requests()
            .await
            .iter()
            .filter(|body| {
                let request: Value = serde_json::from_slice(body).expect("request JSON");
                streaming_request_has_thread_id(&request, worker_thread_id)
            })
            .count(),
        1
    );

    let mut queued_items = None;
    for (prompt, task, release) in [
        (
            "resume completed worker",
            FOLLOWUP_TASK,
            followup_release_tx,
        ),
        ("give worker another turn", NEXT_TASK, next_release_tx),
    ] {
        test.submit_turn(prompt).await?;
        ThreadIdle::wait(&test.codex).await;
        let request = wait_for_streaming_request_matching(&server, |request| {
            streaming_request_has_thread_id(request, worker_thread_id)
                && streaming_request_has_input_type_with_text(request, "agent_message", task)
        })
        .await?;
        let messages: Vec<_> = request["input"]
            .as_array()
            .expect("worker input")
            .iter()
            .filter(|item| item["type"] == "agent_message")
            .collect();
        let payloads: Vec<_> = messages
            .iter()
            .filter(|item| item.to_string().contains(QUEUED_MESSAGE))
            .map(|item| (**item).clone())
            .collect();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].to_string().matches(QUEUED_MESSAGE).count(), 1);
        assert_eq!(payloads[0]["author"], json!("/root"));
        assert_eq!(payloads[0]["recipient"], json!("/root/worker"));
        assert_eq!(
            messages
                .iter()
                .filter(|item| item.to_string().contains(task))
                .count(),
            1
        );
        if let Some(previous) = &queued_items {
            // Earlier history is sent again, but queued mail must not be inserted a second time.
            assert_eq!(&payloads, previous);
        }
        queued_items = Some(payloads);
        release
            .send(())
            .expect("finish worker task after root is idle");
        wait_for_event(worker.as_ref(), |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        ThreadIdle::wait(&worker).await;
    }
    Ok(())
}

#[test_case(false; "retained report")]
#[test_case(true; "consumed report")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_followup_wakes_completed_worker_and_reports_to_root(
    consume_report: bool,
) -> Result<()> {
    let (worker_release_tx, worker_release_rx) = oneshot::channel();
    let (spawn_release_tx, spawn_release_rx) = oneshot::channel();
    let (requester_release_tx, requester_release_rx) = oneshot::channel();
    let (followup_release_tx, followup_release_rx) = oneshot::channel();
    let (accept_release_tx, accept_release_rx) = oneshot::channel();
    let (root_release_tx, root_release_rx) = oneshot::channel();
    let spawn_worker = serde_json::to_string(&json!({
        "message": "peer worker initial task", "task_name": "worker", "fork_turns": "none",
    }))?;
    let spawn_requester = serde_json::to_string(&json!({
        "message": "peer requester task", "task_name": "requester", "fork_turns": "none",
    }))?;
    let followup = serde_json::to_string(&json!({
        "target": "/root/worker", "message": "peer worker followup task",
    }))?;
    let root_id = Arc::new(Mutex::new(None::<String>));
    let root_id_for_matcher = Arc::clone(&root_id);
    let (server, _) = start_routed_streaming_sse_server(
        vec![
            vec![vec![StreamingSseChunk {
                gate: None,
                body: sse(peer_call(
                    "peer-root-start",
                    "peer-spawn-worker",
                    "spawn_agent",
                    &spawn_worker,
                )),
            }]],
            vec![
                vec![gated_streaming_chunk(
                    spawn_release_rx,
                    peer_call(
                        "peer-root-spawn-requester",
                        "peer-spawn-requester",
                        "spawn_agent",
                        &spawn_requester,
                    ),
                )],
                vec![
                    gated_streaming_chunk(
                        accept_release_rx,
                        vec![
                            ev_response_created("peer-root-holding"),
                            ev_assistant_message(
                                "peer-root-holding-message",
                                "root is awaiting peers",
                            ),
                        ],
                    ),
                    gated_streaming_chunk(root_release_rx, vec![ev_completed("peer-root-holding")]),
                ],
                vec![StreamingSseChunk {
                    gate: None,
                    body: sse(peer_message(
                        "peer-root-final",
                        "root received peer results",
                    )),
                }],
            ],
            vec![
                vec![gated_streaming_chunk(
                    worker_release_rx,
                    peer_message("peer-worker-initial", "peer worker initial result"),
                )],
                vec![gated_streaming_chunk(
                    followup_release_rx,
                    peer_message("peer-worker-followup", "peer worker followup result"),
                )],
            ],
            vec![
                vec![gated_streaming_chunk(
                    requester_release_rx,
                    peer_call(
                        "peer-requester-start",
                        "peer-followup-call",
                        "followup_task",
                        &followup,
                    ),
                )],
                vec![StreamingSseChunk {
                    gate: None,
                    body: sse(peer_message(
                        "peer-requester-final",
                        "peer requester finished",
                    )),
                }],
            ],
        ],
        move |_headers, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let thread_id = body["client_metadata"]["thread_id"].as_str()?;
            let text = body.to_string();
            let mut root = root_id_for_matcher
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if root.is_none() && text.contains("start peer wake regression") {
                *root = Some(thread_id.to_string());
                return Some(0);
            }
            if root.as_deref() == Some(thread_id) {
                return Some(1);
            }
            if text.contains("peer requester task") {
                Some(3)
            } else if text.contains("peer worker initial task") {
                Some(2)
            } else {
                None
            }
        },
    )
    .await;
    let test = test_codex()
        .with_model("koffing")
        .with_session_source(SessionSource::Cli)
        .with_config(|config| {
            for feature in [Feature::Collab, Feature::MultiAgentV2] {
                config
                    .features
                    .enable(feature)
                    .expect("enable test feature");
            }
            config.multi_agent_v2.agent_polling = AgentPolling::Disabled;
            config.multi_agent_v2.max_concurrent_threads_per_session = 3;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_streaming_server(&server)
        .await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.codex
        .start_or_steer_turn(codex_core::TurnInputRequest::user_input(vec![
            UserInput::Text {
                text: "start peer wake regression".to_string(),
                text_elements: Vec::new(),
            },
        ]))
        .await?;
    let worker_thread_id = created.recv().await?;
    let worker = test.thread_manager.get_thread(worker_thread_id).await?;
    wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, test.session_configured.thread_id)
            && request.to_string().contains("peer-spawn-worker")
    })
    .await?;
    worker_release_tx.send(()).expect("release worker");
    wait_for_event(worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    spawn_release_tx.send(()).expect("spawn requester");
    let requester = test
        .thread_manager
        .get_thread(created.recv().await?)
        .await?;
    let requester_turn_id = wait_for_event_match(requester.as_ref(), |event| match event {
        EventMsg::TurnStarted(started) => Some(started.turn_id.clone()),
        _ => None,
    })
    .await;
    let holding_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, test.session_configured.thread_id)
            && request.to_string().contains("peer-spawn-requester")
    })
    .await?;
    assert!(streaming_request_has_input_type_with_text(
        &holding_request,
        "agent_message",
        "peer worker initial result",
    ));
    let mut accept_release_tx = Some(accept_release_tx);
    if consume_report {
        accept_release_tx
            .take()
            .expect("accept sender")
            .send(())
            .expect("accept old worker report");
        wait_for_event(test.codex.as_ref(), |event| {
            matches!(event,
                EventMsg::AgentMessage(message) if message.message == "root is awaiting peers"
            )
        })
        .await;
    }
    requester_release_tx
        .send(())
        .expect("request peer followup");
    wait_for_event(requester.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let followup_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, worker_thread_id)
            && streaming_request_has_input_type_with_text(
                request,
                "agent_message",
                "peer worker followup task",
            )
    })
    .await?;
    assert!(
        followup_request
            .to_string()
            .contains("Sender: /root/requester")
    );
    followup_release_tx
        .send(())
        .expect("complete followup after requester finishes");
    wait_for_event(worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let completion = wait_for_event_match(requester.as_ref(), |event| match event {
        EventMsg::ItemCompleted(completed)
            if matches!(&completed.item,
                TurnItem::SubAgentActivity(activity)
                    if activity.kind == SubAgentActivityKind::Completed
                        && activity.agent_thread_id == worker_thread_id
            ) =>
        {
            Some(completed.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(completion.turn_id, requester_turn_id);
    if !consume_report {
        accept_release_tx
            .take()
            .expect("accept sender")
            .send(())
            .expect("accept retained worker report");
    }
    root_release_tx.send(()).expect("release root");
    let result = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, test.session_configured.thread_id)
            && streaming_request_has_input_type_with_text(
                request,
                "agent_message",
                "peer worker followup result",
            )
    })
    .await?;
    assert!(result.to_string().contains("Sender: /root/worker"));
    let requests = server.requests().await;
    assert_eq!(
        requests
            .iter()
            .filter(|body| {
                let request: Value = serde_json::from_slice(body).expect("request JSON");
                streaming_request_has_thread_id(&request, worker_thread_id)
                    && streaming_request_has_input_type_with_text(
                        &request,
                        "agent_message",
                        "peer worker followup task",
                    )
            })
            .count(),
        1
    );
    Ok(())
}

fn peer_call(response: &str, call: &str, tool: &str, arguments: &str) -> Vec<Value> {
    vec![
        ev_response_created(response),
        ev_function_call_with_namespace(call, MULTI_AGENT_V2_NAMESPACE, tool, arguments),
        ev_completed(response),
    ]
}

fn peer_message(response: &str, message: &str) -> Vec<Value> {
    vec![
        ev_response_created(response),
        ev_assistant_message(response, message),
        ev_completed(response),
    ]
}
