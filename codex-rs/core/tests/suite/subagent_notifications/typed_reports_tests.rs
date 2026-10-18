//! Typed subagent reports: `report(blocked|final)` ends the child's turn and reaches the root,
//! task agents close after their final report, persistent agents stay resumable, and a long
//! subagent turn gets one wrap-up reminder.

use super::*;
use codex_core::CodexThread;
use codex_extension_api::ExtensionRegistryBuilder;
use core_test_support::ThreadIdle;
use pretty_assertions::assert_eq;
use test_case::test_case;

const START_PROMPT: &str = "start typed report regression";
const WORKER_TASK: &str = "typed report worker task";
const WORKER_FOLLOWUP: &str = "typed report worker followup";
const WORKER_RESULT: &str = "typed report worker result";
const WRAP_UP_TAG: &str = "<subagent_wrap_up>";

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

fn report_call(response: &str, kind: &str, message: &str) -> Result<Vec<Value>> {
    let arguments = serde_json::to_string(&json!({ "kind": kind, "message": message }))?;
    Ok(peer_call(
        response,
        &format!("{response}-call"),
        "report",
        &arguments,
    ))
}

fn chunk(events: Vec<Value>) -> Vec<StreamingSseChunk> {
    vec![StreamingSseChunk {
        gate: None,
        body: sse(events),
    }]
}

struct TypedReportsRun {
    test: TestCodex,
    server: StreamingSseServer,
    worker_thread_id: ThreadId,
    worker: Arc<CodexThread>,
}

impl TypedReportsRun {
    /// Starts a root that spawns `/root/worker` with `spawn_args` extras. `root_responses` answer
    /// the root's requests after its first; `worker_responses` answer the worker's requests.
    async fn start(
        spawn_extras: Value,
        root_responses: Vec<Vec<StreamingSseChunk>>,
        worker_responses: Vec<Vec<StreamingSseChunk>>,
        configure: impl Fn(&mut codex_core::config::Config) + Send + Sync + 'static,
    ) -> Result<Self> {
        let mut spawn_args = json!({
            "message": WORKER_TASK, "task_name": "worker", "fork_turns": "none",
        });
        if let (Some(args), Some(extras)) = (spawn_args.as_object_mut(), spawn_extras.as_object()) {
            args.extend(extras.clone());
        }
        let spawn_args = serde_json::to_string(&spawn_args)?;
        let root_id = Arc::new(Mutex::new(None::<String>));
        let root_id_for_matcher = Arc::clone(&root_id);
        let (server, _) = start_routed_streaming_sse_server(
            vec![
                vec![chunk(peer_call(
                    "typed-root-start",
                    "typed-spawn-worker",
                    "spawn_agent",
                    &spawn_args,
                ))],
                root_responses,
                worker_responses,
            ],
            move |_headers, body| {
                let body: Value = serde_json::from_slice(body).ok()?;
                let thread_id = body["client_metadata"]["thread_id"].as_str()?;
                let text = body.to_string();
                let mut root = root_id_for_matcher
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if root.is_none() && text.contains(START_PROMPT) {
                    *root = Some(thread_id.to_string());
                    return Some(0);
                }
                if root.as_deref() == Some(thread_id) {
                    return Some(1);
                }
                text.contains(WORKER_TASK).then_some(2)
            },
        )
        .await;
        let base_url = format!("{}/v1", server.uri());
        let mock_server = start_mock_server().await;
        let mut extensions = ExtensionRegistryBuilder::new();
        extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
        let test = test_codex()
            .with_model("koffing")
            .with_session_source(SessionSource::Cli)
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
                configure(config);
            })
            .build_with_auto_env(&mock_server)
            .await?;
        let mut created = test.thread_manager.subscribe_thread_created();
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: START_PROMPT.to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        let worker_thread_id = created.recv().await?;
        let worker = test.thread_manager.get_thread(worker_thread_id).await?;
        Ok(Self {
            test,
            server,
            worker_thread_id,
            worker,
        })
    }

    fn root_thread_id(&self) -> ThreadId {
        self.test.session_configured.thread_id
    }

    async fn requests_for(&self, thread_id: ThreadId) -> Vec<Value> {
        self.server
            .requests()
            .await
            .iter()
            .filter_map(|body| serde_json::from_slice::<Value>(body).ok())
            .filter(|request| streaming_request_has_thread_id(request, thread_id))
            .collect()
    }

    /// Waits for the root request that carries the worker's report of `message_type`.
    async fn root_request_with_report(&self, message_type: &str, payload: &str) -> Result<Value> {
        let root = self.root_thread_id();
        let header = format!("Message Type: {message_type}");
        wait_for_streaming_request_matching(&self.server, |request| {
            let text = request.to_string();
            streaming_request_has_thread_id(request, root)
                && text.contains(&header)
                && text.contains(payload)
        })
        .await
    }

    /// Waits until closing the worker removed it from the thread manager.
    async fn wait_for_worker_shutdown(&self) {
        wait_for_event(self.worker.as_ref(), |event| {
            matches!(event, EventMsg::ShutdownComplete)
        })
        .await;
        timeout(Duration::from_secs(10), async {
            while self.worker_is_loaded().await {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("closed worker should be removed");
    }

    async fn worker_is_loaded(&self) -> bool {
        self.test
            .thread_manager
            .get_thread(self.worker_thread_id)
            .await
            .is_ok()
    }
}

/// Count of root agent messages carrying `text`, so a duplicated report is visible.
fn agent_messages_containing(request: &Value, text: &str) -> usize {
    request["input"].as_array().map_or(0, |items| {
        items
            .iter()
            .filter(|item| item["type"] == "agent_message" && item.to_string().contains(text))
            .count()
    })
}

fn function_output(request: &Value, call_id: &str) -> Option<String> {
    request["input"].as_array()?.iter().find_map(|item| {
        (item["type"] == "function_call_output" && item["call_id"] == call_id)
            .then(|| item["output"].as_str().map(str::to_string))
            .flatten()
    })
}

#[test_case(true; "explicit final report")]
#[test_case(false; "turn ends without report")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_agent_final_report_reaches_root_once_then_agent_closes(explicit: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let (worker_release_tx, worker_release_rx) = oneshot::channel();
    let (root_wake_release_tx, root_wake_release_rx) = oneshot::channel();
    let followup = serde_json::to_string(&json!({
        "target": "worker", "message": WORKER_FOLLOWUP,
    }))?;
    let worker_events = if explicit {
        report_call("typed-worker-final", "final", WORKER_RESULT)?
    } else {
        peer_message("typed-worker-final", WORKER_RESULT)
    };
    let run = TypedReportsRun::start(
        json!({}),
        vec![
            chunk(peer_message("typed-root-yield", "root waits for worker")),
            vec![gated_streaming_chunk(
                root_wake_release_rx,
                peer_call(
                    "typed-root-followup",
                    "typed-root-followup-call",
                    "followup_task",
                    &followup,
                ),
            )],
            chunk(peer_message("typed-root-done", "root done")),
        ],
        vec![vec![gated_streaming_chunk(
            worker_release_rx,
            worker_events,
        )]],
        |_| {},
    )
    .await?;
    ThreadIdle::wait(&run.test.codex).await;
    worker_release_tx.send(()).expect("release worker");

    let wake_request = run
        .root_request_with_report("FINAL_ANSWER", WORKER_RESULT)
        .await?;
    assert_eq!(agent_messages_containing(&wake_request, WORKER_RESULT), 1);
    assert!(
        !wake_request
            .to_string()
            .contains("closed before it reported")
    );
    run.wait_for_worker_shutdown().await;
    root_wake_release_tx.send(()).expect("release root wake");

    let followup_request = wait_for_streaming_request_matching(&run.server, |request| {
        streaming_request_has_thread_id(request, run.root_thread_id())
            && function_output(request, "typed-root-followup-call").is_some()
    })
    .await?;
    assert_eq!(
        function_output(&followup_request, "typed-root-followup-call").as_deref(),
        Some("agent `/root/worker` is closed; spawn a fresh agent with a self-contained brief")
    );
    assert_eq!(
        agent_messages_containing(&followup_request, WORKER_RESULT),
        1
    );
    ThreadIdle::wait(&run.test.codex).await;
    assert_eq!(run.requests_for(run.worker_thread_id).await.len(), 1);
    Ok(())
}

#[test_case(true; "blocked report")]
#[test_case(false; "persistent final")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_or_persistent_agent_stays_resumable(blocked: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));

    const BLOCKER: &str = "typed report worker needs a decision";
    let (worker_release_tx, worker_release_rx) = oneshot::channel();
    let followup = serde_json::to_string(&json!({
        "target": "worker", "message": WORKER_FOLLOWUP,
    }))?;
    let (spawn_extras, first_worker_events, first_message_type, first_payload) = if blocked {
        (
            json!({}),
            report_call("typed-worker-blocked", "blocked", BLOCKER)?,
            "BLOCKED",
            BLOCKER,
        )
    } else {
        (
            json!({ "lifetime": "persistent" }),
            peer_message("typed-worker-first", "typed report first result"),
            "FINAL_ANSWER",
            "typed report first result",
        )
    };
    let run = TypedReportsRun::start(
        spawn_extras,
        vec![
            chunk(peer_message("typed-root-yield", "root waits for worker")),
            chunk(peer_call(
                "typed-root-followup",
                "typed-root-followup-call",
                "followup_task",
                &followup,
            )),
            chunk(peer_message("typed-root-accepted", "root waits again")),
            chunk(peer_message("typed-root-done", "root done")),
        ],
        vec![
            vec![gated_streaming_chunk(
                worker_release_rx,
                first_worker_events,
            )],
            chunk(peer_message("typed-worker-second", WORKER_RESULT)),
        ],
        |_| {},
    )
    .await?;
    ThreadIdle::wait(&run.test.codex).await;
    worker_release_tx.send(()).expect("release worker");

    let first_report = run
        .root_request_with_report(first_message_type, first_payload)
        .await?;
    if blocked {
        assert!(
            first_report
                .to_string()
                .contains("Reply with followup_task to resume it.")
        );
    }
    let resumed = wait_for_streaming_request_matching(&run.server, |request| {
        streaming_request_has_thread_id(request, run.worker_thread_id)
            && request.to_string().contains(WORKER_FOLLOWUP)
    })
    .await?;
    assert!(!resumed.to_string().contains(WRAP_UP_TAG));
    run.root_request_with_report("FINAL_ANSWER", WORKER_RESULT)
        .await?;
    // The blocked agent's later final report closes it; a persistent agent stays loaded.
    if blocked {
        run.wait_for_worker_shutdown().await;
    } else {
        ThreadIdle::wait(&run.worker).await;
        assert!(run.worker_is_loaded().await);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_subagent_turn_gets_one_wrap_up_reminder() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let list =
        |response: &str| peer_call(response, &format!("{response}-call"), "list_agents", "{}");
    let run = TypedReportsRun::start(
        json!({}),
        vec![
            chunk(peer_message("typed-root-yield", "root waits for worker")),
            chunk(peer_message("typed-root-done", "root done")),
        ],
        vec![
            chunk(list("typed-worker-step-1")),
            chunk(list("typed-worker-step-2")),
            chunk(list("typed-worker-step-3")),
            report_call("typed-worker-final", "final", WORKER_RESULT).map(chunk)?,
        ],
        |config| config.multi_agent_v2.wrap_up_after_model_calls = 2,
    )
    .await?;
    run.root_request_with_report("FINAL_ANSWER", WORKER_RESULT)
        .await?;

    let reminders = run
        .requests_for(run.worker_thread_id)
        .await
        .iter()
        .map(|request| request.to_string().matches(WRAP_UP_TAG).count())
        .collect::<Vec<_>>();
    assert_eq!(reminders, vec![0, 0, 1, 1]);
    let third = &run.requests_for(run.worker_thread_id).await[2];
    assert!(third.to_string().contains(
        "You have made 2 model calls on this task. Finish the current step, then report(kind=\\\"final\\\")"
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_reports_opt_out_keeps_finished_agent_and_reminder_off() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let list =
        |response: &str| peer_call(response, &format!("{response}-call"), "list_agents", "{}");
    let run = TypedReportsRun::start(
        json!({}),
        vec![
            chunk(peer_message("typed-root-yield", "root waits for worker")),
            chunk(peer_message("typed-root-done", "root done")),
        ],
        vec![
            chunk(list("typed-worker-step-1")),
            chunk(peer_message("typed-worker-final", WORKER_RESULT)),
        ],
        |config| {
            config.multi_agent_v2.typed_reports = false;
            config.multi_agent_v2.wrap_up_after_model_calls = 1;
        },
    )
    .await?;
    run.root_request_with_report("FINAL_ANSWER", WORKER_RESULT)
        .await?;
    ThreadIdle::wait(&run.worker).await;

    let worker_requests = run.requests_for(run.worker_thread_id).await;
    assert_eq!(worker_requests.len(), 2);
    for request in &worker_requests {
        let text = request.to_string();
        assert!(!text.contains(WRAP_UP_TAG));
        assert!(!text.contains("\"name\":\"report\""));
    }
    assert!(run.worker_is_loaded().await);
    Ok(())
}
