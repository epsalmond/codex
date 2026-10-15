//! Bounded owner records, retained until ordinary model acceptance.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;

use crate::agent::control::AgentAssignmentId;
use crate::context::AsyncToolCompletion;
use crate::context::ContextualUserFragment;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_history::ResponseItemEnvelope;
use codex_history::UserInputOrigin;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;

const MAX_RESERVATIONS: usize = 64;
/// Bounds each rendered completion message by its encoded UTF-8 bytes, including IDs, metadata
/// and escapes. 64 reservations at this size match the 256 KiB per-owner stored payload bound.
pub(crate) const MAX_FRAGMENT_BYTES: usize = 4096;
/// Correlation identifiers are preserved exactly, never truncated.
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_NEW_REQUEST_FRAGMENTS: usize = 8;

/// Fixed at regular-task entry; steering and reentry cannot grant recovery authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionDrain {
    ExactAssignment,
    ExplicitRootRecovery,
}

impl CompletionDrain {
    pub(crate) fn for_original_input(source: &SessionSource, input: &[TurnInput]) -> Self {
        let explicit = input.iter().any(|input| match input {
            TurnInput::UserInput {
                content, metadata, ..
            } => {
                metadata.origin == UserInputOrigin::User
                    && content.iter().any(|item| match item {
                        UserInput::Text { text, .. } => !text.trim().is_empty(),
                        _ => true,
                    })
            }
            TurnInput::FunctionCallOutput(_)
            | TurnInput::ResponseItem(_)
            | TurnInput::InterAgentCommunication(_) => false,
        });
        if !source.is_non_root_agent() && explicit {
            Self::ExplicitRootRecovery
        } else {
            Self::ExactAssignment
        }
    }

    fn permits(self, record: &Record, owner: ThreadId, turn: &TurnContext) -> bool {
        record.owner == owner
            && (self == Self::ExplicitRootRecovery
                || record.assignment.as_ref() == turn.agent_assignment.get())
    }
}

#[derive(Default)]
pub(crate) struct AsyncCompletions {
    state: Mutex<CompletionState>,
    owner: OnceLock<Weak<Session>>,
    root_owner: OnceLock<bool>,
    wake: Mutex<CompletionWake>,
}

#[derive(Default)]
struct CompletionWake {
    revision: u64,
    scheduled: bool,
}

#[derive(Default)]
struct CompletionState {
    closed: bool,
    records: BTreeMap<ResponseItemId, Record>,
}

struct Record {
    owner: ThreadId,
    turn_id: String,
    create_time: serde_json::Number,
    call_id: String,
    producer: Producer,
    assignment: Option<AgentAssignmentId>,
    yielded: bool,
    claimed: bool,
    terminal: Option<AsyncToolCompletion>,
    recorded: bool,
}

enum Producer {
    /// A native process, optionally launched from inside a Code Mode cell.
    Process {
        process_id: i32,
        cell_id: Option<String>,
    },
    /// A Code Mode cell; `None` until the provider assigns its cell ID, and unbound cells never
    /// publish.
    Cell { cell_id: Option<String> },
}

/// Releases an inline result; cancellation after registration retains its record.
pub(crate) struct CompletionReservation {
    store: Arc<AsyncCompletions>,
    id: ResponseItemId,
    registered: bool,
}

#[derive(Clone)]
pub(crate) struct CompletionPublisher {
    store: Weak<AsyncCompletions>,
    id: ResponseItemId,
}

pub(crate) enum CompletionStatus {
    Exited(i32),
    Failed,
    /// Interrupted or cancelled work, for processes and cells alike.
    Cancelled,
    TimedOut,
    CellCompleted,
    CellError,
    CollectionFailed,
}

/// Returning an unrecorded claim makes it available to the next permitted turn.
pub(crate) struct CompletionClaim {
    store: Arc<AsyncCompletions>,
    ids: Vec<ResponseItemId>,
}

impl AsyncCompletions {
    fn state(&self) -> std::sync::MutexGuard<'_, CompletionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn attach_owner(&self, owner: &Arc<Session>, source: &SessionSource) {
        let _ = self.root_owner.set(!source.is_non_root_agent());
        let _ = self.owner.set(Arc::downgrade(owner));
    }

    pub(crate) fn has_owned_work(&self) -> bool {
        !self.state().records.is_empty()
    }

    /// Published fragment texts, in record order.
    #[cfg(test)]
    pub(crate) fn published_texts(&self) -> Vec<String> {
        self.state()
            .records
            .values()
            .filter_map(|record| {
                record
                    .terminal
                    .as_ref()
                    .map(|fragment| fragment.text.clone())
            })
            .collect()
    }

    pub(crate) fn owns_assignment(&self, assignment: &AgentAssignmentId) -> bool {
        self.state().records.values().any(|record| {
            record.owner == assignment.thread_id && record.assignment.as_ref() == Some(assignment)
        })
    }

    pub(crate) fn has_ready(
        &self,
        owner: ThreadId,
        assignment: Option<&AgentAssignmentId>,
    ) -> bool {
        self.state().records.values().any(|record| {
            record.owner == owner
                && record.assignment.as_ref() == assignment
                && record.yielded
                && record.terminal.is_some()
        })
    }

    // Never acquire the coordinator or start a turn while holding the record mutex.
    fn changed(self: &Arc<Self>) {
        let Some(owner) = self.owner.get().and_then(Weak::upgrade) else {
            return;
        };
        let runtime = &owner.services.local_agent_runtime;
        if let Some(assignment) = runtime.current_wake_assignment(owner.thread_id()) {
            runtime.observe_owned_completions(&assignment, self);
        } else if self.root_owner.get() == Some(&true) && self.has_ready(owner.thread_id(), None) {
            let Some(revision) = self
                .wake
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .request()
            else {
                return;
            };
            let store = Arc::downgrade(self);
            let owner = Arc::downgrade(&owner);
            tokio::spawn(async move {
                let Some(store) = store.upgrade() else {
                    return;
                };
                if let Some(owner) = owner.upgrade() {
                    owner.maybe_start_turn_for_pending_work().await;
                }
                let changed = store
                    .wake
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .finish(revision);
                // Only a new publication can request another bounded attempt. Busy, capacity
                // rejection and failure never retry themselves; finalization rechecks active work.
                if changed {
                    store.changed();
                }
            });
        }
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        call_id: &str,
        process_id: i32,
        cell_id: Option<&str>,
    ) -> Result<CompletionReservation, &'static str> {
        self.reserve_producer(
            owner,
            turn,
            call_id,
            Producer::Process {
                process_id,
                cell_id: cell_id.map(str::to_owned),
            },
        )
    }

    /// Reserves capacity for a Code Mode cell before it starts; bind the provider's cell ID with
    /// [`CompletionReservation::bind_cell`] before publishing.
    pub(crate) fn reserve_cell(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        call_id: &str,
    ) -> Result<CompletionReservation, &'static str> {
        self.reserve_producer(owner, turn, call_id, Producer::Cell { cell_id: None })
    }

    fn reserve_producer(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        call_id: &str,
        producer: Producer,
    ) -> Result<CompletionReservation, &'static str> {
        let cell_id = match &producer {
            Producer::Process { cell_id, .. } | Producer::Cell { cell_id } => cell_id.as_deref(),
        };
        if self
            .owner
            .get()
            .and_then(Weak::upgrade)
            .is_some_and(|session| session.thread_id() != owner)
            || turn
                .agent_assignment
                .get()
                .is_some_and(|assignment| assignment.thread_id != owner)
        {
            return Err("async completion owner does not match its assignment");
        }
        // Preserve correlation exactly; do not silently truncate identifiers.
        if turn.sub_id.is_empty()
            || call_id.len() > MAX_IDENTIFIER_BYTES
            || turn.sub_id.len() > MAX_IDENTIFIER_BYTES
            || cell_id.is_some_and(|id| id.len() > MAX_IDENTIFIER_BYTES)
        {
            return Err("async completion identifiers exceed the bounded record limit");
        }
        let admit = || {
            let mut state = self.state();
            if state.closed {
                return Err("async completion owner is closed");
            }
            let records = &mut state.records;
            if records.len() >= MAX_RESERVATIONS {
                return Err(
                    "async completion capacity exhausted; continue the owner turn before launching another command",
                );
            }
            let id = ResponseItemId::new("msg");
            let record = Record {
                owner,
                turn_id: turn.sub_id.clone(),
                create_time: Session::response_item_create_time(),
                call_id: call_id.to_owned(),
                producer,
                assignment: turn.agent_assignment.get().cloned(),
                yielded: false,
                claimed: false,
                terminal: None,
                recorded: false,
            };
            // Preserve every identifier; reject before launch if escaped metadata cannot fit.
            if !record.metadata_fits(&id) {
                return Err("async completion metadata exceeds the encoded fragment limit");
            }
            records.insert(id.clone(), record);
            drop(state);
            Ok(CompletionReservation {
                store: Arc::clone(self),
                id,
                registered: false,
            })
        };
        let reservation = if let Some(assignment) = turn.agent_assignment.get()
            && let Some(session) = self.owner.get().and_then(Weak::upgrade)
        {
            session
                .services
                .local_agent_runtime
                .admit_owned_completion(assignment, &turn.sub_id, self, admit)?
        } else {
            admit()?
        };
        self.changed();
        Ok(reservation)
    }

    fn claim(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        drain: CompletionDrain,
        history: &[ResponseItem],
    ) -> (CompletionClaim, Vec<ResponseItemEnvelope>) {
        let mut state = self.state();
        let records = &mut state.records;
        let eligible = |record: &Record| {
            record.yielded
                && !record.claimed
                && record.terminal.is_some()
                && drain.permits(record, owner, turn)
        };
        let mut remaining = MAX_NEW_REQUEST_FRAGMENTS;
        for (id, record) in records.iter_mut() {
            if eligible(record) && history.iter().any(|item| item.id() == Some(id)) {
                record.recorded = true;
                remaining = remaining.saturating_sub(1);
            }
        }
        let mut ids = Vec::new();
        let mut items = Vec::new();
        // Restore the unaccepted recorded batch before admitting fresh completions.
        let mut missing = records
            .iter_mut()
            .filter(|(id, record)| {
                eligible(record) && !history.iter().any(|item| item.id() == Some(*id))
            })
            .collect::<Vec<_>>();
        missing.sort_by_key(|(_, record)| !record.recorded);
        for (id, record) in missing.into_iter().take(remaining) {
            if let Some(fragment) = &record.terminal {
                record.claimed = true;
                ids.push(id.clone());
                items.push(ResponseItemEnvelope::new(record.item(fragment, id)));
            }
        }
        (
            CompletionClaim {
                store: Arc::clone(self),
                ids,
            },
            items,
        )
    }

    pub(super) fn candidates(
        &self,
        owner: ThreadId,
        turn: &TurnContext,
        drain: CompletionDrain,
        input: &[ResponseItem],
    ) -> Vec<ResponseItemId> {
        if drain == CompletionDrain::ExactAssignment
            && self
                .owner
                .get()
                .and_then(Weak::upgrade)
                .is_some_and(|session| {
                    session
                        .services
                        .local_agent_runtime
                        .current_wake_assignment(owner)
                        .as_ref()
                        != turn.agent_assignment.get()
                })
        {
            return Vec::new();
        }
        let state = self.state();
        input
            .iter()
            .filter_map(ResponseItem::id)
            .filter(|id| {
                state
                    .records
                    .get(*id)
                    .is_some_and(|record| record.recorded && drain.permits(record, owner, turn))
            })
            .cloned()
            .collect()
    }

    pub(super) fn accept(self: &Arc<Self>, ids: &[ResponseItemId]) {
        let mut state = self.state();
        for id in ids {
            state.records.remove(id);
        }
        drop(state);
        self.changed();
    }

    pub(crate) fn retire(self: &Arc<Self>) {
        let mut state = self.state();
        state.closed = true;
        state.records.clear();
        drop(state);
        self.changed();
    }
}

impl CompletionWake {
    fn request(&mut self) -> Option<u64> {
        self.revision = self.revision.wrapping_add(1);
        if self.scheduled {
            return None;
        }
        self.scheduled = true;
        Some(self.revision)
    }

    fn finish(&mut self, observed_revision: u64) -> bool {
        self.scheduled = false;
        self.revision != observed_revision
    }
}

impl CompletionReservation {
    /// Binds the provider's cell ID to an unbound cell reservation. Rejects IDs over 128 bytes,
    /// non-cell or already-bound reservations, and IDs whose escaped metadata cannot fit; a
    /// rejected reservation stays unbound.
    pub(crate) fn bind_cell(&mut self, cell_id: &str) -> Result<(), &'static str> {
        if cell_id.len() > MAX_IDENTIFIER_BYTES {
            return Err("async completion identifiers exceed the bounded record limit");
        }
        let mut state = self.store.state();
        let Some(record) = state.records.get_mut(&self.id) else {
            return Err("async completion reservation is no longer held");
        };
        if !matches!(record.producer, Producer::Cell { cell_id: None }) {
            return Err("async completion reservation is not an unbound cell");
        }
        record.producer = Producer::Cell {
            cell_id: Some(cell_id.to_owned()),
        };
        if !record.metadata_fits(&self.id) {
            record.producer = Producer::Cell { cell_id: None };
            return Err("async completion metadata exceeds the encoded fragment limit");
        }
        Ok(())
    }

    pub(crate) fn register(&mut self) -> CompletionPublisher {
        self.registered = true;
        CompletionPublisher {
            store: Arc::downgrade(&self.store),
            id: self.id.clone(),
        }
    }

    pub(crate) fn release(mut self) {
        self.registered = false;
    }
}

impl Drop for CompletionReservation {
    fn drop(&mut self) {
        let mut state = self.store.state();
        let records = &mut state.records;
        if self.registered {
            if let Some(record) = records.get_mut(&self.id) {
                record.yielded = true;
            }
        } else {
            records.remove(&self.id);
        }
        drop(state);
        self.store.changed();
    }
}

impl CompletionPublisher {
    pub(crate) fn publish(&self, status: CompletionStatus, output: &[u8], omitted_bytes: usize) {
        let Some(store) = self.store.upgrade() else {
            return;
        };
        let mut state = store.state();
        let records = &mut state.records;
        let Some(record) = records.get_mut(&self.id) else {
            return;
        };
        // A cell publishes only after its provider-assigned ID is bound.
        if record.terminal.is_some() || matches!(record.producer, Producer::Cell { cell_id: None })
        {
            return;
        }
        let status = match status {
            CompletionStatus::Exited(code) => format!("exited({code})"),
            CompletionStatus::Failed => "failed".to_owned(),
            CompletionStatus::Cancelled => "cancelled".to_owned(),
            CompletionStatus::TimedOut => "timedOut".to_owned(),
            CompletionStatus::CellCompleted => "completed".to_owned(),
            CompletionStatus::CellError => "error".to_owned(),
            CompletionStatus::CollectionFailed => "collectionFailed".to_owned(),
        };
        let start = output.len().saturating_sub(MAX_FRAGMENT_BYTES);
        let tail = String::from_utf8_lossy(&output[start..]);
        let mut trim = 0;
        loop {
            let omitted = omitted_bytes.saturating_add(start).saturating_add(trim);
            let fragment = record.fragment(&self.id, &status, &tail[trim..], omitted);
            let encoded = record.encoded_size(&fragment, &self.id);
            if encoded <= MAX_FRAGMENT_BYTES {
                record.terminal = Some(fragment);
                break;
            }
            // Escapes can expand bytes; trim whole UTF-8 characters from the oldest end until
            // their encoded width covers the excess, so escaped output keeps the full bound.
            // Admission guarantees the metadata alone fits, so an empty tail always terminates.
            let excess = encoded - MAX_FRAGMENT_BYTES;
            let mut removed = 0;
            for ch in tail[trim..].chars() {
                if removed >= excess {
                    break;
                }
                removed += json_escaped_len(ch);
                trim += ch.len_utf8();
            }
        }
        drop(state);
        store.changed();
    }
}

/// Encoded width of one character inside a JSON string, matching `serde_json` escaping.
fn json_escaped_len(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        '\0'..='\u{1f}' => 6,
        _ => ch.len_utf8(),
    }
}

impl Record {
    fn item(&self, fragment: &AsyncToolCompletion, id: &ResponseItemId) -> ResponseItem {
        let mut item = ContextualUserFragment::into(fragment.clone());
        item.set_id(Some(id.clone()));
        // Immutable originating metadata prevents history preparation from growing replayed items.
        item.set_turn_id_if_missing(&self.turn_id);
        item.set_create_time_if_missing(self.create_time.clone());
        item
    }

    fn encoded_size(&self, fragment: &AsyncToolCompletion, id: &ResponseItemId) -> usize {
        // This fixed Message contains UTF-8 strings, a finite JSON Number and string metadata.
        // Serialization to a Vec has neither fallible I/O nor unsupported map keys.
        serde_json::to_vec(&self.item(fragment, id))
            .unwrap_or_else(|error| unreachable!("fixed completion message serialization: {error}"))
            .len()
    }

    /// Checks the longest status and truncation count, so publication only ever trims the tail.
    fn metadata_fits(&self, id: &ResponseItemId) -> bool {
        let metadata = self.fragment(id, "exited(-2147483648)", "", usize::MAX);
        self.encoded_size(&metadata, id) <= MAX_FRAGMENT_BYTES
    }

    fn fragment(
        &self,
        id: &ResponseItemId,
        status: &str,
        tail: &str,
        omitted: usize,
    ) -> AsyncToolCompletion {
        let generation = self
            .assignment
            .as_ref()
            .map(|assignment| assignment.generation.to_string())
            .unwrap_or_else(|| "none".to_owned());
        let producer = match &self.producer {
            Producer::Process {
                process_id,
                cell_id,
            } => format!(
                "kind=process process={process_id} cell={}",
                cell_id.as_deref().unwrap_or("none")
            ),
            Producer::Cell { cell_id } => {
                format!("kind=cell cell={}", cell_id.as_deref().unwrap_or("unbound"))
            }
        };
        // `truncated` is always present so the header shape is stable; it counts output bytes
        // omitted before the tail, including any the producer dropped before publication.
        AsyncToolCompletion {
            text: format!(
                "id={id} owner={} turn={} call={} {producer} assignment={generation} status={status} truncated={omitted}\n{tail}",
                self.owner, self.turn_id, self.call_id,
            ),
        }
    }
}

impl CompletionClaim {
    // Recorded results remain reserved until an ordinary model request accepts them.
    pub(crate) fn recorded(mut self) {
        let mut state = self.store.state();
        let records = &mut state.records;
        for id in self.ids.drain(..) {
            if let Some(record) = records.get_mut(&id) {
                record.recorded = true;
                record.claimed = false;
            }
        }
    }
}

impl Drop for CompletionClaim {
    fn drop(&mut self) {
        let mut state = self.store.state();
        let records = &mut state.records;
        for id in &self.ids {
            if let Some(record) = records.get_mut(id) {
                record.claimed = false;
            }
        }
    }
}

#[cfg(test)]
#[path = "async_completion_tests.rs"]
mod tests;

pub(super) async fn record_ready(
    sess: &Session,
    turn: &TurnContext,
    drain: CompletionDrain,
    model: &ModelInfo,
) -> bool {
    if !turn
        .config
        .features
        .enabled(codex_features::Feature::AsyncProcessCompletion)
    {
        return false;
    }
    if drain == CompletionDrain::ExactAssignment
        && sess
            .services
            .local_agent_runtime
            .current_wake_assignment(sess.thread_id())
            .as_ref()
            != turn.agent_assignment.get()
    {
        return false;
    }
    let history = sess
        .clone_history()
        .await
        .for_prompt(&model.input_modalities);
    let (claim, items) =
        sess.services
            .async_completions
            .claim(sess.thread_id(), turn, drain, &history);
    if items.is_empty() {
        return false;
    }
    sess.record_annotated_conversation_items(turn, model, items)
        .await;
    claim.recorded();
    true
}

#[cfg(test)]
#[path = "async_completion_runtime_tests.rs"]
mod runtime_tests;
