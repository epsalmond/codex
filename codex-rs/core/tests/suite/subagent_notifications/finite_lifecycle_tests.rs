//! Real children retain finite producers and close their owned Services after one final report.

use super::*;
use codex_code_mode::*;
use codex_protocol::protocol::MultiAgentVersion;
use core_test_support::code_mode_host::CodeModeHostRecorder;
use core_test_support::process::process_is_alive;
use core_test_support::responses::ev_custom_tool_call;
use pretty_assertions::assert_eq;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
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

const ACCEPTANCE_DEADLINE: Duration = Duration::from_secs(20);

/// All synchronization is over the native process's socket, including shutdown EOF.
struct NativeInputGate(TcpListener);

impl NativeInputGate {
    async fn new() -> Result<Self> {
        Ok(Self(TcpListener::bind("127.0.0.1:0").await?))
    }

    fn arguments(&self, service: bool) -> Result<Value> {
        let port = self.0.local_addr()?.port();
        let mut arguments = json!({
            "cmd": format!("exec 3<>/dev/tcp/127.0.0.1/{port}; printf '%s\\n' \"$$\" >&3; IFS= read -r release <&3; printf retained-native-tail"),
            "shell": "/bin/bash", "login": false, "yield_time_ms": 250,
        });
        if service {
            arguments["work_kind"] = json!("service");
        }
        Ok(arguments)
    }

    async fn connected(&self) -> Result<(String, BufReader<TcpStream>)> {
        timeout(ACCEPTANCE_DEADLINE, async {
            let (stream, _) = self.0.accept().await?;
            let mut stream = BufReader::new(stream);
            let mut pid = String::new();
            stream.read_line(&mut pid).await?;
            let pid = pid.trim().to_owned();
            pid.parse::<u32>().context("native process PID")?;
            Ok((pid, stream))
        })
        .await
        .context("native process must reach its input gate")?
    }
}

#[derive(Clone, Debug, Default)]
struct NativeCellCallbacks {
    cell_id: Option<String>,
    ready: usize,
    closed: usize,
}

/// Forward every operation to the process-owned host. An optional gate holds the first actual
/// nested dispatch at the delegate boundary, so it reaches the native handler after turn cleanup.
struct NativeCellProvider {
    inner: ProcessOwnedCodeModeSessionProvider,
    callbacks: watch::Sender<NativeCellCallbacks>,
    dispatch_gate: Arc<tokio::sync::Mutex<Option<oneshot::Receiver<()>>>>,
}

impl CodeModeSessionProvider for NativeCellProvider {
    fn availability(&self) -> std::result::Result<(), String> {
        self.inner.availability()
    }

    fn terminal_ready_capability(&self) -> TerminalReadySupport {
        self.inner.terminal_ready_capability()
    }

    fn create_session(&self) -> CodeModeSessionProviderFuture<'_> {
        self.create_session_with_limits(CodeModeSessionCellExecutionLimits::default())
    }

    fn create_session_with_limits<'a>(
        &'a self,
        limits: CodeModeSessionCellExecutionLimits,
    ) -> CodeModeSessionProviderFuture<'a> {
        Box::pin(async move {
            Ok(Arc::new(NativeCellSession {
                inner: self.inner.create_session_with_limits(limits).await?,
                callbacks: self.callbacks.clone(),
                dispatch_gate: Arc::clone(&self.dispatch_gate),
            }) as Arc<dyn CodeModeSession>)
        })
    }
}

struct NativeCellSession {
    inner: Arc<dyn CodeModeSession>,
    callbacks: watch::Sender<NativeCellCallbacks>,
    dispatch_gate: Arc<tokio::sync::Mutex<Option<oneshot::Receiver<()>>>>,
}

impl CodeModeSession for NativeCellSession {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async move {
            let started = self
                .inner
                .execute(
                    request,
                    Arc::new(NativeCellDelegate {
                        inner: delegate,
                        callbacks: self.callbacks.clone(),
                        dispatch_gate: Arc::clone(&self.dispatch_gate),
                    }),
                    preempt,
                )
                .await?;
            self.callbacks.send_modify(|callbacks| {
                callbacks.cell_id = Some(started.cell_id.to_string());
            });
            assert_eq!(
                started.terminal_ready_support,
                TerminalReadySupport::Supported
            );
            Ok(started)
        })
    }

    fn wait<'a>(
        &'a self,
        request: WaitRequest,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.inner.wait(request, preempt)
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.inner.terminate(cell_id)
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        self.inner.shutdown()
    }
}

struct NativeCellDelegate {
    inner: Arc<dyn CodeModeSessionDelegate>,
    callbacks: watch::Sender<NativeCellCallbacks>,
    dispatch_gate: Arc<tokio::sync::Mutex<Option<oneshot::Receiver<()>>>>,
}

impl CodeModeSessionDelegate for NativeCellDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            let gate = self.dispatch_gate.lock().await.take();
            if let Some(gate) = gate {
                gate.await
                    .map_err(|_| "test dispatch gate dropped".to_owned())?;
            }
            self.inner.invoke_tool(invocation, cancellation_token).await
        })
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

    fn cell_terminal_ready(&self, ready: CellTerminalReady) {
        self.callbacks.send_modify(|callbacks| callbacks.ready += 1);
        self.inner.cell_terminal_ready(ready);
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.inner.cell_closed(cell_id);
        self.callbacks
            .send_modify(|callbacks| callbacks.closed += 1);
    }
}

impl TypedReportsRun {
    /// Cell-capable variant of the same typed-report fixture. The parent stays in Direct mode;
    /// only the spawned child uses Code Mode, with the real stdio provider and forwarding recorder.
    async fn start_with_native_cells(
        persistent: bool,
        root_responses: Vec<Vec<StreamingSseChunk>>,
        worker_responses: Vec<Vec<StreamingSseChunk>>,
        provider: Arc<NativeCellProvider>,
    ) -> Result<Self> {
        let mut spawn_args = json!({
            "message": WORKER_TASK, "task_name": "worker", "fork_turns": "none", "model": "koffing",
        });
        if persistent {
            spawn_args["agent_type"] = json!(PERSISTENT_ROLE);
        }
        let root_id = Arc::new(Mutex::new(None::<String>));
        let (server, _) = start_routed_streaming_sse_server(
            vec![
                vec![chunk(peer_call(
                    "cell-root-start",
                    "cell-spawn",
                    "spawn_agent",
                    &spawn_args.to_string(),
                ))],
                root_responses,
                worker_responses,
            ],
            move |_headers, body| {
                let body: Value = serde_json::from_slice(body).ok()?;
                let id = body["client_metadata"]["thread_id"].as_str()?;
                let mut root = root_id.lock().unwrap();
                if root.is_none() && body.to_string().contains(START_PROMPT) {
                    *root = Some(id.to_owned());
                    return Some(0);
                }
                if root.as_deref() == Some(id) {
                    Some(1)
                } else {
                    body.to_string().contains(WORKER_TASK).then_some(2)
                }
            },
        )
        .await;
        let base_url = format!("{}/v1", server.uri());
        let mock_server = start_mock_server().await;
        let mut extensions = ExtensionRegistryBuilder::new();
        extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
        let test = test_codex()
            .with_model("gpt-5.6-sol")
            .with_model_info_override("gpt-5.6-sol", |model| {
                model.tool_mode = Some(ToolMode::Direct)
            })
            .with_model_info_override("koffing", |model| {
                model.tool_mode = Some(ToolMode::CodeMode);
                model.multi_agent_version = Some(MultiAgentVersion::V2);
            })
            .with_model("gpt-5.6-sol")
            .with_session_source(SessionSource::Cli)
            .with_extensions(Arc::new(extensions.build()))
            .with_thread_manager(move |manager| manager.with_code_mode_session_provider(provider))
            .with_config(move |config| {
                configure_finite_child(config);
                for feature in [Feature::Collab, Feature::MultiAgentV2, Feature::CodeMode] {
                    config.features.enable(feature).unwrap();
                }
                config.multi_agent_v2.agent_polling = AgentPolling::Disabled;
                config.multi_agent_v2.expose_spawn_agent_model_overrides = true;
                config.model_provider.base_url = Some(base_url);
                config.model_provider.request_max_retries = Some(0);
                config.model_provider.stream_max_retries = Some(0);
                config.model_provider.supports_websockets = false;
                config.agent_roles.insert(
                    PERSISTENT_ROLE.to_owned(),
                    AgentRoleConfig {
                        description: Some("Retains context across assignments".to_owned()),
                        config_file: None,
                        nickname_candidates: None,
                        lifetime: Some(AgentLifetime::Persistent),
                    },
                );
            })
            .build_with_auto_env(&mock_server)
            .await?;
        let mut created = test.thread_manager.subscribe_thread_created();
        test.submit_turn(START_PROMPT).await?;
        let worker_thread_id = timeout(ACCEPTANCE_DEADLINE, created.recv()).await??;
        let worker = test.thread_manager.get_thread(worker_thread_id).await?;
        Ok(Self {
            test,
            server,
            worker_thread_id,
            worker,
        })
    }
}

fn native_cell_provider(
    recorder: &CodeModeHostRecorder,
    dispatch_gate: Option<oneshot::Receiver<()>>,
) -> (
    Arc<NativeCellProvider>,
    watch::Receiver<NativeCellCallbacks>,
) {
    let (callbacks, updates) = watch::channel(NativeCellCallbacks::default());
    (
        Arc::new(NativeCellProvider {
            inner: ProcessOwnedCodeModeSessionProvider::with_host_program(recorder.program()),
            callbacks,
            dispatch_gate: Arc::new(tokio::sync::Mutex::new(dispatch_gate)),
        }),
        updates,
    )
}

#[test_case(false; "native_service")]
#[test_case(true; "cell_service")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_only_task_child_reports_once_and_auto_close_terminates_native_work(
    cell: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    timeout(ACCEPTANCE_DEADLINE, service_only_task_child(cell)).await?
}

async fn service_only_task_child(cell: bool) -> Result<()> {
    let gate = NativeInputGate::new().await?;
    let mut arguments = gate.arguments(true)?;
    let (start_tx, start_rx) = oneshot::channel();
    let (final_tx, final_rx) = oneshot::channel();
    let recorder = CodeModeHostRecorder::new()?;
    let (provider, mut callbacks) = native_cell_provider(&recorder, None);
    let call = if cell {
        arguments["yield_time_ms"] = json!(30_000);
        ev_custom_tool_call(
            "service-child-command",
            "exec",
            &format!(
                "// @exec: {{\"work_kind\":\"service\",\"yield_time_ms\":1}}\nawait tools.exec_command({arguments});"
            ),
        )
    } else {
        ev_function_call(
            "service-child-command",
            "exec_command",
            &arguments.to_string(),
        )
    };
    let root_responses = vec![
        chunk(peer_message(
            "service-root-yield",
            "root waits for service child",
        )),
        chunk(peer_message(
            "service-root-done",
            "root accepted service child",
        )),
    ];
    let worker_responses = vec![
        vec![gated_streaming_chunk(
            start_rx,
            vec![
                ev_response_created("service-launch"),
                call,
                ev_completed("service-launch"),
            ],
        )],
        vec![gated_streaming_chunk(
            final_rx,
            peer_message("service-child-final", FINAL_RESULT),
        )],
    ];
    let run = if cell {
        TypedReportsRun::start_with_native_cells(false, root_responses, worker_responses, provider)
            .await?
    } else {
        TypedReportsRun::start(
            json!({}),
            root_responses,
            worker_responses,
            configure_finite_child,
        )
        .await?
    };
    ThreadIdle::wait(&run.test.codex).await;
    start_tx
        .send(())
        .expect("root is idle before child service starts");
    let (pid, mut stream) = gate.connected().await?;
    let yielded = wait_for_streaming_request_matching(&run.server, |request| {
        streaming_request_has_thread_id(request, run.worker_thread_id)
            && request.to_string().contains(if cell {
                "Script running with cell ID"
            } else {
                "Process running with session ID"
            })
    })
    .await?;
    assert!(yielded.to_string().contains("service"), "{yielded}");
    assert!(process_is_alive(&pid)?);
    if cell {
        assert!(callbacks.borrow().cell_id.is_some());
        assert_eq!(callbacks.borrow().ready, 0);
        assert_eq!(callbacks.borrow().closed, 0);
        assert_eq!(recorder.invocations()?.len(), 1);
    }
    final_tx
        .send(())
        .expect("finalize with only Services alive");
    let report = run
        .root_request_with_report("FINAL_ANSWER", FINAL_RESULT)
        .await?;
    assert_eq!(agent_messages_containing(&report, FINAL_RESULT), 1);
    wait_for_event(run.worker.as_ref(), |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let mut terminal = Vec::new();
    stream.read_to_end(&mut terminal).await?;
    assert!(
        terminal.is_empty(),
        "auto-close must terminate rather than release the socket read"
    );
    assert!(
        !process_is_alive(&pid)?,
        "auto-close must terminate the exact native Service PID"
    );
    if cell {
        while callbacks.borrow_and_update().closed == 0 {
            callbacks.changed().await?;
        }
        assert_eq!(
            callbacks.borrow().closed,
            1,
            "owned shutdown closes the native Service cell once"
        );
    }
    ThreadIdle::wait(&run.test.codex).await;
    assert_eq!(
        run.requests_for(run.worker_thread_id).await.len(),
        2,
        "Service teardown cannot start another child turn"
    );
    assert_eq!(
        run.requests_for(run.root_thread_id()).await.len(),
        3,
        "one parent FINAL_ANSWER wake"
    );
    run.test.codex.shutdown_and_wait().await?;
    run.server.shutdown().await;
    Ok(())
}

fn automatic_receipt(request: &Value, producer: &str) -> String {
    request["input"]
        .as_array()
        .expect("model input")
        .iter()
        .filter_map(|item| {
            let text = item["content"][0]["text"].as_str()?;
            (text.starts_with("<async_tool_completion>") && text.contains(producer))
                .then_some(text.to_owned())
        })
        .last()
        .expect("automatic completion for this producer")
}

fn receipt_identity(receipt: &str, name: &str) -> String {
    receipt
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("receipt missing {name}: {receipt}"))
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_cell_launches_native_task_after_final_in_original_generation() -> Result<()> {
    skip_if_no_network!(Ok(()));
    timeout(ACCEPTANCE_DEADLINE, retained_cell_launch()).await?
}

async fn retained_cell_launch() -> Result<()> {
    const NEXT_RESULT: &str = "a new assignment completed independently";
    const NEXT_LIVE: &str = "the new assignment has its own finite cell";
    let gate = NativeInputGate::new().await?;
    let next_gate = NativeInputGate::new().await?;
    let arguments = gate.arguments(false)?;
    let mut next_arguments = next_gate.arguments(false)?;
    next_arguments["yield_time_ms"] = json!(30_000);
    let recorder = CodeModeHostRecorder::new()?;
    let (dispatch_tx, dispatch_rx) = oneshot::channel();
    let (provider, callbacks) = native_cell_provider(&recorder, Some(dispatch_rx));
    let (start_tx, start_rx) = oneshot::channel();
    let (cell_accept_tx, cell_accept_rx) = oneshot::channel();
    let (native_accept_tx, native_accept_rx) = oneshot::channel();
    let (followup_tx, followup_rx) = oneshot::channel();
    let (next_accept_tx, next_accept_rx) = oneshot::channel();
    let run = TypedReportsRun::start_with_native_cells(
        true,
        vec![
            chunk(peer_message("retained-root-yield", "root waits for retained cell")),
            vec![gated_streaming_chunk(followup_rx, peer_call(
                "retained-root-followup", "retained-followup", "followup_task",
                &json!({"target":"worker", "message":WORKER_FOLLOWUP}).to_string(),
            ))],
            chunk(peer_message("retained-root-wait-again", "root waits for new assignment")),
            chunk(peer_message("retained-root-done", "root accepted both exact generations")),
        ],
        vec![
            vec![gated_streaming_chunk(start_rx, vec![
                ev_response_created("retained-cell-launch"),
                ev_custom_tool_call("retained-cell", "exec", &format!(
                    "// @exec: {{\"yield_time_ms\":1}}\ntext('retained cell yielded'); yield_control(); const result = await tools.exec_command({arguments}); text(result.output);"
                )),
                ev_completed("retained-cell-launch"),
            ])],
            chunk(peer_message("retained-child-yield", LIVE_ANSWER)),
            vec![gated_streaming_chunk(cell_accept_rx, peer_message("retained-cell-accepted", LIVE_ANSWER))],
            vec![gated_streaming_chunk(native_accept_rx, peer_message("retained-task-accepted", FINAL_RESULT))],
            chunk(vec![
                ev_response_created("next-cell-launch"),
                ev_custom_tool_call("retained-cell", "exec", &format!(
                    "// @exec: {{\"yield_time_ms\":1}}\nawait tools.exec_command({next_arguments}); text('next-generation-cell-tail');"
                )),
                ev_completed("next-cell-launch"),
            ]),
            chunk(peer_message("next-child-yield", NEXT_LIVE)),
            vec![gated_streaming_chunk(next_accept_rx, peer_message("next-child-final", NEXT_RESULT))],
        ],
        provider,
    ).await?;
    ThreadIdle::wait(&run.test.codex).await;
    start_tx
        .send(())
        .expect("root yields before retained cell starts");
    ThreadIdle::wait(&run.worker).await;
    let initial = wait_for_event_match(run.worker.as_ref(), |event| match event {
        EventMsg::TurnComplete(turn) => Some(turn.clone()),
        _ => None,
    })
    .await;
    assert_eq!(run.worker.agent_status().await, AgentStatus::Waiting);
    assert!(run.worker_is_loaded().await);
    assert_eq!(run.requests_for(run.worker_thread_id).await.len(), 2);
    assert!(
        run.worker.list_background_terminals().await.is_empty(),
        "native handler has not run before initial turn cleanup"
    );
    let pending = run.worker.list_pending_completions().await;
    assert_eq!(pending.len(), 1);
    assert!(pending[0].cell_id.is_some());
    assert!(!pending[0].ready);
    assert_eq!(callbacks.borrow().closed, 0);

    dispatch_tx
        .send(())
        .expect("dispatch the actual native Task from the retained cell after turn cleanup");
    let (pid, mut stream) = gate.connected().await?;
    let native_start = wait_for_event_match(run.worker.as_ref(), |event| match event {
        EventMsg::ExecCommandBegin(begin) => Some(begin.clone()),
        _ => None,
    })
    .await;
    assert_eq!(native_start.turn_id, initial.turn_id);
    let cell_request = wait_for_streaming_request_matching(&run.server, |request| {
        streaming_request_has_thread_id(request, run.worker_thread_id)
            && request.to_string().contains("<async_tool_completion>")
    })
    .await?;
    let cell_receipt = automatic_receipt(&cell_request, "kind=cell");
    assert_eq!(receipt_identity(&cell_receipt, "turn"), initial.turn_id);
    assert_eq!(
        receipt_identity(&cell_receipt, "owner"),
        run.worker_thread_id.to_string()
    );
    let original_generation = receipt_identity(&cell_receipt, "assignment");
    assert_ne!(original_generation, "none");
    assert!(process_is_alive(&pid)?);
    assert_eq!(run.worker.list_background_terminals().await.len(), 1);
    cell_accept_tx
        .send(())
        .expect("accept cell completion but leave its native Task alive");
    ThreadIdle::wait(&run.worker).await;
    assert_eq!(
        run.worker.agent_status().await,
        AgentStatus::Waiting,
        "the native Task, admitted after initial finalization, retains the original assignment after the cell closes"
    );
    assert!(run.worker_is_loaded().await);
    assert_eq!(callbacks.borrow().closed, 1);
    assert!(
        run.worker.list_pending_completions().await.is_empty(),
        "a native Task started after its originating turn ended keeps its nonautomatic result policy"
    );
    let terminals = run.worker.list_background_terminals().await;
    assert_eq!(terminals.len(), 1);
    assert!(!terminals[0].wakes_owner);
    assert_eq!(run.requests_for(run.worker_thread_id).await.len(), 3);
    assert_eq!(
        run.requests_for(run.root_thread_id()).await.len(),
        2,
        "neither initial final nor Cell completion may report premature parent final"
    );

    stream.get_mut().write_all(b"finish\n").await?;
    let native_end = wait_for_event_match(run.worker.as_ref(), |event| match event {
        EventMsg::ExecCommandEnd(end) if end.call_id == native_start.call_id => Some(end.clone()),
        _ => None,
    })
    .await;
    assert_eq!(native_end.turn_id, initial.turn_id);
    assert_eq!(native_end.exit_code, 0);
    assert!(
        native_end
            .aggregated_output
            .contains("retained-native-tail")
    );
    let native_request = wait_for_streaming_request_matching(&run.server, |request| {
        streaming_request_has_thread_id(request, run.worker_thread_id)
            && request["input"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["type"] == "message"
                        && item["role"] == "assistant"
                        && item.to_string().contains(LIVE_ANSWER)
                })
            })
            && request != &cell_request
    })
    .await?;
    assert_eq!(
        automatic_receipt(&native_request, "kind=cell"),
        cell_receipt
    );
    assert!(
        !native_request.to_string().contains("kind=process"),
        "the original-generation finite hint resumes once without synthesizing a native automatic result"
    );
    native_accept_tx
        .send(())
        .expect("accept original generation's native result");
    let original_report = run
        .root_request_with_report("FINAL_ANSWER", FINAL_RESULT)
        .await?;
    assert_eq!(agent_messages_containing(&original_report, FINAL_RESULT), 1);
    assert_eq!(agent_messages_containing(&original_report, LIVE_ANSWER), 0);
    ThreadIdle::wait(&run.worker).await;
    assert!(run.worker.list_pending_completions().await.is_empty());
    assert!(!process_is_alive(&pid)?);

    // Reuse the same child and call ID under a new assignment. Old completions must remain
    // accepted history and cannot donate a wake, hold or identity to the replacement generation.
    followup_tx
        .send(())
        .expect("begin an explicit new generation after original drain");
    let (next_pid, mut next_stream) = next_gate.connected().await?;
    ThreadIdle::wait(&run.worker).await;
    ThreadIdle::wait(&run.test.codex).await;
    assert_eq!(run.worker.agent_status().await, AgentStatus::Waiting);
    assert_eq!(
        run.requests_for(run.worker_thread_id).await.len(),
        6,
        "no stale completion creates an extra new-generation turn"
    );
    let pending = run.worker.list_pending_completions().await;
    assert_eq!(
        pending.len(),
        1,
        "new generation retains only its own outer Cell"
    );
    assert!(pending[0].cell_id.is_some());
    assert!(!pending[0].ready);
    next_stream.get_mut().write_all(b"finish\n").await?;
    let next_request = wait_for_streaming_request_matching(&run.server, |request| {
        streaming_request_has_thread_id(request, run.worker_thread_id)
            && request["input"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["content"][0]["text"].as_str().is_some_and(|text| {
                        text.starts_with("<async_tool_completion>")
                            && text.contains("kind=cell")
                            && text.contains("next-generation-cell-tail")
                    })
                })
            })
    })
    .await?;
    let next_receipt = automatic_receipt(&next_request, "kind=cell");
    assert_ne!(
        receipt_identity(&next_receipt, "assignment"),
        original_generation
    );
    assert_ne!(receipt_identity(&next_receipt, "turn"), initial.turn_id);
    assert!(
        !next_request.to_string().contains("kind=process"),
        "the old nonautomatic Task never donates an automatic result to a new generation"
    );
    next_accept_tx
        .send(())
        .expect("accept only the new generation's result");
    let next_report = run
        .root_request_with_report("FINAL_ANSWER", NEXT_RESULT)
        .await?;
    assert_eq!(agent_messages_containing(&next_report, FINAL_RESULT), 1);
    assert_eq!(agent_messages_containing(&next_report, NEXT_RESULT), 1);
    ThreadIdle::wait(&run.worker).await;
    ThreadIdle::wait(&run.test.codex).await;
    assert_eq!(run.requests_for(run.worker_thread_id).await.len(), 7);
    assert_eq!(run.requests_for(run.root_thread_id()).await.len(), 5);
    assert!(run.worker.list_pending_completions().await.is_empty());
    assert_eq!(callbacks.borrow().closed, 2);
    assert_eq!(callbacks.borrow().ready, 2);
    assert_eq!(
        recorder.invocations()?.len(),
        1,
        "both generations use the same real host"
    );
    assert!(!process_is_alive(&next_pid)?);
    run.test.codex.shutdown_and_wait().await?;
    run.server.shutdown().await;
    Ok(())
}
