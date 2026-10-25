//! Deterministic startup/shutdown and cancelled-initial-response ownership tests.
use super::*;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionResultFuture;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::StartedCell;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

struct GatedSession {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    start: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    terminating: Mutex<Option<oneshot::Sender<()>>>,
    terminate: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    delegate: Mutex<Option<Arc<dyn CodeModeSessionDelegate>>>,
    stopped: AtomicBool,
}

impl CodeModeSession for GatedSession {
    fn execute<'a>(
        &'a self,
        _request: ExecuteRequest,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async move {
            *self.delegate.lock().unwrap() = Some(delegate);
            self.entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            self.start.lock().await.take().unwrap().await.unwrap();
            Ok(StartedCell::from_future(
                CellId::new("gated-cell".to_owned()),
                std::future::pending(),
            ))
        })
    }

    fn wait<'a>(
        &'a self,
        _request: WaitRequest,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { Err("fixture does not wait".to_owned()) })
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            self.terminating
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            self.terminate.lock().await.take().unwrap().await.unwrap();
            self.delegate
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .cell_closed(&cell_id);
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id,
                content_items: vec![],
                code_mode_host_duration: None,
            }))
        })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async move {
            assert!(self.delegate.lock().unwrap().is_none());
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

struct Fixture {
    owner: Arc<Session>,
    turn: Arc<TurnContext>,
    provider: Arc<GatedSession>,
    entered: oneshot::Receiver<()>,
    start: oneshot::Sender<()>,
    terminating: oneshot::Receiver<()>,
    terminate: oneshot::Sender<()>,
}

async fn fixture() -> Fixture {
    let (session, turn) = make_session_and_context().await;
    let owner = Arc::new(session);
    let (entered_tx, entered) = oneshot::channel();
    let (start, start_rx) = oneshot::channel();
    let (terminating_tx, terminating) = oneshot::channel();
    let (terminate, terminate_rx) = oneshot::channel();
    let provider = Arc::new(GatedSession {
        entered: Mutex::new(Some(entered_tx)),
        start: tokio::sync::Mutex::new(Some(start_rx)),
        terminating: Mutex::new(Some(terminating_tx)),
        terminate: tokio::sync::Mutex::new(Some(terminate_rx)),
        delegate: Mutex::new(None),
        stopped: AtomicBool::new(false),
    });
    let session: Arc<dyn CodeModeSession> = provider.clone();
    assert!(
        owner
            .services
            .code_mode_service
            .session
            .set(session)
            .is_ok()
    );
    Fixture {
        owner,
        turn: Arc::new(turn),
        provider,
        entered,
        start,
        terminating,
        terminate,
    }
}

async fn execute(
    owner: Arc<Session>,
    turn: Arc<TurnContext>,
    kind: WorkKind,
) -> Result<(StartedCell, InitialCellGuard), String> {
    let runtime = &owner.services.local_agent_runtime;
    let registration = runtime.reserve_owned_work(&owner, &turn, "startup", kind, None)?;
    registration.mark_cell();
    let membership = runtime.admit_start().map_err(|error| error.to_string())?;
    owner
        .services
        .code_mode_service
        .execute(
            ExecuteRequest {
                tool_call_id: "startup".to_owned(),
                enabled_tools: vec![],
                source: "text('startup')".to_owned(),
                yield_time_ms: None,
                max_output_tokens: None,
            },
            crate::session::step_context::StepContext::for_test(turn),
            Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
            None,
            registration,
            membership,
            None,
            Arc::clone(&owner),
        )
        .await
}

#[tokio::test]
async fn cell_start_racing_shutdown_is_confirmed_closed_before_provider_shutdown() {
    let Fixture {
        owner,
        turn,
        provider,
        entered,
        start,
        terminating,
        terminate,
    } = fixture().await;
    // Services and feature-off cells must use the same startup fence as automatic cells.
    let startup = tokio::spawn(execute(Arc::clone(&owner), turn, WorkKind::Service));
    entered.await.unwrap();
    let shutdown_owner = Arc::clone(&owner);
    let shutdown =
        tokio::spawn(async move { shutdown_owner.services.code_mode_service.shutdown().await });
    owner
        .services
        .code_mode_service
        .shutdown_token
        .cancelled()
        .await;
    assert!(!shutdown.is_finished());
    start.send(()).unwrap();
    terminating.await.unwrap();
    assert!(!shutdown.is_finished());
    assert!(!provider.stopped.load(Ordering::SeqCst));
    terminate.send(()).unwrap();
    assert!(startup.await.unwrap().is_err());
    shutdown.await.unwrap().unwrap();
    assert!(provider.stopped.load(Ordering::SeqCst));
    assert!(
        !owner
            .services
            .local_agent_runtime
            .has_live_owned_work(owner.thread_id())
    );
}

#[tokio::test]
async fn cancelled_cell_initial_response_keeps_startup_membership_until_native_close() {
    let Fixture {
        owner,
        turn,
        provider,
        entered,
        start,
        terminating,
        terminate,
    } = fixture().await;
    let startup = tokio::spawn(execute(Arc::clone(&owner), turn, WorkKind::Cell));
    entered.await.unwrap();
    start.send(()).unwrap();
    let (started, guard) = startup.await.unwrap().unwrap();
    assert!(
        owner
            .services
            .code_mode_service
            .work_registration(&started.cell_id)
            .is_some()
    );
    drop(started);
    drop(guard);
    terminating.await.unwrap();
    let shutdown_owner = Arc::clone(&owner);
    let shutdown =
        tokio::spawn(async move { shutdown_owner.services.code_mode_service.shutdown().await });
    owner
        .services
        .code_mode_service
        .shutdown_token
        .cancelled()
        .await;
    assert!(!shutdown.is_finished());
    assert!(!provider.stopped.load(Ordering::SeqCst));
    terminate.send(()).unwrap();
    shutdown.await.unwrap().unwrap();
    assert!(
        !owner
            .services
            .local_agent_runtime
            .has_live_owned_work(owner.thread_id())
    );
}

#[tokio::test]
async fn cancelled_cell_startup_cleans_up_provider_publication_after_caller_drops() {
    let Fixture {
        owner,
        turn,
        provider,
        entered,
        start,
        terminating,
        terminate,
    } = fixture().await;
    let startup = tokio::spawn(execute(Arc::clone(&owner), turn, WorkKind::Cell));
    entered.await.unwrap();
    startup.abort();
    assert!(startup.await.err().unwrap().is_cancelled());
    start.send(()).unwrap();
    terminating.await.unwrap();
    assert!(
        owner
            .services
            .local_agent_runtime
            .has_live_owned_work(owner.thread_id())
    );
    let shutdown_owner = Arc::clone(&owner);
    let shutdown =
        tokio::spawn(async move { shutdown_owner.services.code_mode_service.shutdown().await });
    owner
        .services
        .code_mode_service
        .shutdown_token
        .cancelled()
        .await;
    assert!(!shutdown.is_finished());
    terminate.send(()).unwrap();
    shutdown.await.unwrap().unwrap();
    assert!(provider.stopped.load(Ordering::SeqCst));
    assert!(
        !owner
            .services
            .local_agent_runtime
            .has_live_owned_work(owner.thread_id())
    );
}
