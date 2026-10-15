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
