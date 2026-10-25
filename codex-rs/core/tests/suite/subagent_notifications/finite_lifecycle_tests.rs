//! Real task children retain finite TTY and compatibility Cell ownership across a final answer.

use super::*;
use codex_code_mode::*;
use codex_protocol::protocol::MultiAgentVersion;
use core_test_support::code_mode_host::CodeModeHostRecorder;
use core_test_support::process::process_is_alive;
use core_test_support::responses::ev_custom_tool_call;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wiremock::ResponseTemplate;

const LIVE_ANSWER: &str = "finite child has yielded while its producer remains alive";
const FINAL_RESULT: &str = "finite child has now settled its producer";

fn configure_finite_child(config: &mut codex_core::config::Config) {
    config
        .features
        .enable(Feature::AsyncProcessCompletion)
        .unwrap();
    config.features.enable(Feature::UnifiedExec).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::Disabled)
        .unwrap();
}

async fn assert_waiting_without_parent_final(run: &TypedReportsRun) -> Result<()> {
    ThreadIdle::wait(&run.worker).await;
    assert_eq!(run.worker.agent_status().await, AgentStatus::Waiting);
    assert!(
        run.worker_is_loaded().await,
        "live finite work must prevent task auto-close"
    );
    assert!(
        run.worker.list_pending_completions().await.is_empty(),
        "the finite hold must survive without an automatic completion record"
    );
    let root_requests = run.requests_for(run.root_thread_id()).await;
    assert_eq!(
        root_requests.len(),
        2,
        "the live producer must not report a premature final"
    );
    assert!(
        root_requests
            .iter()
            .all(|request| !request.to_string().contains("Message Type: FINAL_ANSWER"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tty_task_child_waits_for_native_exit_then_reports_final_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let arguments = json!({
        "cmd": format!("exec 3<>/dev/tcp/127.0.0.1/{port}; printf '%s\\n' \"$$\" >&3; IFS= read -r release <&3; printf tty-terminal-tail"),
        "shell": "/bin/bash", "login": false, "tty": true, "yield_time_ms": 250,
    });
    let (start_tx, start_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    let run = TypedReportsRun::start(
        json!({}),
        vec![
            chunk(peer_message(
                "tty-root-waiting",
                "root waits for finite child",
            )),
            chunk(peer_message("tty-root-done", "root accepted finite child")),
        ],
        vec![
            vec![gated_streaming_chunk(
                start_rx,
                vec![
                    ev_response_created("tty-child-start"),
                    ev_function_call("tty-child-command", "exec_command", &arguments.to_string()),
                    ev_completed("tty-child-start"),
                ],
            )],
            chunk(peer_message("tty-child-yield", LIVE_ANSWER)),
            vec![gated_streaming_chunk(
                finish_rx,
                peer_message("tty-child-done", FINAL_RESULT),
            )],
        ],
        configure_finite_child,
    )
    .await?;
    ThreadIdle::wait(&run.test.codex).await;
    start_tx
        .send(())
        .expect("start TTY child after root yields");
    let (stream, _) = timeout(Duration::from_secs(10), listener.accept()).await??;
    let mut stream = BufReader::new(stream);
    let mut pid = String::new();
    timeout(Duration::from_secs(10), stream.read_line(&mut pid)).await??;
    let pid = pid.trim();
    pid.parse::<u32>()
        .context("TTY command must identify its native PID")?;
    assert_waiting_without_parent_final(&run).await?;
    assert!(process_is_alive(pid)?);
    assert_eq!(run.requests_for(run.worker_thread_id).await.len(), 2);
    let terminals = run.worker.list_background_terminals().await;
    assert_eq!(terminals.len(), 1);
    assert!(
        !terminals[0].wakes_owner,
        "TTY must retain its existing nonautomatic result policy"
    );

    stream.get_mut().write_all(b"exit\n").await?;
    let continuation = wait_for_streaming_request_matching(&run.server, |request| {
        streaming_request_has_thread_id(request, run.worker_thread_id)
            && request.to_string().contains(LIVE_ANSWER)
    })
    .await?;
    assert!(
        !continuation.to_string().contains("kind=process"),
        "TTY exit resumes through the finite hint, without synthesizing an automatic result"
    );
    wait_for_event(run.worker.as_ref(), |event| {
        matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == "tty-child-command")
    }).await;
    finish_tx
        .send(())
        .expect("finish the one finite continuation");
    let report = run
        .root_request_with_report("FINAL_ANSWER", FINAL_RESULT)
        .await?;
    assert_eq!(agent_messages_containing(&report, FINAL_RESULT), 1);
    assert_eq!(agent_messages_containing(&report, LIVE_ANSWER), 0);
    wait_for_event(run.worker.as_ref(), |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    ThreadIdle::wait(&run.test.codex).await;
    assert_eq!(
        run.requests_for(run.worker_thread_id).await.len(),
        3,
        "native exit admits exactly one continuation"
    );
    assert_eq!(run.requests_for(run.root_thread_id()).await.len(), 3);
    run.test.codex.shutdown_and_wait().await?;
    run.server.shutdown().await;
    Ok(())
}

#[derive(Clone, Debug, Default)]
struct CompatCallbacks {
    cell_id: Option<String>,
    closed: usize,
    waits: usize,
}

/// The real process/stdio provider, exposing the older visible-wait capability. Only readiness
/// negotiation/callbacks change; execution, observation, cancellation and close remain native.
struct CompatProvider {
    inner: ProcessOwnedCodeModeSessionProvider,
    callbacks: watch::Sender<CompatCallbacks>,
    close_gate: Arc<tokio::sync::Mutex<Option<oneshot::Receiver<()>>>>,
}

impl CodeModeSessionProvider for CompatProvider {
    fn availability(&self) -> std::result::Result<(), String> {
        self.inner.availability()
    }

    fn terminal_ready_capability(&self) -> TerminalReadySupport {
        TerminalReadySupport::Unsupported
    }

    fn create_session(&self) -> CodeModeSessionProviderFuture<'_> {
        self.create_session_with_limits(CodeModeSessionCellExecutionLimits::default())
    }

    fn create_session_with_limits<'a>(
        &'a self,
        limits: CodeModeSessionCellExecutionLimits,
    ) -> CodeModeSessionProviderFuture<'a> {
        Box::pin(async move {
            Ok(Arc::new(CompatSession {
                inner: self.inner.create_session_with_limits(limits).await?,
                callbacks: self.callbacks.clone(),
                close_gate: Arc::clone(&self.close_gate),
            }) as Arc<dyn CodeModeSession>)
        })
    }
}

struct CompatSession {
    inner: Arc<dyn CodeModeSession>,
    callbacks: watch::Sender<CompatCallbacks>,
    close_gate: Arc<tokio::sync::Mutex<Option<oneshot::Receiver<()>>>>,
}

impl CodeModeSession for CompatSession {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async move {
            let mut started = self
                .inner
                .execute(
                    request,
                    Arc::new(CompatDelegate {
                        inner: delegate,
                        callbacks: self.callbacks.clone(),
                    }),
                    preempt,
                )
                .await?;
            assert_eq!(
                started.terminal_ready_support,
                TerminalReadySupport::Supported
            );
            started.terminal_ready_support = TerminalReadySupport::Unsupported;
            self.callbacks
                .send_modify(|callbacks| callbacks.cell_id = Some(started.cell_id.to_string()));
            Ok(started)
        })
    }

    fn wait<'a>(
        &'a self,
        request: WaitRequest,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.callbacks.send_modify(|callbacks| callbacks.waits += 1);
        self.inner.wait(request, preempt)
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            let gate = self.close_gate.lock().await.take();
            if let Some(gate) = gate {
                gate.await
                    .map_err(|_| "test close gate dropped".to_owned())?;
            }
            self.inner.terminate(cell_id).await
        })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        self.inner.shutdown()
    }
}

struct CompatDelegate {
    inner: Arc<dyn CodeModeSessionDelegate>,
    callbacks: watch::Sender<CompatCallbacks>,
}

impl CodeModeSessionDelegate for CompatDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        self.inner.invoke_tool(invocation, cancellation_token)
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        self.inner
            .notify(call_id, cell_id, text, cancellation_token)
    }

    fn cell_terminal_ready(&self, _ready: CellTerminalReady) {
        // Simulate a provider that cannot send readiness. Never elect an automatic collector.
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.inner.cell_closed(cell_id);
        self.callbacks
            .send_modify(|callbacks| callbacks.closed += 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compatibility_cell_child_stays_waiting_until_explicit_wait_terminates_it() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let recorder = CodeModeHostRecorder::new()?;
    let (callbacks, mut callback_updates) = watch::channel(CompatCallbacks::default());
    let (close_tx, close_rx) = oneshot::channel();
    let provider = Arc::new(CompatProvider {
        inner: ProcessOwnedCodeModeSessionProvider::with_host_program(recorder.program()),
        callbacks,
        close_gate: Arc::new(tokio::sync::Mutex::new(Some(close_rx))),
    });
    let callbacks_for_response = callback_updates.clone();
    let routing = Arc::new(Mutex::new((None::<String>, 0_usize, 0_usize)));
    let routing_for_response = Arc::clone(&routing);
    // Build the later wait call from the actual provider ID, rather than guessing host numbering.
    // Per-thread routing also makes an unexpected extra continuation fail immediately.
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&decoded_body(request).expect("decoded request")).expect("request JSON");
            let id = body["client_metadata"]["thread_id"].as_str().expect("thread ID");
            let mut routing = routing_for_response.lock().unwrap();
            if routing.0.is_none() && body.to_string().contains(START_PROMPT) {
                routing.0 = Some(id.to_owned());
            }
            let events = if routing.0.as_deref() == Some(id) {
                routing.1 += 1;
                match routing.1 {
                    1 => peer_call("compat-root-start", "compat-spawn", "spawn_agent", &json!({
                        "task_name": "worker", "message": WORKER_TASK, "fork_turns": "none", "model": "koffing",
                    }).to_string()),
                    2 | 4 => peer_message(&format!("compat-root-yield-{}", routing.1), "root waits for compatibility cell"),
                    3 => peer_call("compat-root-followup", "compat-followup", "followup_task", &json!({
                        "target": "worker", "message": "explicitly observe and terminate the compatibility cell",
                    }).to_string()),
                    5 => peer_message("compat-root-done", "root accepted compatibility final"),
                    _ => return ResponseTemplate::new(500).set_body_string("unexpected root continuation"),
                }
            } else {
                routing.2 += 1;
                match routing.2 {
                    1 => vec![
                        ev_response_created("compat-child-start"),
                        ev_custom_tool_call("compat-live-cell", "exec", "text('compatibility cell is live'); yield_control(); await new Promise(() => {});"),
                        ev_completed("compat-child-start"),
                    ],
                    2 => peer_message("compat-child-yield", LIVE_ANSWER),
                    3 => vec![
                        ev_response_created("compat-child-observe"),
                        ev_function_call("compat-wait", "wait", &json!({
                            "cell_id": callbacks_for_response.borrow().cell_id.as_ref().expect("native cell ID"),
                            "terminate": true,
                        }).to_string()),
                        ev_completed("compat-child-observe"),
                    ],
                    4 => peer_message("compat-child-done", FINAL_RESULT),
                    _ => return ResponseTemplate::new(500).set_body_string("unexpected child continuation"),
                }
            };
            ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse(events))
        }).mount(&server).await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.tool_mode = Some(ToolMode::Direct);
        })
        .with_model_info_override("koffing", |model| {
            model.tool_mode = Some(ToolMode::CodeMode);
            model.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_model("gpt-5.6-sol")
        .with_session_source(SessionSource::Cli)
        .with_extensions(Arc::new(extensions.build()))
        .with_thread_manager(move |manager| manager.with_code_mode_session_provider(provider))
        .with_config(|config| {
            configure_finite_child(config);
            for feature in [Feature::CodeMode, Feature::Collab, Feature::MultiAgentV2] {
                config.features.enable(feature).unwrap();
            }
            config.multi_agent_v2.agent_polling = AgentPolling::Disabled;
            config.multi_agent_v2.expose_spawn_agent_model_overrides = true;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.submit_turn(START_PROMPT).await?;
    let worker_id = timeout(Duration::from_secs(10), created.recv()).await??;
    let worker = test.thread_manager.get_thread(worker_id).await?;
    ThreadIdle::wait(&worker).await;
    assert_eq!(worker.agent_status().await, AgentStatus::Waiting);
    assert!(test.thread_manager.get_thread(worker_id).await.is_ok());
    assert!(
        worker.list_pending_completions().await.is_empty(),
        "unsupported readiness releases the AsyncCompletions reservation"
    );
    assert!(
        worker.list_background_terminals().await.is_empty(),
        "a nested native Task must not mask a missing Cell hold"
    );
    assert!(callback_updates.borrow().cell_id.is_some());
    assert_eq!(callback_updates.borrow().closed, 0);
    assert_eq!(
        callback_updates.borrow().waits,
        0,
        "no hidden watcher may consume this visible-wait result"
    );
    ThreadIdle::wait(&test.codex).await;
    {
        let routing = routing.lock().unwrap();
        assert_eq!((routing.1, routing.2), (2, 2));
    }
    let requests = server.received_requests().await.expect("recorded requests");
    assert!(
        requests
            .iter()
            .all(|request| !body_contains(request, "Message Type: FINAL_ANSWER"))
    );
    let worker_id_text = worker_id.to_string();
    let child_initial: Value = requests
        .iter()
        .filter_map(|request| decoded_body(request))
        .filter_map(|body| serde_json::from_slice::<Value>(&body).ok())
        .find(|body| body["client_metadata"]["thread_id"] == worker_id_text)
        .expect("child request");
    assert!(
        child_initial["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "wait"),
        "Unsupported readiness keeps explicit wait visible"
    );

    test.submit_turn("observe the retained compatibility cell")
        .await?;
    ThreadIdle::wait(&test.codex).await;
    close_tx
        .send(())
        .expect("close native cell after the root yields");
    timeout(Duration::from_secs(10), async {
        loop {
            if callback_updates.borrow_and_update().closed == 1 {
                break;
            }
            callback_updates
                .changed()
                .await
                .expect("native close callback");
        }
    })
    .await
    .context("explicit wait must close the real host cell")?;
    wait_for_event(worker.as_ref(), |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    ThreadIdle::wait(&test.codex).await;
    assert_eq!(callback_updates.borrow().closed, 1);
    assert_eq!(
        callback_updates.borrow().waits,
        0,
        "wait(terminate=true) uses native termination, never a hidden wait"
    );
    {
        let routing = routing.lock().unwrap();
        assert_eq!(
            (routing.1, routing.2),
            (5, 4),
            "native close during an active turn must not create an extra empty continuation"
        );
    }
    let requests = server.received_requests().await.expect("recorded requests");
    let report: Value = requests
        .iter()
        .filter(|request| body_contains(request, "Message Type: FINAL_ANSWER"))
        .filter_map(|request| decoded_body(request))
        .filter_map(|body| serde_json::from_slice(&body).ok())
        .next()
        .expect("real child final reaches the root");
    assert_eq!(agent_messages_containing(&report, FINAL_RESULT), 1);
    assert_eq!(agent_messages_containing(&report, LIVE_ANSWER), 0);
    assert_eq!(recorder.invocations()?.len(), 1);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
