use super::*;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use core_test_support::streaming_sse::start_routed_streaming_sse_server;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::sync::Mutex;
use test_case::test_case;

#[test_case(None; "untyped")]
#[test_case(Some(CodexErrorInfo::Other); "coded")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_root_quiesces_descendants_and_allows_explicit_followup(
    info: Option<CodexErrorInfo>,
) -> Result<()> {
    let (_hold_root, root_gate) = oneshot::channel();
    let (_hold_child, child_gate) = oneshot::channel();
    let (release_grandchild, grandchild_gate) = oneshot::channel();
    let (release_followup, followup_gate) = oneshot::channel();
    let spawn = |id: &str, task: &str| {
        vec![
            chunk(ev_response_created(id)),
            chunk(ev_function_call_with_namespace(
                id,
                NAMESPACE,
                "spawn_agent",
                &json!({"task_name": task, "message": format!("work on {task}"), "fork_turns": "none"}).to_string(),
            )),
            chunk(ev_completed(id)),
        ]
    };
    let routes = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
    let (server, _) = start_routed_streaming_sse_server(
        vec![
            vec![
                spawn("root-spawn", "worker"),
                vec![gated_chunk(root_gate, vec![ev_completed("root-held")])],
                vec![
                    chunk(ev_response_created("followup")),
                    chunk(ev_reasoning_item_added(
                        "followup-reason",
                        &["active followup"],
                    )),
                    gated_chunk(
                        followup_gate,
                        vec![
                            final_answer_item("followup-msg", "explicit followup"),
                            ev_completed("followup"),
                        ],
                    ),
                ],
            ],
            vec![
                spawn("child-spawn", "grandchild"),
                vec![gated_chunk(child_gate, vec![ev_completed("child-held")])],
            ],
            vec![vec![gated_chunk(
                grandchild_gate,
                vec![
                    ev_response_created("grandchild"),
                    final_answer_item("grandchild-msg", "racing report"),
                    ev_completed("grandchild"),
                ],
            )]],
        ],
        move |_, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let thread_id = request_thread_id(&body)?.to_owned();
            let mut routes = routes.lock().expect("routes lock");
            let next = routes.len();
            Some(*routes.entry(thread_id).or_insert(next))
        },
    )
    .await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_session_source(SessionSource::Exec)
        .with_config(|config| configure_multi_agent(config, Some(AgentPolling::Disabled)))
        .build_with_streaming_server(&server)
        .await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    let TurnInputSubmission::Started { turn_id } = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: ROOT_PROMPT.to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?
    else {
        panic!("root must start");
    };
    let child = test
        .thread_manager
        .get_thread(tokio::time::timeout(Duration::from_secs(10), created.recv()).await??)
        .await?;
    let grandchild = test
        .thread_manager
        .get_thread(
            match tokio::time::timeout(Duration::from_secs(10), created.recv()).await {
                Ok(id) => id?,
                Err(error) => {
                    let requests = streaming_request_bodies(&server).await;
                    let outputs = requests
                        .iter()
                        .filter_map(|request| request["input"].as_array())
                        .flatten()
                        .filter(|item| item["type"] == "function_call_output")
                        .map(|item| item["output"].to_string())
                        .collect::<Vec<_>>();
                    anyhow::bail!("grandchild was not created: {error}; tool outputs: {outputs:?}");
                }
            },
        )
        .await?;
    let error = ErrorEvent {
        message: "host failed this turn".to_owned(),
        codex_error_info: info,
        misalignment: None,
    };
    test.codex
        .submit(Op::FailTurn {
            turn_id: turn_id.clone(),
            error: error.clone(),
        })
        .await?;
    // Race a terminal child report against the failure; it must never reopen the root.
    let _ = release_grandchild.send(());
    let EventMsg::TurnComplete(completed) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(
        (completed.turn_id, completed.error),
        (turn_id.clone(), Some(error.clone()))
    );
    assert!(
        child.wait_until_terminated().now_or_never().is_some()
            && grandchild.wait_until_terminated().now_or_never().is_some(),
        "failed completion must follow descendant termination"
    );
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        vec![test.session_configured.thread_id]
    );
    deliver_child_mail(
        &test.codex,
        "/root/worker",
        "late failure report",
        /*trigger_turn*/ true,
    )
    .await;
    assert_no_turn_starts(&test.codex, SETTLE).await;
    submit_user_text(&test.codex, "explicit followup").await;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::ItemStarted(item) if matches!(item.item, TurnItem::Reasoning(_)))).await;
    // Hold the active followup until Core has processed the stale failure submission.
    test.codex.submit(Op::FailTurn { turn_id, error }).await?;
    test.codex
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides::default())
        .await?;
    release_followup.send(()).expect("followup remains active");
    let EventMsg::TurnComplete(completed) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(completed.error, None);
    let requests = streaming_request_bodies(&server).await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request_thread_id(request)
                == Some(test.session_configured.thread_id.to_string().as_str()))
            .count(),
        3
    );
    server.shutdown().await;
    Ok(())
}

#[derive(Default)]
struct ChildStartupBarrier {
    started: tokio::sync::Notify,
    cleanup_entered: tokio::sync::Notify,
    release_cleanup: tokio::sync::Notify,
}
struct ChildCleanupMarker;
impl codex_extension_api::ThreadLifecycleContributor<Config> for ChildStartupBarrier {
    fn on_thread_start<'a>(
        &'a self,
        input: codex_extension_api::ThreadStartInput<'a, Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.session_source.is_non_root_agent() {
                input.thread_store.insert(ChildCleanupMarker);
            }
        })
    }
    fn on_thread_ready<'a>(
        &'a self,
        input: codex_extension_api::ThreadReadyInput<'a, Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.session_source.is_non_root_agent() {
                self.started.notify_one();
                std::future::pending::<()>().await;
            }
        })
    }
    fn on_thread_stop<'a>(
        &'a self,
        input: codex_extension_api::ThreadStopInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.thread_store.get::<ChildCleanupMarker>().is_some() {
                self.cleanup_entered.notify_one();
                self.release_cleanup.notified().await;
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_root_waits_for_reserved_child_cleanup_after_rollback() -> Result<()> {
    let barrier = Arc::new(ChildStartupBarrier::default());
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(barrier.clone());
    let assigned = Arc::new(Mutex::new(Vec::new()));
    let assigned_for_factory = Arc::clone(&assigned);
    let (server, _) = start_streaming_sse_server(vec![vec![
        chunk(ev_response_created("spawn")),
        chunk(ev_function_call_with_namespace(
            "spawn",
            NAMESPACE,
            "spawn_agent",
            &json!({"task_name": "stalled", "message": "start child"}).to_string(),
        )),
        chunk(ev_completed("spawn")),
    ]])
    .await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_session_source(SessionSource::Exec)
        .with_extensions(Arc::new(extensions.build()))
        .with_thread_manager(move |manager| {
            manager.with_thread_id_generator(move || {
                let id = ThreadId::new();
                assigned_for_factory.lock().unwrap().push(id);
                id
            })
        })
        .with_config(|config| {
            configure_multi_agent(config, Some(AgentPolling::Disabled));
            config.experimental_thread_store = codex_core::config::ThreadStoreConfig::Local;
        })
        .build_with_streaming_server(&server)
        .await?;
    let TurnInputSubmission::Started { turn_id } = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: ROOT_PROMPT.to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?
    else {
        panic!("root must start");
    };
    tokio::time::timeout(Duration::from_secs(10), barrier.started.notified()).await?;
    let child = *assigned.lock().unwrap().last().expect("reserved child ID");
    assert_ne!(child, test.session_configured.thread_id);
    test.codex
        .submit(Op::FailTurn {
            turn_id,
            error: ErrorEvent {
                message: "fail while child starts".to_owned(),
                codex_error_info: None,
                misalignment: None,
            },
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(10), barrier.cleanup_entered.notified()).await?;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_turn_complete(&test.codex)
        )
        .await
        .is_err(),
        "failed completion must wait for async rollback cleanup"
    );
    barrier.release_cleanup.notify_one();
    tokio::time::timeout(Duration::from_secs(10), wait_for_turn_complete(&test.codex)).await?;
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        vec![test.session_configured.thread_id]
    );
    assert!(
        matches!(
            test.thread_store.flush_thread(child).await,
            Err(codex_thread_store::ThreadStoreError::ThreadNotFound { .. })
        ),
        "cancelled startup must release persistence before failed completion"
    );
    assert_no_turn_starts(&test.codex, SETTLE).await;
    server.shutdown().await;
    Ok(())
}

#[derive(Default)]
struct ChildFinalizationBarrier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl codex_extension_api::ThreadLifecycleContributor<Config> for ChildFinalizationBarrier {
    fn on_thread_start<'a>(
        &'a self,
        input: codex_extension_api::ThreadStartInput<'a, Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.session_source.is_non_root_agent() {
                input.thread_store.insert(ChildCleanupMarker);
            }
        })
    }
}
impl codex_extension_api::TurnLifecycleContributor for ChildFinalizationBarrier {
    fn on_turn_stop<'a>(
        &'a self,
        input: codex_extension_api::TurnStopInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.thread_store.get::<ChildCleanupMarker>().is_some() {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_root_waits_for_detached_child_finalization() -> Result<()> {
    let barrier = Arc::new(ChildFinalizationBarrier::default());
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(barrier.clone());
    extensions.turn_lifecycle_contributor(barrier.clone());
    let (_hold_root, root_gate) = oneshot::channel();
    let routes = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
    let (server, _) = start_routed_streaming_sse_server(
        vec![
            vec![
                vec![
                    chunk(ev_response_created("spawn")),
                    chunk(ev_function_call_with_namespace(
                        "spawn",
                        NAMESPACE,
                        "spawn_agent",
                        &json!({"task_name": "worker", "message": "finish child"}).to_string(),
                    )),
                    chunk(ev_completed("spawn")),
                ],
                vec![gated_chunk(root_gate, vec![ev_completed("root-held")])],
            ],
            vec![final_answer_chunks("child", "finished child")],
        ],
        move |_, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let thread_id = request_thread_id(&body)?.to_owned();
            let mut routes = routes.lock().expect("routes lock");
            let next = routes.len();
            Some(*routes.entry(thread_id).or_insert(next))
        },
    )
    .await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_session_source(SessionSource::Exec)
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| configure_multi_agent(config, Some(AgentPolling::Disabled)))
        .build_with_streaming_server(&server)
        .await?;
    let TurnInputSubmission::Started { turn_id } = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: ROOT_PROMPT.to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?
    else {
        panic!("root must start");
    };
    tokio::time::timeout(Duration::from_secs(10), barrier.entered.notified()).await?;
    test.codex
        .submit(Op::FailTurn {
            turn_id,
            error: ErrorEvent {
                message: "fail during child finalization".to_owned(),
                codex_error_info: None,
                misalignment: None,
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_turn_complete(&test.codex)
        )
        .await
        .is_err(),
        "failure must wait for detached finalizer"
    );
    barrier.release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), wait_for_turn_complete(&test.codex)).await?;
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        vec![test.session_configured.thread_id]
    );
    assert_no_turn_starts(&test.codex, SETTLE).await;
    server.shutdown().await;
    Ok(())
}
