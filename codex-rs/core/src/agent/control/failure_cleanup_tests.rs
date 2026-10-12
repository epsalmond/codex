use super::*;
use crate::agent::control::spawn_guard::PendingSpawn;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use futures::FutureExt;
use pretty_assertions::assert_eq;

#[derive(Default)]
struct CommittedSpawn {
    pending: tokio::sync::Mutex<Option<PendingSpawn>>,
    child: tokio::sync::Mutex<Option<Arc<CodexThread>>>,
    ready: tokio::sync::Notify,
    held: tokio::sync::Notify,
    fail: tokio::sync::Notify,
    task_token: tokio::sync::Mutex<Option<CancellationToken>>,
}

struct HoldCommittedSpawn(Arc<CommittedSpawn>, FailureSource);

#[derive(Clone, Copy)]
enum FailureSource {
    Host,
    Natural,
}

struct CleanupCell(Arc<CommittedSpawn>);
impl codex_code_mode::CodeModeSessionProvider for CleanupCell {
    fn create_session(&self) -> codex_code_mode::CodeModeSessionProviderFuture<'_> {
        Box::pin(async {
            Ok(Arc::new(Self(Arc::clone(&self.0))) as Arc<dyn codex_code_mode::CodeModeSession>)
        })
    }
    fn create_session_with_limits<'a>(
        &'a self,
        _limits: codex_code_mode::CodeModeSessionCellExecutionLimits,
    ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
        self.create_session()
    }
}
impl codex_code_mode::CodeModeSession for CleanupCell {
    fn execute<'a>(
        &'a self,
        _request: codex_code_mode::ExecuteRequest,
        _delegate: Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
        _preempt: Option<CancellationToken>,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::StartedCell> {
        Box::pin(async { Err("cell already yielded".to_owned()) })
    }
    fn wait<'a>(
        &'a self,
        _request: codex_code_mode::WaitRequest,
        _preempt: Option<CancellationToken>,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async { Err("cell already yielded".to_owned()) })
    }
    fn terminate<'a>(
        &'a self,
        cell_id: codex_code_mode::CellId,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async move {
            assert!(
                self.0
                    .task_token
                    .lock()
                    .await
                    .as_ref()
                    .expect("root task token")
                    .is_cancelled(),
                "natural failure must fence retained task work before cell cleanup"
            );
            drop(self.0.pending.lock().await.take());
            let child = Arc::clone(self.0.child.lock().await.as_ref().expect("started child"));
            child.wait_until_terminated().await;
            Ok(codex_code_mode::WaitOutcome::LiveCell(
                codex_code_mode::RuntimeResponse::Terminated {
                    cell_id,
                    content_items: Vec::new(),
                    code_mode_host_duration: None,
                },
            ))
        })
    }
    fn shutdown<'a>(&'a self) -> codex_code_mode::CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct WaitForCancellation;
impl SessionTask for WaitForCancellation {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "test.running_descendant"
    }
    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancel: CancellationToken,
    ) -> SessionTaskResult {
        std::future::pending().await
    }
}

async fn start_waiting_descendant(thread: &CodexThread, turn_id: &str) -> String {
    if let Some(context) = thread
        .session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|active| {
            active
                .task
                .as_ref()
                .map(|task| Arc::clone(&task.turn_context))
        })
        .filter(|context| context.agent_assignment.get().is_some())
    {
        return context.sub_id.clone();
    }
    thread
        .session
        .abort_all_tasks(TurnAbortReason::Replaced)
        .await;
    let mut context = thread.session.new_default_turn().await;
    Arc::get_mut(&mut context).expect("new context").sub_id = turn_id.to_owned();
    thread
        .session
        .bind_wake_assignment(&context, /*allow_new_generation*/ true)
        .expect("descendant assignment");
    assert!(context.agent_assignment.get().is_some());
    thread
        .session
        .spawn_task(context, Vec::new(), WaitForCancellation)
        .await;
    turn_id.to_owned()
}

async fn spawn_committed_child(
    harness: &AgentControlHarness,
    control: &LocalAgentControl,
    parent: &CodexThread,
    parent_turn_id: &str,
    name: &str,
) -> Arc<CodexThread> {
    let source = thread_spawn_source(
        parent.session.thread_id,
        &parent.session_source,
        next_thread_spawn_depth(&parent.session_source),
        /*agent_role*/ None,
        Some(name.to_owned()),
    )
    .expect("child source");
    let created = control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("hello"),
            source,
            SpawnAgentOptions {
                parent_thread_id: Some(parent.session.thread_id),
                parent_turn_id: Some(parent_turn_id.to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("committed wake child");
    harness.manager.get_thread(created.thread_id).await.unwrap()
}
impl SessionTask for HoldCommittedSpawn {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "test.hold_committed_spawn"
    }
    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancel: CancellationToken,
    ) -> SessionTaskResult {
        *self.0.task_token.lock().await = Some(cancel);
        self.0.ready.notified().await;
        if matches!(self.1, FailureSource::Natural) {
            self.0.held.notify_one();
            self.0.fail.notified().await;
            session
                .send_event(
                    &context,
                    EventMsg::Error(ErrorEvent {
                        message: "natural failure".to_owned(),
                        codex_error_info: None,
                        misalignment: None,
                    }),
                )
                .await;
            return Ok(None);
        }
        let _pending = self
            .0
            .pending
            .lock()
            .await
            .take()
            .expect("committed spawn guard");
        self.0.held.notify_one();
        std::future::pending().await
    }
    async fn abort(&self, _session: Arc<Session>, _context: Arc<TurnContext>) {
        // Force the guard's Interrupted child teardown to finish before root finalization.
        let child = self.0.child.lock().await.clone().expect("child started");
        child.wait_until_terminated().await;
    }
}

#[test_case::test_case(FailureSource::Host; "host")]
#[test_case::test_case(FailureSource::Natural; "natural")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_captures_grandchild_before_committed_spawn_cleanup_detaches_it(
    source: FailureSource,
) {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.disable(Feature::CodeModeHost).unwrap();
    config.features.disable(Feature::CodeModeInterrupt).unwrap();
    let mut harness = AgentControlHarness::new_with_config(home, config).await;
    let held = Arc::new(CommittedSpawn::default());
    // Release the harness's Weak manager reference before installing the provider.
    harness.control = LocalAgentControl::default();
    harness.manager = harness
        .manager
        .with_code_mode_session_provider(Arc::new(CleanupCell(Arc::clone(&held))));
    harness.control = harness.manager.agent_control();
    harness.config.multi_agent_v2.agent_polling = AgentPolling::Disabled;
    let cwd = PathUri::from_abs_path(&harness.config.codex_home);
    let root = harness
        .manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![codex_protocol::protocol::TurnEnvironmentRequest {
                environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_owned(),
                cwd: cwd.clone(),
                workspace_roots: vec![cwd],
                config: EnvironmentConfigState::Pending,
            }]),
            ..StartThreadOptions::new(harness.config.clone())
        })
        .await
        .expect("root")
        .thread;
    let runtime = &root.session.services.local_agent_runtime;
    runtime.enable_wake_mode();
    let mut context = root.session.new_default_turn().await;
    Arc::get_mut(&mut context).expect("new context").sub_id = "root-turn".to_owned();
    root.session
        .bind_wake_assignment(&context, /*allow_new_generation*/ true)
        .unwrap();
    root.session
        .spawn_task(
            Arc::clone(&context),
            Vec::new(),
            HoldCommittedSpawn(Arc::clone(&held), source),
        )
        .await;
    let control = runtime.control(root.session.session_id());
    let child = spawn_committed_child(&harness, &control, &root, &context.sub_id, "child").await;
    let child_turn_id = start_waiting_descendant(&child, "child-turn").await;
    let child_control = runtime.control(child.session.session_id());
    let grandchild = spawn_committed_child(
        &harness,
        &child_control,
        &child,
        &child_turn_id,
        "grandchild",
    )
    .await;
    let _ = start_waiting_descendant(&grandchild, "grandchild-turn").await;
    let child_assignment = runtime
        .wake_coordinator
        .current_assignment(child.session.thread_id)
        .expect("child assignment");
    let operation = runtime
        .retain_assignment_finalization(&child_assignment)
        .expect("child operation");
    let mut guard = PendingSpawn::new(
        runtime.upgrade().unwrap(),
        child.session.thread_id,
        runtime.admit_start().expect("admit pending spawn"),
    );
    guard.operation = Some(operation);
    *held.pending.lock().await = Some(guard);
    *held.child.lock().await = Some(Arc::clone(&child));
    held.ready.notify_one();
    timeout(Duration::from_secs(10), held.held.notified())
        .await
        .unwrap();
    if matches!(source, FailureSource::Natural) {
        let cell_id = codex_code_mode::CellId::new("nested-spawn".to_owned());
        // The broker only tracks dispatch; install the controlled runtime session as well.
        assert!(
            root.session
                .services
                .code_mode_service
                .wait(
                    codex_code_mode::WaitRequest {
                        cell_id: cell_id.clone(),
                        yield_time_ms: 0,
                    },
                    /*preempt*/ None
                )
                .await
                .is_err()
        );
        root.session
            .services
            .code_mode_service
            .mark_cell_ready_for_dispatch(&cell_id, /*originating_call*/ None);
        held.fail.notify_one();
        timeout(Duration::from_secs(10), async {
            loop {
                if matches!(
                    root.next_event().await.unwrap().msg,
                    EventMsg::TurnComplete(_)
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    } else {
        root.session
            .fail_turn_if_active(
                &context.sub_id,
                ErrorEvent {
                    message: "root host failure".to_owned(),
                    codex_error_info: None,
                    misalignment: None,
                },
            )
            .await;
    }
    assert!(child.wait_until_terminated().now_or_never().is_some());
    assert!(
        grandchild.wait_until_terminated().now_or_never().is_some(),
        "grandchild cannot escape committed spawn cleanup"
    );
    assert_eq!(
        harness.manager.list_thread_ids().await,
        vec![root.session.thread_id]
    );
}
