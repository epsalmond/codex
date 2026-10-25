use super::*;
use crate::session::tests::make_session_and_context;
use codex_code_mode::CellTerminalStatus;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionResultFuture;
use codex_code_mode::ExecuteRequest;
use pretty_assertions::assert_eq;

const CELL: &str = "generation:cell-1";

/// Returns one fixed terminal result to the collector's read.
struct FixedSession(RuntimeResponse);

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
        let response = self.0.clone();
        Box::pin(async move { Ok(WaitOutcome::LiveCell(response)) })
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
        let provider: Arc<dyn CodeModeSession> = Arc::new(FixedSession(terminal));
        completion.admit(&started_cell(), &provider);
        completion.initial(&yielded());
        Self {
            session,
            completion,
            _provider: provider,
        }
    }

    fn store(&self) -> &crate::session::async_completion::AsyncCompletions {
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
    let provider: Arc<dyn CodeModeSession> = Arc::new(FixedSession(yielded()));
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
    let provider: Arc<dyn CodeModeSession> = Arc::new(FixedSession(terminal));
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
            if self.terminal.is_ok() {
                self.delegate.cell_closed(&request.cell_id);
            }
            self.terminal.clone().map(WaitOutcome::LiveCell)
        })
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            self.delegate.cell_closed(&cell_id);
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
        use crate::session::step_context::StepContext;
        use crate::tools::code_mode::delegate::CodeModeCellDelegate;
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let registration = session
            .services
            .local_agent_runtime
            .reserve_owned_work(&session, &turn, "cell-call", kind, None)
            .unwrap();
        registration.mark_cell();
        registration.mark_started();
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
            false,
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
        });
        let dynamic_provider: Arc<dyn CodeModeSession> = provider.clone();
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
        if !cancelled {
            assert!(fixture.delegate.work_registration.lock().unwrap().is_some());
            fixture.delegate.cell_closed(&cell_id());
        }
        fixture.base.session.services.async_completions.retire();
        assert!(!fixture.live());
    }
}
