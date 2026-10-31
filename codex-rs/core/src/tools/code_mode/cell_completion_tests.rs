use super::*;
use crate::session::tests::make_session_and_context;
use codex_code_mode::CellTerminalStatus;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionResultFuture;
use codex_code_mode::ExecuteRequest;
use pretty_assertions::assert_eq;

const CELL: &str = "generation:cell-1";

/// Returns one fixed terminal result to the collector's read.
struct FixedSession {
    response: RuntimeResponse,
    completion: Weak<CellCompletion>,
}

fn fixed_session(
    response: RuntimeResponse,
    completion: &Arc<CellCompletion>,
) -> Arc<dyn CodeModeSession> {
    Arc::new(FixedSession {
        response,
        completion: Arc::downgrade(completion),
    })
}

impl CodeModeSession for FixedSession {
    fn execute<'a>(
        &'a self,
        _request: ExecuteRequest,
        _delegate: Arc<dyn CodeModeSessionDelegate>,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async { Err("fixture does not execute".to_owned()) })
    }

    fn wait<'a>(
        &'a self,
        _request: WaitRequest,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            let response = self.response.clone();
            if !matches!(response, RuntimeResponse::Yielded { .. })
                && let Some(completion) = self.completion.upgrade()
            {
                completion.closed(&cell_id());
            }
            Ok(WaitOutcome::LiveCell(response))
        })
    }

    fn terminate<'a>(&'a self, _cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { Err("fixture does not terminate".to_owned()) })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

fn text(text: &str) -> FunctionCallOutputContentItem {
    FunctionCallOutputContentItem::InputText {
        text: text.to_owned(),
    }
}

fn image() -> FunctionCallOutputContentItem {
    FunctionCallOutputContentItem::InputImage {
        image_url: "data:image/png;base64,full-media".to_owned(),
        detail: None,
    }
}

fn cell_id() -> CellId {
    CellId::new(CELL.to_owned())
}

fn started_cell() -> StartedCell {
    let mut started = StartedCell::from_future(cell_id(), async {
        Err("fixture initial response is supplied directly".to_owned())
    });
    started.terminal_ready_support = TerminalReadySupport::Supported;
    started
}

fn yielded() -> RuntimeResponse {
    RuntimeResponse::Yielded {
        cell_id: cell_id(),
        content_items: vec![],
        code_mode_host_duration: None,
    }
}

fn ready(output_kind: CellOutputKind) -> CellTerminalReady {
    CellTerminalReady {
        cell_id: cell_id(),
        status: CellTerminalStatus::Completed,
        output_kind,
    }
}

struct Fixture {
    session: Arc<Session>,
    completion: Arc<CellCompletion>,
    // The binding holds the provider weakly; keep it alive for the collector.
    _provider: Arc<dyn CodeModeSession>,
}

impl Fixture {
    /// A bound, yielded cell whose collector reads `terminal`.
    async fn yielded(terminal: RuntimeResponse, wait_hidden: bool) -> Self {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let reservation = session
            .services
            .async_completions
            .reserve_cell(session.thread_id(), &turn, "cell-call")
            .expect("reserve cell");
        let completion = CellCompletion::new(
            reservation,
            None,
            &session,
            turn,
            CancellationToken::new(),
            wait_hidden,
        );
        let provider: Arc<dyn CodeModeSession> = fixed_session(terminal, &completion);
        completion.admit(&started_cell(), &provider);
        completion.initial(&yielded());
        Self {
            session,
            completion,
            _provider: provider,
        }
    }

    fn store(&self) -> &Arc<crate::session::async_completion::AsyncCompletions> {
        &self.session.services.async_completions
    }

    async fn published(&self) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let texts = self.store().published_texts();
                if !texts.is_empty() && !self.completion.collecting() {
                    return texts;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cell completion published")
    }
}

#[tokio::test]
async fn cell_completion_initial_yield_is_visible_before_terminal_readiness() {
    let fixture = Fixture::yielded(yielded(), /*wait_hidden*/ false).await;
    let pending = fixture.store().pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].cell_id.as_deref(), Some(CELL));
    assert!(!pending[0].ready);
    assert!(fixture.store().published_texts().is_empty());
    assert!(!fixture.store().has_ready(fixture.session.thread_id(), None));
}

#[test_case::test_case(None; "inline_result")]
#[test_case::test_case(Some("inline error"); "inline_error")]
#[tokio::test]
async fn cell_completion_initial_inline_response_is_never_visible(error: Option<&str>) {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let store = &session.services.async_completions;
    let reservation = store
        .reserve_cell(session.thread_id(), &turn, "inline-cell")
        .unwrap();
    let completion = CellCompletion::new(
        reservation,
        None,
        &session,
        turn,
        CancellationToken::new(),
        /*wait_hidden*/ false,
    );
    let provider = fixed_session(yielded(), &completion);
    completion.admit(&started_cell(), &provider);
    assert!(store.pending().is_empty());
    assert!(!completion.initial(&RuntimeResponse::Result {
        cell_id: cell_id(),
        content_items: vec![text("inline output")],
        error_text: error.map(str::to_owned),
        result_buffered_for: Duration::ZERO,
        code_mode_host_duration: None,
    }));
    assert!(store.pending().is_empty());
    assert!(!store.has_owned_work());
}

#[tokio::test]
async fn cell_completion_cancelled_initial_releases_without_a_close_or_owner_cycle() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let weak = Arc::downgrade(&session);
    let turn = Arc::new(turn);
    let store = Arc::clone(&session.services.async_completions);
    let reservation = store
        .reserve_cell(session.thread_id(), &turn, "cancelled-initial")
        .unwrap();
    let completion = CellCompletion::new(
        reservation,
        None,
        &session,
        Arc::clone(&turn),
        CancellationToken::new(),
        /*wait_hidden*/ false,
    );
    assert_eq!(Arc::strong_count(&session), 1);
    drop(InitialCompletion(Arc::clone(&completion)));
    let mut held = vec![];
    for _ in 0..64 {
        held.push(
            store
                .reserve_cell(session.thread_id(), &turn, "capacity-after-cancel")
                .unwrap(),
        );
    }
    assert!(
        store
            .reserve_cell(session.thread_id(), &turn, "full")
            .is_err()
    );
    drop(session);
    assert!(weak.upgrade().is_none());
    assert!(completion.owner.upgrade().is_none());
}

#[test]
fn cell_completion_text_tail_bounds_allocation_before_publication() {
    let items = vec![text(&"large-output".repeat(10000)), text("newest")];
    let (tail, omitted) = text_tail(&items, Some("script-error"));
    assert_eq!(
        (tail.len(), omitted),
        (MAX_FRAGMENT_BYTES, 120000 + 6 + 12 + 2 - MAX_FRAGMENT_BYTES)
    );
    assert!(tail.ends_with(b"newest\nscript-error"));
}

#[test]
fn cell_completion_text_tail_notes_omitted_media_last() {
    let items = vec![text("caption"), image(), image()];
    let (tail, omitted) = text_tail(&items, /*error*/ None);
    assert_eq!(
        (String::from_utf8(tail).unwrap(), omitted),
        ("caption\n[2 image/audio items omitted]".to_owned(), 0)
    );
}

#[tokio::test]
async fn cell_completion_publishes_text_tail_once_on_readiness() {
    let fixture = Fixture::yielded(
        RuntimeResponse::Result {
            cell_id: cell_id(),
            content_items: vec![text("terminal-tail")],
            error_text: None,
            result_buffered_for: Duration::ZERO,
            code_mode_host_duration: None,
        },
        /*wait_hidden*/ false,
    )
    .await;
    fixture.completion.ready(ready(CellOutputKind::Text));
    fixture.completion.ready(ready(CellOutputKind::Text));
    let published = fixture.published().await;
    assert_eq!(published.len(), 1);
    assert!(published[0].contains(&format!("kind=cell cell={CELL}")));
    assert!(published[0].contains("status=completed"));
    assert!(published[0].ends_with("\nterminal-tail"));
}

#[tokio::test]
async fn cell_completion_media_ready_is_left_for_visible_wait() {
    let fixture = Fixture::yielded(
        RuntimeResponse::Result {
            cell_id: cell_id(),
            content_items: vec![text("caption"), image()],
            error_text: None,
            result_buffered_for: Duration::ZERO,
            code_mode_host_duration: None,
        },
        /*wait_hidden*/ false,
    )
    .await;
    fixture.completion.ready(ready(CellOutputKind::Media));
    tokio::task::yield_now().await;
    assert!(!fixture.store().has_owned_work());
    assert!(fixture.store().published_texts().is_empty());
}

#[tokio::test]
async fn cell_completion_media_ready_publishes_text_and_omission_when_wait_is_hidden() {
    let fixture = Fixture::yielded(
        RuntimeResponse::Result {
            cell_id: cell_id(),
            content_items: vec![text("caption"), image()],
            error_text: None,
            result_buffered_for: Duration::ZERO,
            code_mode_host_duration: None,
        },
        /*wait_hidden*/ true,
    )
    .await;
    fixture.completion.ready(ready(CellOutputKind::Media));
    let published = fixture.published().await;
    assert_eq!(published.len(), 1);
    assert!(published[0].contains("status=completed"));
    assert!(published[0].ends_with("\ncaption\n[1 image/audio items omitted]"));
    assert!(!published[0].contains("full-media"));
}

#[tokio::test]
async fn cell_completion_model_read_releases_without_publication() {
    let fixture = Fixture::yielded(yielded(), /*wait_hidden*/ false).await;
    fixture
        .completion
        .observed(&Ok(WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: cell_id(),
            content_items: vec![text("read-by-model")],
            error_text: None,
            result_buffered_for: Duration::ZERO,
            code_mode_host_duration: None,
        })));
    fixture.completion.ready(ready(CellOutputKind::Text));
    tokio::task::yield_now().await;
    assert!(!fixture.store().has_owned_work());
}

#[tokio::test]
async fn cell_completion_interrupt_publishes_cancelled_tail_once() {
    let fixture = Fixture::yielded(yielded(), /*wait_hidden*/ false).await;
    let terminated = Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated {
        cell_id: cell_id(),
        content_items: vec![text("partial-tail")],
        code_mode_host_duration: None,
    }));
    // Providers close the cell during terminate, before it returns the terminated result.
    fixture.completion.begin_interrupt();
    fixture.completion.closed(&cell_id());
    fixture.completion.interrupted(&terminated);
    fixture.completion.interrupted(&terminated);
    fixture.completion.ready(ready(CellOutputKind::Text));
    let published = fixture.store().published_texts();
    assert_eq!(published.len(), 1);
    assert!(published[0].contains("status=cancelled"));
    assert!(published[0].ends_with("\npartial-tail"));
    assert!(
        fixture
            .store()
            .has_ready(fixture.session.thread_id(), /*assignment*/ None)
    );
}

#[tokio::test]
async fn cell_completion_unsupported_readiness_releases_at_admission() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let reservation = session
        .services
        .async_completions
        .reserve_cell(session.thread_id(), &turn, "cell-call")
        .unwrap();
    let completion = CellCompletion::new(
        reservation,
        None,
        &session,
        turn,
        CancellationToken::new(),
        /*wait_hidden*/ false,
    );
    let provider: Arc<dyn CodeModeSession> = fixed_session(yielded(), &completion);
    let mut started = started_cell();
    started.terminal_ready_support = TerminalReadySupport::Unsupported;
    completion.admit(&started, &provider);
    assert!(!session.services.async_completions.has_owned_work());
}

#[tokio::test]
async fn cell_completion_terminate_after_readiness_before_collection_releases() {
    let fixture = Fixture::yielded(
        RuntimeResponse::Result {
            cell_id: cell_id(),
            content_items: vec![text("collected")],
            error_text: None,
            result_buffered_for: Duration::ZERO,
            code_mode_host_duration: None,
        },
        /*wait_hidden*/ true,
    )
    .await;
    // `exec_terminate` holds the observation lock when readiness elects the collector.
    let permit = fixture.completion.observe().await;
    fixture.completion.ready(ready(CellOutputKind::Text));
    tokio::task::yield_now().await;
    fixture
        .completion
        .observed(&Ok(WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: cell_id(),
            content_items: vec![text("read-by-terminate")],
            error_text: None,
            result_buffered_for: Duration::ZERO,
            code_mode_host_duration: None,
        })));
    drop(permit);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(fixture.store().published_texts().is_empty());
    assert!(!fixture.store().has_owned_work());
}

/// A provider that reports readiness support for the session but not for this cell.
async fn watched_fixture(terminal: RuntimeResponse) -> Fixture {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let reservation = session
        .services
        .async_completions
        .reserve_cell(session.thread_id(), &turn, "cell-call")
        .expect("reserve cell");
    let completion = CellCompletion::new(
        reservation,
        None,
        &session,
        turn,
        CancellationToken::new(),
        /*wait_hidden*/ true,
    );
    let provider: Arc<dyn CodeModeSession> = fixed_session(terminal, &completion);
    let mut started = started_cell();
    started.terminal_ready_support = TerminalReadySupport::Unsupported;
    completion.admit(&started, &provider);
    completion.initial(&yielded());
    Fixture {
        session,
        completion,
        _provider: provider,
    }
}

#[tokio::test]
async fn cell_completion_watches_unsupported_cell_when_wait_is_hidden() {
    let fixture = watched_fixture(RuntimeResponse::Result {
        cell_id: cell_id(),
        content_items: vec![text("watched-tail")],
        error_text: None,
        result_buffered_for: Duration::ZERO,
        code_mode_host_duration: None,
    })
    .await;
    let published = fixture.published().await;
    assert_eq!(published.len(), 1);
    assert!(published[0].contains("kind=cell"));
    assert!(published[0].contains("status=completed"));
    assert!(published[0].ends_with("\nwatched-tail"));
}

#[tokio::test]
async fn cell_completion_cancelled_watcher_leaves_the_result_to_terminate() {
    let fixture = watched_fixture(yielded()).await;
    fixture.completion.cancel_watcher();
    let _permit = fixture.completion.observe().await;
    fixture
        .completion
        .observed(&Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated {
            cell_id: cell_id(),
            content_items: vec![text("read-by-terminate")],
            code_mode_host_duration: None,
        })));
    assert!(fixture.store().published_texts().is_empty());
    assert!(!fixture.store().has_owned_work());
}

#[test]
fn cell_completion_watcher_tail_keeps_newest_bytes() {
    let mut tail = Vec::new();
    let mut omitted = 0;
    append_tail(&mut tail, &mut omitted, b"first", 0);
    append_tail(&mut tail, &mut omitted, b"second", 0);
    assert_eq!(tail, b"first\nsecond");
    assert_eq!(omitted, 0);
    let long = vec![b'x'; MAX_FRAGMENT_BYTES];
    append_tail(&mut tail, &mut omitted, &long, 3);
    assert_eq!(tail, long);
    assert_eq!(omitted, "first\nsecond\n".len() + 3);
}

/// Uses the real delegate/native-close callback rather than releasing a completion reservation
/// as a substitute for closing a cell.
struct ClosingSession {
    terminal: Result<RuntimeResponse, String>,
    delegate: Arc<super::super::delegate::CodeModeCellDelegate>,
    waits: std::sync::atomic::AtomicUsize,
    terminations: std::sync::atomic::AtomicUsize,
    fail_termination: std::sync::atomic::AtomicBool,
    missing_after_close: std::sync::atomic::AtomicBool,
    /// A provider may return its response before the native close notification arrives.
    close_gate: Option<CancellationToken>,
}

impl ClosingSession {
    fn close(&self, cell_id: CellId) {
        if let Some(gate) = &self.close_gate {
            let gate = gate.clone();
            let delegate = Arc::clone(&self.delegate);
            tokio::spawn(async move {
                gate.cancelled().await;
                delegate.cell_closed(&cell_id);
            });
        } else {
            self.delegate.cell_closed(&cell_id);
        }
    }
}

impl CodeModeSession for ClosingSession {
    fn execute<'a>(
        &'a self,
        _request: ExecuteRequest,
        _delegate: Arc<dyn CodeModeSessionDelegate>,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async { Err("fixture starts through the native callback binding".to_owned()) })
    }

    fn wait<'a>(
        &'a self,
        request: WaitRequest,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            self.waits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if request.cell_id != cell_id() {
                return Ok(WaitOutcome::MissingCell(RuntimeResponse::Result {
                    cell_id: request.cell_id,
                    content_items: vec![],
                    error_text: Some("cell not found".to_owned()),
                    result_buffered_for: Duration::ZERO,
                    code_mode_host_duration: None,
                }));
            }
            if self.terminal.is_ok() {
                self.close(request.cell_id);
            }
            self.terminal.clone().map(|response| {
                if self
                    .missing_after_close
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    WaitOutcome::MissingCell(response)
                } else {
                    WaitOutcome::LiveCell(response)
                }
            })
        })
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            self.terminations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .missing_after_close
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(WaitOutcome::MissingCell(RuntimeResponse::Result {
                    cell_id,
                    content_items: vec![],
                    error_text: Some("cell not found".to_owned()),
                    result_buffered_for: Duration::ZERO,
                    code_mode_host_duration: None,
                }));
            }
            if self
                .fail_termination
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err("fixture termination failed".to_owned());
            }
            self.close(cell_id.clone());
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id,
                content_items: vec![text("cancelled-tail")],
                code_mode_host_duration: None,
            }))
        })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct NativeFixture {
    base: Fixture,
    delegate: Arc<super::super::delegate::CodeModeCellDelegate>,
    provider: Arc<ClosingSession>,
}

impl NativeFixture {
    async fn yielded(
        kind: crate::agent::control::WorkKind,
        terminal: Result<RuntimeResponse, String>,
        readiness: TerminalReadySupport,
    ) -> Self {
        Self::with_options(kind, terminal, readiness, false, None, false).await
    }

    async fn with_options(
        kind: crate::agent::control::WorkKind,
        terminal: Result<RuntimeResponse, String>,
        readiness: TerminalReadySupport,
        wait_hidden: bool,
        close_gate: Option<CancellationToken>,
        fail_termination: bool,
    ) -> Self {
        Self::with_options_and_assignment(
            kind,
            terminal,
            readiness,
            wait_hidden,
            close_gate,
            fail_termination,
            false,
        )
        .await
    }

    async fn with_options_and_assignment(
        kind: crate::agent::control::WorkKind,
        terminal: Result<RuntimeResponse, String>,
        readiness: TerminalReadySupport,
        wait_hidden: bool,
        close_gate: Option<CancellationToken>,
        fail_termination: bool,
        bind_assignment: bool,
    ) -> Self {
        use crate::session::step_context::StepContext;
        use crate::tools::code_mode::delegate::CodeModeCellDelegate;
        let (session, mut turn) = make_session_and_context().await;
        if bind_assignment {
            use codex_protocol::protocol::SessionSource;
            use codex_protocol::protocol::SubAgentSource;

            let runtime = &session.services.local_agent_runtime;
            runtime.enable_wake_mode();
            let parent_thread_id = codex_protocol::ThreadId::new();
            runtime
                .begin_wake_assignment_for_turn(
                    parent_thread_id,
                    &SessionSource::Cli,
                    "parent-turn",
                    true,
                )
                .unwrap();
            turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            });
            let assignment = runtime
                .begin_wake_assignment_for_turn(
                    session.thread_id(),
                    &turn.session_source,
                    &turn.sub_id,
                    true,
                )
                .unwrap()
                .unwrap();
            turn.agent_assignment.set(assignment).unwrap();
        }
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let registration = session
            .services
            .local_agent_runtime
            .reserve_owned_work(&session, &turn, "cell-call", kind, None)
            .unwrap();
        registration.mark_cell();
        registration.mark_started();
        // Quiescence is meaningful only after a host subscribes to output forwarding.
        let _ = session
            .services
            .local_agent_runtime
            .work_observation()
            .subscribe();
        let reservation = session
            .services
            .async_completions
            .reserve_cell_with_lifecycle(session.thread_id(), &turn, "cell-call", &registration)
            .unwrap();
        let completion = CellCompletion::new(
            reservation,
            Some(registration.clone()),
            &session,
            Arc::clone(&turn),
            CancellationToken::new(),
            wait_hidden,
        );
        let broker = Arc::clone(&session.services.code_mode_service.dispatch_broker);
        let delegate = Arc::new(CodeModeCellDelegate {
            broker: Arc::clone(&broker),
            step_context: StepContext::for_test(turn),
            tracker: Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
            completion: Some(Arc::clone(&completion)),
            work_registration: Mutex::new(Some(registration.clone())),
        });
        broker.attach_registration(&cell_id(), &delegate);
        let provider = Arc::new(ClosingSession {
            terminal,
            delegate: Arc::clone(&delegate),
            waits: Default::default(),
            terminations: Default::default(),
            fail_termination: std::sync::atomic::AtomicBool::new(fail_termination),
            missing_after_close: Default::default(),
            close_gate,
        });
        let dynamic_provider: Arc<dyn CodeModeSession> = provider.clone();
        session
            .services
            .code_mode_service
            .session
            .set(Arc::clone(&dynamic_provider))
            .unwrap_or_else(|_| panic!("fixture provider already initialized"));
        let mut started = started_cell();
        started.terminal_ready_support = readiness;
        completion.admit(&started, &dynamic_provider);
        broker.attach_completion(&cell_id(), Arc::clone(&completion));
        let automatic = completion.initial(&yielded());
        if !automatic {
            registration.mark_yielded(false);
        }
        drop(registration);
        Self {
            base: Fixture {
                session,
                completion,
                _provider: dynamic_provider,
            },
            delegate,
            provider,
        }
    }

    fn retire_ready_record(&self) {
        assert!(self.base.store().has_ready(
            self.base.session.thread_id(),
            self.base.completion.turn.agent_assignment.get(),
        ));
        assert!(self.delegate.work_registration.lock().unwrap().is_none());
        // Actual request acceptance is covered at the session handler boundary. Here retirement
        // proves the existing record is the only remaining registration owner after native close.
        self.base.store().retire();
    }

    fn live(&self) -> bool {
        self.base
            .session
            .services
            .local_agent_runtime
            .has_live_owned_work(self.base.session.thread_id())
    }
}

#[tokio::test]
async fn compatibility_cell_reservation_release_keeps_native_registration_until_close() {
    let fixture = NativeFixture::yielded(
        crate::agent::control::WorkKind::Cell,
        Ok(yielded()),
        TerminalReadySupport::Unsupported,
    )
    .await;
    assert!(!fixture.base.store().has_owned_work());
    assert!(fixture.live());
    let observation = fixture
        .base
        .session
        .services
        .local_agent_runtime
        .work_observation();
    let (snapshot, _updates) = observation.subscribe();
    assert_eq!(snapshot.running_finite_work, 1);
    assert!(!snapshot.quiescent);
    let registration = fixture
        .base
        .session
        .services
        .code_mode_service
        .work_registration(&cell_id())
        .unwrap();
    assert_eq!(registration.kind(), crate::agent::control::WorkKind::Cell);
    drop(registration);
    fixture.delegate.cell_closed(&cell_id());
    fixture
        .base
        .session
        .services
        .code_mode_service
        .mark_cell_ready_for_dispatch(&cell_id(), None);
    assert!(
        fixture
            .base
            .session
            .services
            .code_mode_service
            .active_cell_ids()
            .is_empty()
    );
    assert!(!fixture.live());
    assert!(observation.snapshot().quiescent);
    assert!(
        fixture
            .base
            .session
            .services
            .code_mode_service
            .work_registration(&cell_id())
            .is_none()
    );
}

#[tokio::test]
async fn cell_native_close_and_repeated_readiness_keep_one_collector_and_retained_record() {
    for error in [None, Some("script failed".to_owned())] {
        let fixture = NativeFixture::yielded(
            crate::agent::control::WorkKind::Cell,
            Ok(RuntimeResponse::Result {
                cell_id: cell_id(),
                content_items: vec![text("retained-tail")],
                error_text: error.clone(),
                result_buffered_for: Duration::ZERO,
                code_mode_host_duration: None,
            }),
            TerminalReadySupport::Supported,
        )
        .await;
        fixture.base.completion.ready(ready(CellOutputKind::Text));
        let published = fixture.base.published().await;
        assert_eq!(published.len(), 1);
        assert!(published[0].contains(if error.is_some() {
            "status=error"
        } else {
            "status=completed"
        }));
        assert!(fixture.delegate.work_registration.lock().unwrap().is_none());
        fixture.delegate.cell_closed(&cell_id());
        fixture.base.completion.ready(ready(CellOutputKind::Text));
        assert_eq!(
            fixture
                .provider
                .waits
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            fixture.live(),
            "the automatic record retains the registration after native close"
        );
        fixture.base.session.services.async_completions.retire();
        assert!(!fixture.live());
    }
}

#[tokio::test]
async fn service_cell_diagnostic_preserves_cell_identity_without_finite_weight() {
    let fixture = NativeFixture::yielded(
        crate::agent::control::WorkKind::Service,
        Ok(RuntimeResponse::Result {
            cell_id: cell_id(),
            content_items: vec![text("service-tail")],
            error_text: None,
            result_buffered_for: Duration::ZERO,
            code_mode_host_duration: None,
        }),
        TerminalReadySupport::Supported,
    )
    .await;
    let observation = fixture
        .base
        .session
        .services
        .local_agent_runtime
        .work_observation();
    let (running, _updates) = observation.subscribe();
    assert_eq!(running.running_finite_work, 0);
    assert!(running.quiescent);
    assert!(fixture.live(), "services remain session-owned");
    fixture.base.completion.ready(ready(CellOutputKind::Text));
    let published = fixture.base.published().await;
    assert_eq!(published.len(), 1);
    assert!(published[0].contains(&format!("kind=cell cell={CELL}")));
    assert!(published[0].contains("work_kind=service"));
    assert!(published[0].contains("call=cell-call"));
    assert!(observation.snapshot().quiescent);
    assert!(
        fixture.live(),
        "service diagnostics retain their exact native registration"
    );
    fixture.base.session.services.async_completions.retire();
    assert!(!fixture.live());
}

#[tokio::test]
async fn cell_cancel_and_collection_failure_keep_one_terminal_record_and_native_hold() {
    for cancelled in [true, false] {
        let fixture = NativeFixture::yielded(
            crate::agent::control::WorkKind::Cell,
            Err("terminal read failed".to_owned()),
            TerminalReadySupport::Supported,
        )
        .await;
        if cancelled {
            fixture.base.completion.begin_interrupt();
            let result = fixture.provider.terminate(cell_id()).await;
            fixture.base.completion.interrupted(&result);
            fixture.base.completion.interrupted(&result);
        } else {
            fixture.base.completion.ready(ready(CellOutputKind::Text));
        }
        let published = fixture.base.published().await;
        assert_eq!(published.len(), 1);
        assert!(published[0].contains(if cancelled {
            "status=cancelled"
        } else {
            "status=collectionFailed"
        }));
        assert!(fixture.live());
        assert!(fixture.delegate.work_registration.lock().unwrap().is_none());
        assert_eq!(
            fixture
                .provider
                .terminations
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        fixture.retire_ready_record();
        assert!(!fixture.live());
    }
}

#[tokio::test]
async fn cell_collector_missing_after_native_close_keeps_collection_failed_diagnostic() {
    let fixture = NativeFixture::yielded(
        crate::agent::control::WorkKind::Cell,
        Ok(RuntimeResponse::Terminated {
            cell_id: cell_id(),
            content_items: vec![],
            code_mode_host_duration: None,
        }),
        TerminalReadySupport::Supported,
    )
    .await;
    fixture
        .provider
        .missing_after_close
        .store(true, std::sync::atomic::Ordering::SeqCst);
    fixture.base.completion.ready(ready(CellOutputKind::Text));
    let published = fixture.base.published().await;
    assert_eq!(published.len(), 1);
    assert!(published[0].contains("status=collectionFailed"));
    assert!(published[0].ends_with("\nterminal output could not be collected"));
    assert_eq!(
        fixture
            .provider
            .terminations
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an already confirmed native close needs no termination retry"
    );
    fixture.retire_ready_record();
    assert!(!fixture.live());
}

#[tokio::test]
async fn cell_collection_failure_closes_provider_before_acceptance_in_collector_and_watcher() {
    for readiness in [
        TerminalReadySupport::Supported,
        TerminalReadySupport::Unsupported,
    ] {
        let fixture = NativeFixture::with_options(
            crate::agent::control::WorkKind::Cell,
            Err("provider wait failed without closing the native cell".to_owned()),
            readiness,
            true,
            None,
            false,
        )
        .await;
        fixture.base.completion.ready(ready(CellOutputKind::Text));
        let published = fixture.base.published().await;
        assert_eq!(published.len(), 1);
        assert!(published[0].contains("status=collectionFailed"));
        assert_eq!(
            fixture
                .provider
                .waits
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            fixture
                .provider
                .terminations
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(fixture.base.completion.is_closed());
        assert!(fixture.delegate.work_registration.lock().unwrap().is_none());
        assert!(
            fixture
                .base
                .session
                .services
                .code_mode_service
                .active_cell_ids()
                .is_empty()
        );
        assert!(
            fixture.live(),
            "the existing terminal record owns the only hold"
        );
        fixture.retire_ready_record();
        assert!(
            !fixture.live(),
            "removing the existing result must settle finite work without another turn"
        );
        assert!(
            fixture
                .base
                .session
                .services
                .local_agent_runtime
                .work_observation()
                .snapshot()
                .quiescent
        );
    }
}

#[tokio::test]
async fn cell_collection_cleanup_failure_keeps_active_gate_and_fenced_record_for_interrupt() {
    let fixture = NativeFixture::with_options(
        crate::agent::control::WorkKind::Cell,
        Err("provider wait failed".to_owned()),
        TerminalReadySupport::Supported,
        false,
        None,
        true,
    )
    .await;
    fixture.base.completion.ready(ready(CellOutputKind::Text));
    let published = fixture.base.published().await;
    assert_eq!(published.len(), 1);
    assert!(published[0].contains("status=collectionFailed"));
    assert!(
        !fixture
            .base
            .store()
            .has_ready(fixture.base.session.thread_id(), None)
    );
    assert_eq!(
        fixture
            .base
            .session
            .services
            .code_mode_service
            .active_cell_ids(),
        vec![cell_id()]
    );
    assert!(fixture.delegate.work_registration.lock().unwrap().is_some());
    assert!(fixture.live());
    fixture
        .provider
        .fail_termination
        .store(false, std::sync::atomic::Ordering::SeqCst);
    fixture
        .base
        .session
        .services
        .code_mode_service
        .interrupt_active_cells()
        .await;
    assert_eq!(
        fixture
            .provider
            .terminations
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(fixture.base.store().published_texts(), published);
    fixture.retire_ready_record();
    assert!(!fixture.live());
}

#[tokio::test]
async fn cell_terminal_record_waits_for_delayed_native_close_in_collector_watcher_and_interrupt() {
    // Each provider returns a terminal result before delivering its close callback.
    for path in ["collector", "watcher", "interrupt"] {
        let gate = CancellationToken::new();
        let fixture = NativeFixture::with_options(
            crate::agent::control::WorkKind::Cell,
            Ok(RuntimeResponse::Result {
                cell_id: cell_id(),
                content_items: vec![text("terminal before native close")],
                error_text: None,
                result_buffered_for: Duration::ZERO,
                code_mode_host_duration: None,
            }),
            if path == "watcher" {
                TerminalReadySupport::Unsupported
            } else {
                TerminalReadySupport::Supported
            },
            true,
            Some(gate.clone()),
            false,
        )
        .await;
        let pending = fixture.base.store().pending();
        assert_eq!(pending.len(), 1, "yielded cell is visible: {path}");
        assert_eq!(pending[0].cell_id.as_deref(), Some(CELL));
        assert!(!pending[0].ready);
        if path == "interrupt" {
            fixture
                .base
                .session
                .services
                .code_mode_service
                .interrupt_active_cells()
                .await;
        } else {
            fixture.base.completion.ready(ready(CellOutputKind::Text));
        }
        let published = fixture.base.published().await;
        assert_eq!(published.len(), 1);
        assert_eq!(fixture.base.store().pending(), pending);
        assert!(fixture.delegate.work_registration.lock().unwrap().is_some());
        assert_eq!(
            fixture
                .base
                .session
                .services
                .code_mode_service
                .active_cell_ids(),
            vec![cell_id()]
        );
        assert!(
            !fixture
                .base
                .store()
                .has_ready(fixture.base.session.thread_id(), None)
        );
        let permit = fixture.base.completion.observe().await;
        fixture
            .base
            .completion
            .observed(&Ok(WaitOutcome::MissingCell(RuntimeResponse::Terminated {
                cell_id: cell_id(),
                content_items: vec![],
                code_mode_host_duration: None,
            })));
        drop(permit);
        fixture.base.completion.ready(ready(CellOutputKind::Media));
        assert_eq!(fixture.base.store().published_texts(), published);
        assert!(fixture.base.store().has_owned_work());
        gate.cancel();
        tokio::time::timeout(
            Duration::from_secs(10),
            fixture.base.completion.native_closed.cancelled(),
        )
        .await
        .expect("provider native close callback");
        let ready = fixture.base.store().pending();
        assert_eq!(ready.len(), 1);
        assert!(ready[0].ready);
        assert!(fixture.delegate.work_registration.lock().unwrap().is_none());
        assert!(
            fixture
                .base
                .session
                .services
                .code_mode_service
                .active_cell_ids()
                .is_empty()
        );
        fixture.retire_ready_record();
        assert!(
            !fixture.live(),
            "the existing result owns the final hold: {path}"
        );
        assert!(
            fixture
                .base
                .session
                .services
                .local_agent_runtime
                .work_observation()
                .snapshot()
                .quiescent
        );
        assert!(
            !fixture
                .base
                .store()
                .has_ready(fixture.base.session.thread_id(), None)
        );
    }
}

#[tokio::test]
async fn cell_model_terminal_read_delayed_close_settles_exact_assignment_without_payload() {
    use crate::agent::control::AssignmentPhase;
    use crate::agent::control::TurnEndDisposition;
    use crate::agent::control::WorkKind;

    for kind in [WorkKind::Cell, WorkKind::Service] {
        for terminate in [true, false] {
            tokio::time::timeout(Duration::from_secs(10), async {
                let gate = CancellationToken::new();
                let fixture = NativeFixture::with_options_and_assignment(
                    kind,
                    Ok(RuntimeResponse::Result {
                        cell_id: cell_id(),
                        content_items: vec![text("manually consumed terminal output")],
                        error_text: None,
                        result_buffered_for: Duration::ZERO,
                        code_mode_host_duration: None,
                    }),
                    TerminalReadySupport::Supported,
                    false,
                    Some(gate.clone()),
                    false,
                    true,
                )
                .await;
                let session = &fixture.base.session;
                let service = &session.services.code_mode_service;
                let runtime = &session.services.local_agent_runtime;
                let turn = &fixture.base.completion.turn;
                let assignment = turn.agent_assignment.get().unwrap();
                let result = if terminate {
                    service
                        .terminate(cell_id())
                        .await
                        .map(super::super::CellWaitOutcome::Runtime)
                } else {
                    service
                        .wait(
                            WaitRequest {
                                cell_id: cell_id(),
                                yield_time_ms: 1,
                            },
                            None,
                            fixture.base.store(),
                        )
                        .await
                };
                assert!(matches!(
                    result,
                    Ok(super::super::CellWaitOutcome::Runtime(
                        WaitOutcome::LiveCell(
                            RuntimeResponse::Result { .. } | RuntimeResponse::Terminated { .. }
                        )
                    ))
                ));
                assert!(!fixture.base.store().has_owned_work());
                assert!(fixture.base.store().published_texts().is_empty());
                assert_eq!(service.active_cell_ids(), vec![cell_id()]);
                assert!(fixture.delegate.work_registration.lock().unwrap().is_some());
                assert!(fixture.live());
                // The parent and child assignments are open, plus exactly one finite Cell hold.
                assert_eq!(
                    runtime.work_observation().snapshot().running_finite_work,
                    2 + usize::from(kind.is_finite())
                );
                assert!(!runtime.has_ready_owned_completions(assignment));
                assert_eq!(
                    runtime
                        .classify_wake_turn_end(
                            assignment,
                            &turn.sub_id,
                            TurnEndDisposition::Succeeded,
                        )
                        .await,
                    Ok(if kind.is_finite() {
                        AssignmentPhase::Waiting
                    } else {
                        AssignmentPhase::Completed
                    })
                );
                assert!(!runtime.has_ready_owned_completions(assignment));

                // Only the provider delivers native close, after the model's final boundary.
                gate.cancel();
                fixture.base.completion.native_closed.cancelled().await;
                assert!(fixture.delegate.work_registration.lock().unwrap().is_none());
                assert!(service.active_cell_ids().is_empty());
                assert!(!fixture.live());
                assert!(!fixture.base.store().has_owned_work());
                assert!(fixture.base.store().published_texts().is_empty());
                assert_eq!(
                    runtime.has_ready_owned_completions(assignment),
                    kind.is_finite()
                );
                if kind.is_finite() {
                    runtime
                        .continue_owned_completion(assignment, "manual-close")
                        .unwrap();
                    runtime.acknowledge_finite_terminal_hint(assignment, "manual-close");
                    assert_eq!(
                        runtime
                            .classify_wake_turn_end(
                                assignment,
                                "manual-close",
                                TurnEndDisposition::Succeeded,
                            )
                            .await,
                        Ok(AssignmentPhase::Completed)
                    );
                }
                assert!(!runtime.has_ready_owned_completions(assignment));
                assert!(
                    runtime
                        .continue_owned_completion(assignment, "extra")
                        .is_err()
                );
            })
            .await
            .expect("manual terminal read and provider-controlled native close");
        }
    }
}

#[tokio::test]
async fn cell_model_missing_read_preserves_published_fence_without_close_hint() {
    use crate::agent::control::AssignmentPhase;
    use crate::agent::control::TurnEndDisposition;

    tokio::time::timeout(Duration::from_secs(10), async {
        let gate = CancellationToken::new();
        let fixture = NativeFixture::with_options_and_assignment(
            crate::agent::control::WorkKind::Cell,
            Ok(RuntimeResponse::Result {
                cell_id: cell_id(),
                content_items: vec![text("collector-owned terminal output")],
                error_text: None,
                result_buffered_for: Duration::ZERO,
                code_mode_host_duration: None,
            }),
            TerminalReadySupport::Supported,
            false,
            Some(gate.clone()),
            false,
            true,
        )
        .await;
        let session = &fixture.base.session;
        let runtime = &session.services.local_agent_runtime;
        let turn = &fixture.base.completion.turn;
        let assignment = turn.agent_assignment.get().unwrap();
        fixture.base.completion.ready(ready(CellOutputKind::Text));
        let published = fixture.base.published().await;
        let pending = fixture.base.store().pending();
        assert_eq!(published.len(), 1);
        assert_eq!(pending.len(), 1);
        assert!(!pending[0].ready);
        fixture
            .provider
            .missing_after_close
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let result = session
            .services
            .code_mode_service
            .wait(
                WaitRequest {
                    cell_id: cell_id(),
                    yield_time_ms: 1,
                },
                None,
                fixture.base.store(),
            )
            .await;
        assert!(matches!(
            result,
            Ok(super::super::CellWaitOutcome::AlreadyCollected(_))
        ));
        assert_eq!(fixture.base.store().published_texts(), published);
        assert_eq!(fixture.base.store().pending(), pending);
        assert_eq!(
            runtime
                .classify_wake_turn_end(assignment, &turn.sub_id, TurnEndDisposition::Succeeded)
                .await,
            Ok(AssignmentPhase::Waiting)
        );
        assert!(!runtime.has_ready_owned_completions(assignment));
        gate.cancel();
        fixture.base.completion.native_closed.cancelled().await;
        assert_eq!(fixture.base.store().published_texts(), published);
        assert!(fixture.base.store().pending()[0].ready);
        assert!(runtime.has_ready_owned_completions(assignment));
        fixture.retire_ready_record();
        assert!(fixture.base.store().published_texts().is_empty());
        assert!(!runtime.has_ready_owned_completions(assignment));
        assert!(
            runtime
                .continue_owned_completion(assignment, "unwanted-close-hint")
                .is_err()
        );
    })
    .await
    .expect("published record remains fenced until provider native close");
}

#[tokio::test]
async fn cell_model_wait_error_preserves_automatic_collection_and_native_gate() {
    let fixture = NativeFixture::yielded(
        crate::agent::control::WorkKind::Cell,
        Err("provider wait failed".to_owned()),
        TerminalReadySupport::Supported,
    )
    .await;
    let result = fixture
        .base
        .session
        .services
        .code_mode_service
        .wait(
            WaitRequest {
                cell_id: cell_id(),
                yield_time_ms: 1,
            },
            None,
            fixture.base.store(),
        )
        .await;
    assert!(result.is_err());
    assert!(fixture.base.store().has_owned_work());
    assert_eq!(
        fixture
            .base
            .session
            .services
            .code_mode_service
            .active_cell_ids(),
        vec![cell_id()]
    );
    fixture.base.completion.ready(ready(CellOutputKind::Text));
    assert_eq!(fixture.base.published().await.len(), 1);
    fixture.retire_ready_record();
    assert!(!fixture.live());
}

#[tokio::test(start_paused = true)]
async fn cell_collection_close_timeout_keeps_native_cleanup_authority_until_delayed_callback() {
    let gate = CancellationToken::new();
    let fixture = NativeFixture::with_options(
        crate::agent::control::WorkKind::Cell,
        Err("provider wait failed".to_owned()),
        TerminalReadySupport::Supported,
        false,
        Some(gate.clone()),
        false,
    )
    .await;
    // Drive the real collector directly so the paused clock can advance while it awaits close.
    fixture.base.completion.collect().await;
    let published = fixture.base.store().published_texts();
    assert_eq!(published.len(), 1);
    assert!(published[0].contains("status=collectionFailed"));
    assert!(
        !fixture
            .base
            .store()
            .has_ready(fixture.base.session.thread_id(), None)
    );
    assert_eq!(
        fixture
            .base
            .session
            .services
            .code_mode_service
            .active_cell_ids(),
        vec![cell_id()]
    );
    assert!(fixture.delegate.work_registration.lock().unwrap().is_some());
    gate.cancel();
    fixture.base.completion.native_closed.cancelled().await;
    assert_eq!(fixture.base.store().published_texts(), published);
    fixture.retire_ready_record();
    assert!(!fixture.live());
}

async fn model_wait(
    fixture: &NativeFixture,
    id: &str,
    terminate: bool,
) -> Box<dyn crate::tools::context::ToolOutput> {
    model_wait_or_terminate(fixture, id, terminate, false)
        .await
        .expect("wait handler returns a tool output")
}

async fn model_wait_or_terminate(
    fixture: &NativeFixture,
    id: &str,
    terminate: bool,
    terminate_only: bool,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, crate::function_tool::FunctionCallError> {
    use crate::session::step_context::StepContext;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::registry::ToolExecutor;

    let turn = Arc::clone(&fixture.base.completion.turn);
    let handler = if terminate_only {
        super::super::CodeModeWaitHandler::terminate_only()
    } else {
        super::super::CodeModeWaitHandler::new(None, None)
    };
    handler
        .handle(ToolInvocation {
            session: Arc::clone(&fixture.base.session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
            call_id: "late-wait".to_owned(),
            tool_name: codex_tools::ToolName::plain(if terminate_only {
                super::super::TERMINATE_TOOL_NAME
            } else {
                super::super::WAIT_TOOL_NAME
            }),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: if terminate_only {
                    serde_json::json!({"cell_id": id})
                } else {
                    serde_json::json!({"cell_id": id, "yield_time_ms": 1, "terminate": terminate})
                }
                .to_string(),
            },
            environments: Default::default(),
        })
        .await
}

#[test_case::test_case(false, false; "native_closed")]
#[test_case::test_case(true, false; "native_close_pending")]
#[test_case::test_case(false, true; "terminate_native_closed")]
#[test_case::test_case(true, true; "terminate_native_close_pending")]
#[tokio::test]
async fn cell_completion_late_model_wait_returns_the_one_automatic_message(
    close_pending: bool,
    terminate: bool,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let gate = close_pending.then(CancellationToken::new);
        let fixture = NativeFixture::with_options(
            crate::agent::control::WorkKind::Cell,
            Ok(RuntimeResponse::Result {
                cell_id: cell_id(),
                content_items: vec![text("automatic-only terminal output")],
                error_text: None,
                result_buffered_for: Duration::ZERO,
                code_mode_host_duration: None,
            }),
            TerminalReadySupport::Supported,
            false,
            gate.clone(),
            false,
        )
        .await;
        fixture.base.completion.ready(ready(CellOutputKind::Text));
        fixture.base.published().await;
        fixture
            .provider
            .fail_termination
            .store(true, std::sync::atomic::Ordering::SeqCst);
        fixture
            .provider
            .missing_after_close
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let pending = fixture.base.store().pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].ready, !close_pending);
        let message_id = &pending[0].id;
        let published = fixture.base.store().published_texts();
        assert_eq!(published.len(), 1);
        for _ in 0..2 {
            let output = model_wait(&fixture, CELL, terminate).await;
            assert!(output.success_for_logging());
            let text = output.log_output();
            assert!(text.contains("already collected for automatic completion"));
            assert!(text.contains(message_id));
            assert!(text.contains("Delivery may still be pending"));
            assert!(!text.contains("was delivered"));
            assert!(!text.contains("automatic-only terminal output"));
            assert_eq!(fixture.base.store().published_texts(), published);
            assert_eq!(fixture.base.store().pending(), pending);
            assert_eq!(
                fixture
                    .provider
                    .waits
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
        }
        if let Some(gate) = gate {
            assert!(
                !fixture
                    .base
                    .store()
                    .has_ready(fixture.base.session.thread_id(), None)
            );
            assert!(fixture.live());
            gate.cancel();
            fixture.base.completion.native_closed.cancelled().await;
        }
        assert_eq!(fixture.base.store().published_texts(), published);
        assert!(fixture.base.store().pending()[0].ready);
        fixture
            .base
            .store()
            .accept(&[codex_protocol::ResponseItemId::from_server(
                message_id.clone(),
            )]);
        assert!(fixture.base.store().pending().is_empty());
        let output = model_wait(&fixture, CELL, terminate).await;
        assert!(output.success_for_logging());
        assert!(output.log_output().contains(message_id));
        assert!(fixture.base.store().pending().is_empty());
        assert_eq!(
            fixture
                .provider
                .terminations
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(!fixture.live());
    })
    .await
    .expect("late wait preserves automatic result and native close fence");
}

#[tokio::test]
async fn cell_completion_terminate_only_preserves_the_already_finished_response() {
    let fixture = NativeFixture::yielded(
        crate::agent::control::WorkKind::Cell,
        Ok(RuntimeResponse::Terminated {
            cell_id: cell_id(),
            content_items: vec![],
            code_mode_host_duration: None,
        }),
        TerminalReadySupport::Supported,
    )
    .await;
    fixture.base.completion.ready(ready(CellOutputKind::Text));
    let published = fixture.base.published().await;
    fixture
        .provider
        .missing_after_close
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let output = model_wait_or_terminate(&fixture, CELL, true, true).await;
    assert!(
        matches!(output, Err(crate::function_tool::FunctionCallError::RespondToModel(text))
        if text == format!("cell {CELL} already finished; its result was delivered automatically"))
    );
    assert_eq!(fixture.base.store().published_texts(), published);
    fixture.retire_ready_record();
}

#[tokio::test]
async fn cell_completion_unknown_model_wait_keeps_the_missing_cell_error() {
    let fixture = NativeFixture::yielded(
        crate::agent::control::WorkKind::Cell,
        Ok(RuntimeResponse::Result {
            cell_id: cell_id(),
            content_items: vec![text("collected known cell")],
            error_text: None,
            result_buffered_for: Duration::ZERO,
            code_mode_host_duration: None,
        }),
        TerminalReadySupport::Supported,
    )
    .await;
    fixture.base.completion.ready(ready(CellOutputKind::Text));
    let published = fixture.base.published().await;
    let output = model_wait(&fixture, "unknown-generation:cell", false).await;
    assert!(!output.success_for_logging());
    assert!(output.log_output().contains("cell not found"));
    assert!(!output.log_output().contains("already collected"));
    assert_eq!(fixture.base.store().published_texts(), published);
    fixture.retire_ready_record();
}

#[test_case::test_case(false; "wait")]
#[test_case::test_case(true; "terminating_wait")]
#[tokio::test]
async fn cell_completion_model_wait_queued_behind_collector_does_not_read_twice(terminate: bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = NativeFixture::yielded(
            crate::agent::control::WorkKind::Cell,
            Ok(RuntimeResponse::Result {
                cell_id: cell_id(),
                content_items: vec![text("terminal")],
                error_text: None,
                result_buffered_for: Duration::ZERO,
                code_mode_host_duration: None,
            }),
            TerminalReadySupport::Supported,
        )
        .await;
        let permit = fixture.base.completion.observe().await;
        let mut collecting = std::pin::pin!(async {
            fixture.base.completion.collect().await;
            fixture
                .provider
                .missing_after_close
                .store(true, std::sync::atomic::Ordering::SeqCst);
        });
        // Queue collector first, then the model on the FIFO observation lock.
        assert!(futures::poll!(collecting.as_mut()).is_pending());
        let mut waiting = std::pin::pin!(model_wait(&fixture, CELL, terminate));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        drop(permit);
        let ((), output) = tokio::join!(collecting, waiting);
        assert!(output.success_for_logging());
        let pending = fixture.base.store().pending();
        assert_eq!(pending.len(), 1);
        assert!(output.log_output().contains(&pending[0].id));
        assert_eq!(
            fixture
                .provider
                .waits
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(fixture.base.store().published_texts().len(), 1);
        fixture.retire_ready_record();
    })
    .await
    .expect("queued wait observes the collector's authoritative completion identity");
}
