use super::*;
use anyhow::Context;
use codex_protocol::protocol::ErrorEvent;
use core_test_support::ThreadIdle;
use core_test_support::streaming_sse::start_routed_streaming_sse_server;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
struct ReloadBarrier {
    starts: Mutex<usize>,
    paths: Mutex<Vec<String>>,
    ready: tokio::sync::Notify,
    cleanup: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
struct ReloadMarker;
impl codex_extension_api::ThreadLifecycleContributor<Config> for ReloadBarrier {
    fn on_thread_start<'a>(
        &'a self,
        input: codex_extension_api::ThreadStartInput<'a, Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.paths.lock().expect("paths lock").push(
                input
                    .session_source
                    .get_agent_path()
                    .map(|path| path.to_string())
                    .unwrap_or_default(),
            );
            if input
                .session_source
                .get_agent_path()
                .is_some_and(|path| path.to_string() == "/root/worker")
            {
                let mut starts = self.starts.lock().expect("starts lock");
                *starts += 1;
                if *starts == 2 {
                    input.thread_store.insert(ReloadMarker);
                }
            }
        })
    }
    fn on_thread_ready<'a>(
        &'a self,
        input: codex_extension_api::ThreadReadyInput<'a, Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.thread_store.get::<ReloadMarker>().is_some() {
                self.ready.notify_one();
                std::future::pending::<()>().await;
            }
        })
    }
    fn on_thread_stop<'a>(
        &'a self,
        input: codex_extension_api::ThreadStopInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.thread_store.get::<ReloadMarker>().is_some() {
                self.cleanup.notify_one();
                self.release.notified().await;
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_root_waits_for_cold_wake_reload_cleanup() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
        .try_init();
    let barrier = Arc::new(ReloadBarrier::default());
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(barrier.clone());
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let (_hold_root, root_gate) = oneshot::channel();
    let (_hold_sibling, sibling_gate) = oneshot::channel();
    let (evict_worker, eviction_gate) = oneshot::channel();
    let (release_grandchild, grandchild_gate) = oneshot::channel();
    let spawn = |id: &str, task: &str| {
        vec![
            chunk(ev_response_created(id)),
            chunk(ev_function_call_with_namespace(
                id,
                NAMESPACE,
                "spawn_agent",
                &json!({"task_name": task, "message": "work", "fork_turns": "none"}).to_string(),
            )),
            chunk(ev_completed(id)),
        ]
    };
    let routes = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
    let (server, _) = start_routed_streaming_sse_server(
        vec![
            vec![
                spawn("root-spawn", "worker"),
                vec![gated_chunk(eviction_gate, vec![
                    ev_response_created("evict-worker"),
                    ev_function_call_with_namespace("sibling-spawn", NAMESPACE, "spawn_agent", &json!({"task_name": "sibling", "message": "finish", "fork_turns": "none"}).to_string()),
                    ev_completed("evict-worker"),
                ])],
                vec![gated_chunk(root_gate, vec![ev_completed("root-held")])],
            ],
            vec![
                spawn("worker-spawn", "grandchild"),
                final_answer_chunks("worker", "waiting for grandchild"),
            ],
            vec![vec![gated_chunk(
                grandchild_gate,
                vec![
                    ev_response_created("grandchild"),
                    final_answer_item("grandchild-msg", "finished"),
                    ev_completed("grandchild"),
                ],
            )]],
            vec![vec![gated_chunk(sibling_gate, vec![ev_completed("sibling-held")])]],
        ],
        move |_, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let id = request_thread_id(&body)?.to_owned();
            let mut routes = routes.lock().unwrap();
            let next = routes.len();
            Some(*routes.entry(id).or_insert(next))
        },
    )
    .await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_session_source(SessionSource::Exec)
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            configure_multi_agent(config, Some(AgentPolling::Disabled));
            config.multi_agent_v2.max_concurrent_threads_per_session = 3;
        })
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
        panic!("root starts")
    };
    let worker_id = tokio::time::timeout(Duration::from_secs(10), created.recv())
        .await
        .context("worker creation")??;
    let worker = test.thread_manager.get_thread(worker_id).await?;
    let grandchild_id = tokio::time::timeout(Duration::from_secs(10), created.recv())
        .await
        .context("grandchild creation")??;
    tokio::time::timeout(Duration::from_secs(10), wait_for_turn_complete(&worker))
        .await
        .context("worker completion")?;
    tokio::time::timeout(Duration::from_secs(10), ThreadIdle::wait(&worker))
        .await
        .context("worker idle")?;
    // Admission evicts the Waiting worker and retains its authoritative reload config.
    evict_worker.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), created.recv())
        .await
        .context("sibling admission")??;
    assert!(test.thread_manager.get_thread(worker_id).await.is_err());
    release_grandchild.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), barrier.ready.notified())
        .await
        .with_context(|| {
            format!(
                "cold reload ready; lifecycle paths={:?}",
                barrier.paths.lock().expect("paths lock")
            )
        })?;
    test.codex
        .submit(Op::FailTurn {
            turn_id,
            error: ErrorEvent {
                message: "fail during cold wake resume".to_owned(),
                codex_error_info: None,
                misalignment: None,
            },
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(10), barrier.cleanup.notified())
        .await
        .context("cold reload teardown")?;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_turn_complete(&test.codex)
        )
        .await
        .is_err(),
        "failure must retain cold resume cleanup"
    );
    barrier.release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), wait_for_turn_complete(&test.codex)).await?;
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        vec![test.session_configured.thread_id]
    );
    assert!(test.thread_manager.get_thread(grandchild_id).await.is_err());
    assert_no_turn_starts(&test.codex, SETTLE).await;
    assert_eq!(*barrier.starts.lock().expect("starts lock"), 2);
    server.shutdown().await;
    Ok(())
}
