//! Automatic delivery of a yielded Code Mode cell's terminal result.
//!
//! One reservation per `exec`. Readiness from the provider elects a single collector that reads
//! the terminal result under the cell's observation lock and publishes a bounded text tail.
//! `wait` and `terminate` share that lock, so whichever reads the final result first owns it: a
//! model read releases the reservation and no automatic message follows.
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CellOutputKind;
use codex_code_mode::CellTerminalReady;
use codex_code_mode::CodeModeSession;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::StartedCell;
use codex_code_mode::TerminalReadySupport;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use tokio_util::sync::CancellationToken;

use crate::agent::control::OwnedWorkRegistration;
use crate::session::async_completion::CompletionPublisher;
use crate::session::async_completion::CompletionReservation;
use crate::session::async_completion::CompletionStatus;
use crate::session::async_completion::MAX_FRAGMENT_BYTES;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

/// Bounds the single terminal read after readiness; the result is already buffered.
const COLLECTION_TIMEOUT: Duration = Duration::from_secs(30);
const COLLECTION_YIELD_TIME_MS: u64 = 1_000;

pub(super) struct CellCompletion {
    state: Mutex<State>,
    observation: Arc<tokio::sync::Mutex<()>>,
    owner: Weak<Session>,
    turn: Arc<TurnContext>,
    shutdown: CancellationToken,
    /// True when the model has no `wait` tool to read the result itself. Media results are then
    /// published as their text parts plus an omission note instead of being left for `wait`, and
    /// a cell whose provider cannot report readiness is watched instead of released.
    wait_hidden: bool,
    /// Stops the fallback watcher; `exec_terminate` triggers it before taking the observation lock.
    watch_cancel: CancellationToken,
    /// Native close has released the delegate, dispatch and event holds.
    native_closed: CancellationToken,
}

struct State {
    lifecycle: Option<OwnedWorkRegistration>,
    reservation: Option<CompletionReservation>,
    publisher: Option<CompletionPublisher>,
    binding: Option<(CellId, Weak<dyn CodeModeSession>)>,
    ready: Option<CellTerminalReady>,
    yielded: bool,
    elected: bool,
    collecting: bool,
    /// An interrupt's terminate is in flight; its own close must not release the reservation.
    interrupting: bool,
    closed: bool,
    /// The provider could not report readiness, so a core watcher polls the cell instead.
    watched: bool,
    /// The existing store has the terminal payload, still fenced by our reservation.
    published: bool,
}

impl State {
    /// True while a collector or an interrupt owns the terminal result.
    fn settling(&self) -> bool {
        self.collecting || self.interrupting
    }

    fn release(&mut self) {
        self.publisher = None;
        if let Some(reservation) = self.reservation.take() {
            reservation.release();
        }
        self.lifecycle = None;
        self.published = false;
    }

    /// Publishes once and hands the record to the store for delivery.
    fn publish(&mut self, status: CompletionStatus, tail: &[u8], omitted: usize) {
        if let Some(publisher) = self.publisher.take() {
            publisher.publish(status, tail, omitted);
        }
        self.published = true;
        // The store owns the remaining registration before ordinary acceptance can run.
        self.lifecycle = None;
        if self.closed {
            drop(self.reservation.take());
        }
    }
}

/// Releases the reservation if the `exec` call ends (including by cancellation) before the cell
/// yields; a yielded cell keeps it for automatic delivery.
pub(super) struct InitialCompletion(pub(super) Arc<CellCompletion>);

impl Drop for InitialCompletion {
    fn drop(&mut self) {
        let mut state = self.0.state();
        if !state.yielded {
            state.release();
        }
    }
}

impl CellCompletion {
    pub(super) fn new(
        reservation: CompletionReservation,
        lifecycle: Option<OwnedWorkRegistration>,
        owner: &Arc<Session>,
        turn: Arc<TurnContext>,
        shutdown: CancellationToken,
        wait_hidden: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                lifecycle,
                reservation: Some(reservation),
                publisher: None,
                binding: None,
                ready: None,
                yielded: false,
                elected: false,
                collecting: false,
                interrupting: false,
                closed: false,
                watched: false,
                published: false,
            }),
            observation: Arc::new(tokio::sync::Mutex::new(())),
            owner: Arc::downgrade(owner),
            turn,
            shutdown,
            wait_hidden,
            watch_cancel: CancellationToken::new(),
            native_closed: CancellationToken::new(),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Serializes terminal reads between the collector and model `wait`/`terminate` calls.
    pub(super) async fn observe(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.observation).lock_owned().await
    }

    /// Binds the provider's cell ID. When the provider cannot report readiness, the result is left
    /// to `wait`, or, where `wait` is hidden, to a core watcher started once the cell yields.
    pub(super) fn admit(&self, started: &StartedCell, provider: &Arc<dyn CodeModeSession>) {
        let mut state = self.state();
        let supported = started.terminal_ready_support == TerminalReadySupport::Supported;
        let bound = (supported || self.wait_hidden)
            && !state.closed
            && state
                .ready
                .as_ref()
                .is_none_or(|ready| ready.cell_id == started.cell_id)
            && state
                .reservation
                .as_mut()
                .is_some_and(|reservation| reservation.bind_cell(started.cell_id.as_str()).is_ok());
        if bound {
            state.binding = Some((started.cell_id.clone(), Arc::downgrade(provider)));
            state.watched = !supported;
        } else {
            // A provider without readiness leaves the result to `wait`.
            state.release();
        }
    }

    /// Registers a publisher for a yielded cell; an inline result needs no delivery.
    pub(super) fn initial(self: &Arc<Self>, response: &RuntimeResponse) -> bool {
        {
            let mut state = self.state();
            let yielded = matches!(
                response,
                RuntimeResponse::Yielded { cell_id, .. }
                    if state.binding.as_ref().is_some_and(|(id, _)| id == cell_id)
            );
            let publisher = state
                .reservation
                .as_mut()
                .filter(|_| yielded)
                .map(|reservation| {
                    let publisher = reservation.register();
                    reservation.mark_cell_yielded_for_visibility();
                    publisher
                });
            if publisher.is_some() {
                state.publisher = publisher;
                state.yielded = true;
                if let Some(lifecycle) = &state.lifecycle {
                    lifecycle.mark_yielded(true);
                }
            } else {
                state.release();
            }
            if state.watched && state.publisher.is_some() && !state.closed {
                // The watcher owns the terminal read for the cell's whole life.
                state.elected = true;
                state.collecting = true;
                let completion = Arc::clone(self);
                tokio::spawn(async move {
                    completion.watch().await;
                });
                return true;
            }
        }
        self.collect_if_ready();
        self.state().yielded
    }

    /// Stops the fallback watcher before a model terminate takes the observation lock.
    pub(super) fn cancel_watcher(&self) {
        self.watch_cancel.cancel();
    }

    /// Fallback for a cell whose provider cannot report readiness while `wait` is hidden: poll the
    /// cell under the observation lock and publish its final result with a bounded text tail.
    async fn watch(self: Arc<Self>) {
        let mut tail = Vec::new();
        let mut omitted = 0usize;
        loop {
            let permit = tokio::select! {
                _ = self.shutdown.cancelled() => {
                    self.stop_watching(/*release*/ true);
                    return;
                }
                _ = self.watch_cancel.cancelled() => {
                    // `exec_terminate` reads the result inline and releases the reservation.
                    self.stop_watching(/*release*/ false);
                    return;
                }
                permit = self.observe() => permit,
            };
            let binding = {
                let state = self.state();
                if state.reservation.is_none() || state.publisher.is_none() {
                    None
                } else {
                    state.binding.clone()
                }
            };
            let Some((cell_id, provider)) = binding else {
                self.stop_watching(/*release*/ false);
                return;
            };
            let result = match provider.upgrade() {
                Some(provider) => tokio::select! {
                    _ = self.shutdown.cancelled() => {
                        self.stop_watching(/*release*/ true);
                        return;
                    }
                    result = provider.wait(
                        WaitRequest {
                            cell_id: cell_id.clone(),
                            yield_time_ms: super::DEFAULT_WAIT_YIELD_TIME_MS,
                        },
                        Some(self.watch_cancel.clone()),
                    ) => result,
                },
                None => Err("terminal provider unavailable".to_owned()),
            };
            let status = match &result {
                Ok(WaitOutcome::LiveCell(RuntimeResponse::Yielded { content_items, .. })) => {
                    let (piece, piece_omitted) = text_tail(content_items, /*error*/ None);
                    append_tail(&mut tail, &mut omitted, &piece, piece_omitted);
                    if self.watch_cancel.is_cancelled() {
                        self.stop_watching(/*release*/ false);
                        return;
                    }
                    drop(permit);
                    continue;
                }
                Ok(WaitOutcome::LiveCell(
                    response @ RuntimeResponse::Result {
                        content_items,
                        error_text,
                        ..
                    },
                )) => {
                    self.record_closed(&cell_id, response);
                    let (piece, piece_omitted) = text_tail(content_items, error_text.as_deref());
                    append_tail(&mut tail, &mut omitted, &piece, piece_omitted);
                    terminal_status(error_text.as_deref())
                }
                Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                    content_items, ..
                })) => {
                    let (piece, piece_omitted) = text_tail(content_items, /*error*/ None);
                    append_tail(&mut tail, &mut omitted, &piece, piece_omitted);
                    CompletionStatus::Cancelled
                }
                // Only an interrupt or shutdown removes a live cell the model cannot wait on.
                Ok(WaitOutcome::MissingCell(_)) => CompletionStatus::Cancelled,
                Err(_) => {
                    tail = b"terminal output could not be collected".to_vec();
                    omitted = 0;
                    CompletionStatus::CollectionFailed
                }
            };
            if matches!(status, CompletionStatus::CollectionFailed) {
                self.close_after_collection_failure(&cell_id, provider.upgrade())
                    .await;
            }
            {
                let mut state = self.state();
                state.publish(status, &tail, omitted);
                state.collecting = false;
            }
            if let Some(owner) = self.owner.upgrade() {
                owner
                    .services
                    .code_mode_service
                    .finish_cell_dispatch(&cell_id);
            }
            return;
        }
    }

    fn stop_watching(&self, release: bool) {
        let mut state = self.state();
        state.collecting = false;
        if release || state.closed {
            state.release();
        }
    }

    pub(super) fn ready(self: &Arc<Self>, ready: CellTerminalReady) {
        {
            let mut state = self.state();
            if state.published {
                return;
            }
            let media_left_for_wait =
                ready.output_kind == CellOutputKind::Media && !self.wait_hidden;
            if media_left_for_wait
                && state.yielded
                && let Some(lifecycle) = &state.lifecycle
            {
                lifecycle.mark_yielded(false);
            }
            if media_left_for_wait
                || state
                    .binding
                    .as_ref()
                    .is_some_and(|(id, _)| id != &ready.cell_id)
                || state
                    .ready
                    .as_ref()
                    .is_some_and(|previous| previous != &ready)
            {
                if !state.settling() {
                    state.release();
                }
                return;
            }
            state.ready = Some(ready);
        }
        self.collect_if_ready();
    }

    pub(super) fn closed(&self, cell_id: &CellId) {
        let mut state = self.state();
        if state.binding.as_ref().is_some_and(|(id, _)| id != cell_id) {
            return;
        }
        state.closed = true;
        if state.published {
            // Native callback cleanup is complete. Release the last local lifecycle hold before
            // the registered reservation makes the existing terminal record deliverable.
            state.lifecycle = None;
            drop(state.reservation.take());
        } else if !state.settling() {
            state.release();
        }
        self.native_closed.cancel();
    }

    pub(super) fn is_closed(&self) -> bool {
        self.state().closed
    }

    pub(super) fn collecting(&self) -> bool {
        self.state().collecting
    }

    /// Called under the observation lock after a model `wait` or `terminate`. A final result read
    /// by the model is never delivered again.
    pub(super) fn observed(self: &Arc<Self>, result: &Result<WaitOutcome, String>) {
        if matches!(
            result,
            Ok(WaitOutcome::MissingCell(_)
                | WaitOutcome::LiveCell(
                    RuntimeResponse::Result { .. } | RuntimeResponse::Terminated { .. }
                ))
        ) {
            let mut state = self.state();
            // The collector already observed the terminal result. A queued model read may only
            // see MissingCell; it must not remove that collector's still-fenced terminal record.
            if !state.published {
                state.release();
            }
        }
        self.collect_if_ready();
    }

    /// Called before an Esc interrupt or failed turn terminates the cell.
    pub(super) fn begin_interrupt(&self) {
        let mut state = self.state();
        if !state.collecting && state.publisher.is_some() {
            state.interrupting = true;
        }
    }

    /// Called after an Esc interrupt or failed turn terminated the cell. The model never reads
    /// that result, so it is published (as `cancelled` when terminated) instead of released.
    pub(super) fn interrupted(&self, result: &Result<WaitOutcome, String>) {
        let mut state = self.state();
        state.interrupting = false;
        // The collector already owns the terminal read and publishes it.
        if state.collecting || state.reservation.is_none() {
            return;
        }
        if state.publisher.is_none() {
            if !state.published {
                state.release();
            }
            return;
        }
        match result {
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated { content_items, .. })) => {
                let (tail, omitted) = text_tail(content_items, /*error*/ None);
                state.publish(CompletionStatus::Cancelled, &tail, omitted);
            }
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Result {
                content_items,
                error_text,
                ..
            })) => {
                let (tail, omitted) = text_tail(content_items, error_text.as_deref());
                state.publish(terminal_status(error_text.as_deref()), &tail, omitted);
            }
            // The cell may still be live; its readiness or close settles the reservation.
            Err(_) => {
                if state.closed {
                    state.publish(
                        CompletionStatus::CollectionFailed,
                        b"terminal output could not be collected",
                        0,
                    );
                }
            }
            Ok(WaitOutcome::MissingCell(_)) => {
                state.publish(CompletionStatus::Cancelled, &[], 0);
            }
            // An ineffective terminate leaves the existing gate and publisher recoverable.
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Yielded { .. })) => {}
        }
    }

    fn collect_if_ready(self: &Arc<Self>) {
        {
            let mut state = self.state();
            let collectable = match state.ready.as_ref() {
                Some(ready) => ready.output_kind == CellOutputKind::Text || self.wait_hidden,
                None => false,
            };
            if state.elected
                || state.published
                || state.interrupting
                || state.closed
                || !state.yielded
                || state.reservation.is_none()
                || !collectable
            {
                return;
            }
            state.elected = true;
        }
        let completion = Arc::clone(self);
        tokio::spawn(async move {
            completion.collect().await;
        });
    }

    async fn collect(&self) {
        let _permit = tokio::select! {
            _ = self.shutdown.cancelled() => {
                self.state().release();
                return;
            }
            permit = self.observe() => permit,
        };
        let (cell_id, provider) = {
            let mut state = self.state();
            if state.closed
                || state.interrupting
                || state.reservation.is_none()
                || state.publisher.is_none()
            {
                return;
            }
            let Some((cell_id, provider)) = state.binding.clone() else {
                return;
            };
            state.collecting = true;
            (cell_id, provider.upgrade())
        };
        let result = tokio::select! {
            _ = self.shutdown.cancelled() => {
                let mut state = self.state();
                state.collecting = false;
                state.release();
                return;
            }
            result = async {
                let provider = provider.as_ref().ok_or_else(|| "terminal provider unavailable".to_owned())?;
                tokio::time::timeout(
                    COLLECTION_TIMEOUT,
                    provider.wait(
                        WaitRequest {
                            cell_id: cell_id.clone(),
                            yield_time_ms: COLLECTION_YIELD_TIME_MS,
                        },
                        /*preempt*/ None,
                    ),
                )
                .await
                .map_err(|_| "terminal collection timed out".to_owned())?
            } => result,
        };
        let (status, tail, omitted) = match &result {
            Ok(WaitOutcome::LiveCell(
                response @ RuntimeResponse::Result {
                    cell_id: actual,
                    content_items,
                    error_text,
                    ..
                },
            )) if actual == &cell_id => {
                self.record_closed(&cell_id, response);
                let (tail, omitted) = text_tail(content_items, error_text.as_deref());
                (terminal_status(error_text.as_deref()), tail, omitted)
            }
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id: actual,
                content_items,
                ..
            })) if actual == &cell_id => {
                let (tail, omitted) = text_tail(content_items, None);
                (CompletionStatus::Cancelled, tail, omitted)
            }
            _ => (
                CompletionStatus::CollectionFailed,
                b"terminal output could not be collected".to_vec(),
                0,
            ),
        };
        if matches!(status, CompletionStatus::CollectionFailed) {
            self.close_after_collection_failure(&cell_id, provider)
                .await;
        }
        // Publication remains fenced until native close, even when terminate returned first.
        {
            let mut state = self.state();
            state.publish(status, &tail, omitted);
            state.collecting = false;
        }
        if let Some(owner) = self.owner.upgrade() {
            owner
                .services
                .code_mode_service
                .finish_cell_dispatch(&cell_id);
        }
    }

    /// A failed terminal read does not close a buffered native cell. Terminate that exact cell
    /// and bound confirmation; on error its gate and fenced record remain available to interrupt
    /// or session shutdown. The callback token remembers close even if it precedes this wait.
    async fn close_after_collection_failure(
        &self,
        cell_id: &CellId,
        provider: Option<Arc<dyn CodeModeSession>>,
    ) {
        let result = tokio::time::timeout(COLLECTION_TIMEOUT, async {
            if !self.is_closed() {
                let provider =
                    provider.ok_or_else(|| "terminal provider unavailable".to_owned())?;
                provider.terminate(cell_id.clone()).await?;
            }
            self.native_closed.cancelled().await;
            Ok::<(), String>(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%cell_id, %error, "failed to close code-mode cell after collection failure");
            }
            Err(_) => {
                tracing::warn!(%cell_id, "code-mode cell close confirmation timed out after collection failure");
            }
        }
    }

    fn record_closed(&self, cell_id: &CellId, response: &RuntimeResponse) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        owner
            .services
            .rollout_thread_trace
            .code_cell_trace_context(self.turn.sub_id.as_str(), cell_id.as_str())
            .record_ended(response);
        owner
            .services
            .analytics_events_client
            .track_code_mode_tool_call(codex_analytics::CodeModeToolCallFact::CellClosed {
                thread_id: owner.thread_id().to_string(),
                turn_id: self.turn.sub_id.clone(),
                cell_id: cell_id.to_string(),
            });
    }
}

/// Appends a bounded piece to the watcher's tail, keeping the newest `MAX_FRAGMENT_BYTES`.
fn append_tail(tail: &mut Vec<u8>, omitted: &mut usize, piece: &[u8], piece_omitted: usize) {
    if piece.is_empty() {
        *omitted = omitted.saturating_add(piece_omitted);
        return;
    }
    if !tail.is_empty() {
        tail.push(b'\n');
    }
    // Bytes dropped inside the piece precede its kept bytes, so earlier output is omitted too.
    if piece_omitted > 0 {
        *omitted = omitted
            .saturating_add(tail.len())
            .saturating_add(piece_omitted);
        tail.clear();
    }
    tail.extend_from_slice(piece);
    let excess = tail.len().saturating_sub(MAX_FRAGMENT_BYTES);
    tail.drain(..excess);
    *omitted = omitted.saturating_add(excess);
}

fn terminal_status(error_text: Option<&str>) -> CompletionStatus {
    if error_text.is_some() {
        CompletionStatus::CellError
    } else {
        CompletionStatus::CellCompleted
    }
}

/// Keeps the newest text bytes (text parts, then any script error), bounded before publication.
/// Media items are counted and noted last. Returns the tail and the number of bytes dropped.
fn text_tail(items: &[FunctionCallOutputContentItem], error: Option<&str>) -> (Vec<u8>, usize) {
    let media = items
        .iter()
        .filter(|item| !matches!(item, FunctionCallOutputContentItem::InputText { .. }))
        .count();
    let note = (media > 0).then(|| format!("[{media} image/audio items omitted]"));
    let mut tail = Vec::new();
    let mut total = 0usize;
    for text in items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
            FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::InputAudio { .. } => None,
        })
        .chain(error)
        .chain(note.as_deref())
    {
        if total > 0 {
            total = total.saturating_add(1);
            tail.push(b'\n');
        }
        total = total.saturating_add(text.len());
        let bytes = text.as_bytes();
        let start = bytes.len().saturating_sub(MAX_FRAGMENT_BYTES);
        let keep = MAX_FRAGMENT_BYTES.saturating_sub(bytes.len() - start);
        tail.drain(..tail.len().saturating_sub(keep));
        tail.extend_from_slice(&bytes[start..]);
    }
    let omitted = total.saturating_sub(tail.len());
    (tail, omitted)
}

#[cfg(test)]
#[path = "cell_completion_tests.rs"]
mod tests;
