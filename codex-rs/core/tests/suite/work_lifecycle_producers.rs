//! Acceptance through the public observer, native commands and the real Code Mode host.

use anyhow::Context;
use anyhow::Result;
use codex_code_mode::*;
use codex_core::GuardedShutdownOutcome;
use codex_core::WorkObservation;
use codex_core::WorkObservationSnapshot;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TurnCompleteEvent;
use core_test_support::code_mode_host::CodeModeHostRecorder;
use core_test_support::process::process_is_alive;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const DEADLINE: Duration = Duration::from_secs(20);
const CALL_ID: &str = "owned-producer";

// The process reports its actual PID and then blocks in a shell read. Readiness, release and
// teardown are socket events; neither the test nor the model polls a process or a marker file.
struct NativeGate(TcpListener);

impl NativeGate {
    async fn new() -> Result<Self> {
        Ok(Self(TcpListener::bind("127.0.0.1:0").await?))
    }

    fn arguments(&self, service: bool, explicit_task: bool, exit_code: i32) -> Result<Value> {
        let port = self.0.local_addr()?.port();
        let mut args = json!({
            "cmd": format!(
                "exec 3<>/dev/tcp/127.0.0.1/{port}; printf '%s\\n' \"$$\" >&3; IFS= read -r release <&3; printf terminal-native-tail; exit {exit_code}"
            ),
            "shell": "/bin/bash",
            "login": false,
            "yield_time_ms": 250,
        });
        if service || explicit_task {
            args["work_kind"] = json!(if service { "service" } else { "task" });
        }
        Ok(args)
    }

    async fn connected(&self) -> Result<(String, BufReader<TcpStream>)> {
        tokio::time::timeout(DEADLINE, async {
            let (stream, _) = self.0.accept().await?;
            let mut stream = BufReader::new(stream);
            let mut pid = String::new();
            stream.read_line(&mut pid).await?;
            let pid = pid.trim().to_owned();
            pid.parse::<u32>()
                .context("native gate must report its PID")?;
            Ok((pid, stream))
        })
        .await
        .context("native process did not reach its input gate")?
    }
}

#[derive(Clone, Copy, Debug)]
enum Producer {
    Native,
    Cell,
}

impl Producer {
    fn call(self, mut args: Value, service: bool, explicit_task: bool) -> Value {
        match self {
            Self::Native => ev_function_call(CALL_ID, "exec_command", &args.to_string()),
            Self::Cell => {
                // The outer cell yields immediately, while this native Task stays inside its
                // original nested tool call. Service is declared independently at both levels.
                args["yield_time_ms"] = json!(30_000);
                let mut pragma = json!({ "yield_time_ms": 1 });
                if service || explicit_task {
                    pragma["work_kind"] = json!(if service { "service" } else { "task" });
                }
                ev_custom_tool_call(
                    CALL_ID,
                    "exec",
                    &format!(
                        "// @exec: {pragma}\nconst result = await tools.exec_command({args}); text(result.output); text('terminal-cell-tail');"
                    ),
                )
            }
        }
    }
}

fn assistant_response(id: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_assistant_message(id, "producer may continue after this answer"),
        ev_completed(id),
    ])
}

// This is only a forwarding recorder around the real process-owned provider. It records native
// readiness and close callbacks; lifecycle accounting and collection remain entirely in core.
#[derive(Clone, Debug, Default)]
struct CellCallbacks {
    cell_id: Option<String>,
    ready: usize,
    closed: usize,
}

struct RecordingProvider {
    inner: ProcessOwnedCodeModeSessionProvider,
    callbacks: watch::Sender<CellCallbacks>,
}

impl CodeModeSessionProvider for RecordingProvider {
    fn availability(&self) -> Result<(), String> {
        self.inner.availability()
    }

    fn create_session(&self) -> CodeModeSessionProviderFuture<'_> {
        self.create_session_with_limits(CodeModeSessionCellExecutionLimits::default())
    }

    fn create_session_with_limits<'a>(
        &'a self,
        limits: CodeModeSessionCellExecutionLimits,
    ) -> CodeModeSessionProviderFuture<'a> {
        Box::pin(async move {
            Ok(Arc::new(RecordingSession {
                inner: self.inner.create_session_with_limits(limits).await?,
                callbacks: self.callbacks.clone(),
            }) as Arc<dyn CodeModeSession>)
        })
    }

    fn terminal_ready_capability(&self) -> TerminalReadySupport {
        self.inner.terminal_ready_capability()
    }
}

struct RecordingSession {
    inner: Arc<dyn CodeModeSession>,
    callbacks: watch::Sender<CellCallbacks>,
}

impl CodeModeSession for RecordingSession {
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
                    Arc::new(RecordingDelegate {
                        inner: delegate,
                        callbacks: self.callbacks.clone(),
                    }),
                    preempt,
                )
                .await?;
            self.callbacks.send_modify(|callbacks| {
                callbacks.cell_id = Some(started.cell_id.as_str().to_owned());
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

struct RecordingDelegate {
    inner: Arc<dyn CodeModeSessionDelegate>,
    callbacks: watch::Sender<CellCallbacks>,
}

impl CodeModeSessionDelegate for RecordingDelegate {
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

    fn cell_terminal_ready(&self, ready: CellTerminalReady) {
        self.callbacks.send_modify(|callbacks| {
            callbacks.cell_id = Some(ready.cell_id.as_str().to_owned());
            callbacks.ready += 1;
        });
        self.inner.cell_terminal_ready(ready);
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.inner.cell_closed(cell_id);
        self.callbacks.send_modify(|callbacks| {
            callbacks.cell_id = Some(cell_id.as_str().to_owned());
            callbacks.closed += 1;
        });
    }
}

async fn fixture(
    server: &StreamingSseServer,
    producer: Producer,
) -> Result<(
    TestCodex,
    CodeModeHostRecorder,
    watch::Receiver<CellCallbacks>,
)> {
    let recorder = CodeModeHostRecorder::new()?;
    let (callbacks, receiver) = watch::channel(CellCallbacks::default());
    let provider = Arc::new(RecordingProvider {
        inner: ProcessOwnedCodeModeSessionProvider::with_host_program(recorder.program()),
        callbacks,
    });
    // Local execution is intentional: the TCP gate and the real host are on this machine.
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_session_source(codex_protocol::protocol::SessionSource::Exec)
        .with_model_info_override("gpt-5.6-sol", move |model| {
            model.multi_agent_version = Some(MultiAgentVersion::V2);
            model.tool_mode = Some(match producer {
                Producer::Native => ToolMode::Direct,
                Producer::Cell => ToolMode::CodeMode,
            });
        })
        .with_thread_manager(move |manager| manager.with_code_mode_session_provider(provider))
        .with_config(move |config| {
            config
                .features
                .enable(Feature::AsyncProcessCompletion)
                .unwrap();
            config.features.enable(Feature::MultiAgentV2).unwrap();
            if matches!(producer, Producer::Cell) {
                config.features.enable(Feature::CodeMode).unwrap();
            }
            config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Disabled;
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.request_max_retries = Some(0);
        })
        .build_with_streaming_server(server)
        .await?;
    Ok((test, recorder, receiver))
}

async fn terminal_turn(test: &TestCodex) -> Result<TurnCompleteEvent> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            match test.codex.next_event().await?.msg {
                EventMsg::TurnComplete(event) => return Ok(event),
                EventMsg::TurnAborted(event) => anyhow::bail!("turn aborted: {event:?}"),
                _ => {}
            }
        }
    })
    .await
    .context("expected a terminal root event")?
}

async fn observe(
    updates: &mut watch::Receiver<WorkObservationSnapshot>,
    predicate: impl Fn(&WorkObservationSnapshot) -> bool,
) -> Result<WorkObservationSnapshot> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let snapshot = updates.borrow_and_update().clone();
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            updates.changed().await.context("work observation closed")?;
        }
    })
    .await
    .context("expected a work observation transition")?
}

fn assert_close_rejected(observation: &WorkObservation) -> WorkObservationSnapshot {
    let snapshot = observation.snapshot();
    assert!(!snapshot.quiescent, "{snapshot:?}");
    match observation.shutdown_if_quiescent(&snapshot.revision) {
        GuardedShutdownOutcome::NotQuiescent(current)
        | GuardedShutdownOutcome::StaleRevision(current) => {
            assert!(!current.quiescent, "{current:?}");
            current
        }
        outcome => panic!("finite producer must prevent close: {outcome:?}"),
    }
}

// Quiescence is observed independently of the atomic close. Unrelated coordinator epoch
// updates can invalidate that revision while leaving the tree drained; use the returned
// snapshot to retry the guarded operation, as the host must, without accepting new work.
fn close_drained_tree(observation: &WorkObservation, mut drained: WorkObservationSnapshot) {
    for _ in 0..16 {
        assert!(
            drained.quiescent,
            "tree became active before close: {drained:?}"
        );
        match observation.shutdown_if_quiescent(&drained.revision) {
            GuardedShutdownOutcome::Closed(_) => return,
            GuardedShutdownOutcome::StaleRevision(current) => drained = current,
            outcome => panic!("expected drained tree to close: {outcome:?}"),
        }
    }
    panic!("drained tree revision did not settle: {drained:?}");
}

async fn requests(server: &StreamingSseServer) -> Result<Vec<Value>> {
    server
        .requests()
        .await
        .iter()
        .map(|body| Ok(serde_json::from_slice(body)?))
        .collect()
}

async fn request_count(server: &StreamingSseServer, count: usize) -> Result<()> {
    tokio::time::timeout(DEADLINE, server.wait_for_request_count(count))
        .await
        .context("expected a model request")
}

fn completion_texts(request: &Value) -> Vec<&str> {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| {
            let text = item["content"][0]["text"].as_str()?;
            text.starts_with("<async_tool_completion>").then_some(text)
        })
        .collect()
}

fn yielded_output(request: &Value) -> String {
    let item = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| {
            item["call_id"] == CALL_ID
                && item["type"]
                    .as_str()
                    .is_some_and(|kind| kind.ends_with("_call_output"))
        })
        .expect("yielded tool output must reach the model");
    if let Some(text) = item["output"].as_str() {
        text.to_owned()
    } else {
        item["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Copy, Debug)]
enum NativeOutcome {
    DefaultSuccess,
    ExplicitNonzero,
    Cancelled,
}

#[test_case::test_case(NativeOutcome::DefaultSuccess; "default_success")]
#[test_case::test_case(NativeOutcome::ExplicitNonzero; "explicit_task_nonzero")]
#[test_case::test_case(NativeOutcome::Cancelled; "cancelled")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finite_task_registration_prevents_quiescent_close_until_acceptance(
    outcome: NativeOutcome,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    finite_acceptance(
        Producer::Native,
        !matches!(outcome, NativeOutcome::DefaultSuccess),
        outcome,
    )
    .await
}

#[test_case::test_case(false; "default_cell")]
#[test_case::test_case(true; "explicit_task_cell")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cell_registration_prevents_quiescent_close_until_acceptance(
    explicit_task: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    finite_acceptance(Producer::Cell, explicit_task, NativeOutcome::DefaultSuccess).await
}

async fn finite_acceptance(
    producer: Producer,
    explicit_task: bool,
    outcome: NativeOutcome,
) -> Result<()> {
    let gate = NativeGate::new().await?;
    let exit_code = if matches!(outcome, NativeOutcome::ExplicitNonzero) {
        7
    } else {
        0
    };
    let args = gate.arguments(false, explicit_task, exit_code)?;
    let (accept, acceptance) = oneshot::channel();
    let (finish, final_answer) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: sse(vec![
                ev_response_created("launch"),
                producer.call(args, false, explicit_task),
                ev_completed("launch"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: assistant_response("initial-final"),
        }],
        vec![
            StreamingSseChunk {
                gate: Some(acceptance),
                body: sse(vec![ev_response_created("automatic")]),
            },
            StreamingSseChunk {
                gate: Some(final_answer),
                body: sse(vec![
                    ev_assistant_message("automatic-final", "finished"),
                    ev_completed("automatic"),
                ]),
            },
        ],
    ])
    .await;
    let (test, recorder, callbacks) = fixture(&server, producer).await?;
    let observation = test
        .codex
        .work_observation()
        .context("Exec exposes work observation")?;
    let (_, mut updates) = observation.subscribe();
    super::unified_exec::submit_unified_exec_turn(
        &test,
        "start gated work and answer",
        PermissionProfile::Disabled,
    )
    .await?;
    let (pid, mut stream) = gate.connected().await?;
    let initial = terminal_turn(&test).await?;
    assert!(initial.error.is_none(), "{initial:?}");
    test.codex.note_root_turn_output_forwarded(&initial.turn_id);
    let running = assert_close_rejected(&observation);
    assert_eq!(running.active_root_turns, 0);
    assert_eq!(running.pending_terminal_outputs, 0);
    assert!(running.running_finite_work > 0, "{running:?}");
    assert!(process_is_alive(&pid)?);
    let initial_requests = requests(&server).await?;
    assert_eq!(initial_requests.len(), 2);
    let output = yielded_output(&initial_requests[1]);
    if matches!(outcome, NativeOutcome::Cancelled) {
        let process_id = output
            .lines()
            .find_map(|line| line.strip_prefix("Process running with session ID "))
            .context("native session ID")?
            .parse()?;
        assert!(test.codex.terminate_background_terminal(process_id).await);
    } else {
        stream.get_mut().write_all(b"finish\n").await?;
    }
    request_count(&server, 3).await?;
    let bodies = requests(&server).await?;
    let delivered = completion_texts(&bodies[2]);
    assert_eq!(
        delivered.len(),
        1,
        "one automatic result for the yielded producer"
    );
    let receipt = delivered[0];
    assert!(serde_json::to_vec(&json!({"text":receipt}))?.len() <= 3072);
    assert!(receipt.contains(&format!("call={CALL_ID}")), "{receipt}");
    match producer {
        Producer::Native => {
            assert!(receipt.contains("kind=process"), "{receipt}");
            let process_id = output
                .lines()
                .find_map(|line| line.strip_prefix("Process running with session ID "))
                .context("yielded native session ID")?;
            assert!(
                receipt.contains(&format!("process={process_id}")),
                "{receipt}"
            );
            if matches!(outcome, NativeOutcome::Cancelled) {
                assert!(receipt.contains("status=cancelled"), "{receipt}");
            } else {
                assert!(
                    receipt.contains(&format!("status=exited({exit_code})")),
                    "{receipt}"
                );
                assert!(receipt.contains("terminal-native-tail"), "{receipt}");
            }
        }
        Producer::Cell => {
            assert_eq!(recorder.invocations()?.len(), 1, "real host was launched");
            let native = callbacks.borrow().clone();
            assert_eq!(
                native.ready, 1,
                "the native readiness callback elects collection once"
            );
            let cell_id = native.cell_id.context("real native Cell identity")?;
            assert!(
                output.contains(&format!("Script running with cell ID {cell_id}")),
                "{output}"
            );
            assert!(receipt.contains("kind=cell"), "{receipt}");
            assert!(receipt.contains(&format!("cell={cell_id}")), "{receipt}");
            assert!(receipt.contains("terminal-native-tail"), "{receipt}");
            assert!(receipt.contains("terminal-cell-tail"), "{receipt}");
        }
    }
    // The request already contains the history message, but no response.created has arrived.
    // The terminal producer itself must still contribute pending work at this boundary.
    let retained = assert_close_rejected(&observation);
    assert!(
        retained.pending_notifications > 0,
        "history append cannot release the result hold: {retained:?}"
    );
    accept.send(()).unwrap();
    observe(&mut updates, |snapshot| {
        snapshot.revision != retained.revision && snapshot.pending_notifications == 0
    })
    .await?;
    finish.send(()).unwrap();
    let automatic = terminal_turn(&test).await?;
    assert!(automatic.error.is_none(), "{automatic:?}");
    assert_ne!(initial.turn_id, automatic.turn_id);
    let awaiting_output = observation.snapshot();
    assert!(!awaiting_output.quiescent, "{awaiting_output:?}");
    assert_eq!(awaiting_output.pending_terminal_outputs, 1);
    test.codex
        .note_root_turn_output_forwarded(&automatic.turn_id);
    let drained = observe(&mut updates, |snapshot| snapshot.quiescent).await?;
    assert_eq!(drained.outstanding_work, 0);
    close_drained_tree(&observation, drained);
    test.codex.shutdown_and_wait().await?;
    assert_eq!(
        requests(&server).await?.len(),
        3,
        "no additional wake after acceptance and close"
    );
    if matches!(producer, Producer::Cell) {
        assert_eq!(callbacks.borrow().ready, 1);
        assert_eq!(callbacks.borrow().closed, 1);
    }
    assert!(!process_is_alive(&pid)?);
    server.shutdown().await;
    Ok(())
}

#[test_case::test_case(Producer::Native; "native_service")]
#[test_case::test_case(Producer::Cell; "cell_service")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_is_excluded_but_owned_shutdown_terminates_it(producer: Producer) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let gate = NativeGate::new().await?;
    let args = gate.arguments(true, false, 0)?;
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: sse(vec![
                ev_response_created("launch"),
                producer.call(args, true, false),
                ev_completed("launch"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: assistant_response("service-final"),
        }],
    ])
    .await;
    let (test, recorder, mut callbacks) = fixture(&server, producer).await?;
    let observation = test
        .codex
        .work_observation()
        .context("Exec exposes work observation")?;
    let (_, mut updates) = observation.subscribe();
    super::unified_exec::submit_unified_exec_turn(
        &test,
        "start owned service and answer",
        PermissionProfile::Disabled,
    )
    .await?;
    let (pid, mut stream) = gate.connected().await?;
    let initial = terminal_turn(&test).await?;
    assert!(initial.error.is_none(), "{initial:?}");
    test.codex.note_root_turn_output_forwarded(&initial.turn_id);
    let drained = observe(&mut updates, |snapshot| snapshot.quiescent).await?;
    assert_eq!(drained.outstanding_work, 0);
    assert_eq!(drained.running_finite_work, 0);
    assert!(
        process_is_alive(&pid)?,
        "the actual Service remains live after drain"
    );
    let bodies = requests(&server).await?;
    assert_eq!(bodies.len(), 2);
    let output = yielded_output(&bodies[1]);
    assert!(
        output.contains("service"),
        "yielded output reports classification: {output}"
    );
    let cell_id = if matches!(producer, Producer::Cell) {
        assert_eq!(recorder.invocations()?.len(), 1);
        let native = callbacks.borrow().clone();
        assert_eq!(native.ready, 0);
        assert_eq!(native.closed, 0);
        let cell_id = native
            .cell_id
            .context("real native Service cell identity")?;
        assert!(
            output.contains(&format!("Script running with cell ID {cell_id}")),
            "{output}"
        );
        Some(cell_id)
    } else {
        assert!(
            output.contains("Process running with session ID "),
            "{output}"
        );
        None
    };
    close_drained_tree(&observation, drained);
    tokio::time::timeout(DEADLINE, test.codex.shutdown_and_wait()).await??;
    let mut terminal = Vec::new();
    tokio::time::timeout(DEADLINE, stream.read_to_end(&mut terminal)).await??;
    assert!(
        terminal.is_empty(),
        "shutdown did not release the shell gate normally"
    );
    assert!(
        !process_is_alive(&pid)?,
        "owned shutdown stops the exact Service PID"
    );
    if let Some(cell_id) = cell_id {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let native = callbacks.borrow_and_update().clone();
                if native.closed > 0 {
                    assert_eq!(native.cell_id.as_deref(), Some(cell_id.as_str()));
                    assert_eq!(
                        native.closed, 1,
                        "native cell closes once during owned shutdown"
                    );
                    return Ok::<(), anyhow::Error>(());
                }
                callbacks.changed().await?;
            }
        })
        .await??;
    }
    assert_eq!(
        requests(&server).await?.len(),
        2,
        "Service teardown cannot wake the closed scope"
    );
    server.shutdown().await;
    Ok(())
}
