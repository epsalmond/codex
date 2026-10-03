use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

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
