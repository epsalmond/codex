//! Bounded owner records, retained until ordinary model acceptance.
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;

use crate::agent::control::AgentAssignmentId;
use crate::agent::control::OwnedWorkRegistration;
use crate::agent::control::WorkKind;
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
/// and escapes. 3072 bytes is about 768 tokens at a typical ~4 bytes per token. It is not a hard
/// token bound: token-dense output (hex, base64, CJK, minified JSON) can exceed 768 tokens; see
/// epsalmond/codex#192 for a token-count cap. 64 reservations at this size bound each owner's
/// stored payloads to 192 KiB.
pub(crate) const MAX_FRAGMENT_BYTES: usize = 3072;
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
    /// Recent automatically collected cells, including accepted records. Identity alone keeps
    /// late `wait` calls friendly without retaining payloads, reservations or owned work.
    collected_cells: VecDeque<(String, ResponseItemId)>,
}

struct Record {
    lifecycle: Option<OwnedWorkRegistration>,
    work_kind: WorkKind,
    owner: ThreadId,
    turn_id: String,
    create_time: serde_json::Number,
    call_id: String,
    producer: Producer,
    /// A later binding of the same provider ID supersedes identity lookup, not delivery.
    cell_identity_current: bool,
    assignment: Option<AgentAssignmentId>,
    /// A Cell's initial call yielded, even while its native-close delivery fence is retained.
    cell_yielded_for_visibility: bool,
    /// The initial call's delivery fence was released; Cells wait for native close.
    yielded: bool,
    claimed: bool,
    terminal: Option<AsyncToolCompletion>,
    recorded: bool,
    /// Display-only producer command, bounded by [`MAX_COMMAND_LABEL_BYTES`].
    command: Option<String>,
}

/// Bounds the display-only command kept with each record for status views.
const MAX_COMMAND_LABEL_BYTES: usize = 512;

/// One retained, yielded completion record, as shown by status views.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingRecord {
    pub(crate) id: String,
    pub(crate) call_id: String,
    pub(crate) process_id: Option<i32>,
    pub(crate) cell_id: Option<String>,
    pub(crate) command: Option<String>,
    /// Unix seconds when the record was reserved, i.e. when its producer started.
    pub(crate) created_at: i64,
    /// The producer finished and its result awaits delivery to the owner.
    pub(crate) ready: bool,
    /// The result's publication starts (or started) an owner turn on its own.
    pub(crate) wakes_owner: bool,
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

    /// The authoritative automatic message ID, independent of delivery or acceptance. Call under
    /// the cell's observation lock when racing a collector; this never claims or drains a record.
    pub(crate) fn collected_cell(&self, cell_id: &str) -> Option<ResponseItemId> {
        let state = self.state();
        // Live records must remain discoverable even if newer accepted identities evict history.
        let live = state
            .records
            .iter()
            .find_map(|(id, record)| match &record.producer {
                Producer::Cell {
                    cell_id: Some(bound),
                } if bound == cell_id
                    && record.cell_identity_current
                    && record.terminal.is_some() =>
                {
                    Some(id.clone())
                }
                _ => None,
            });
        live.or_else(|| {
            state
                .collected_cells
                .iter()
                .rev()
                .find_map(|(bound, id)| (bound == cell_id).then(|| id.clone()))
        })
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

    #[cfg(test)]
    pub(crate) fn accept_for_test(self: &Arc<Self>, ids: &[ResponseItemId]) {
        self.accept(ids);
    }

    /// Yielded records not yet accepted by the owner: running producers whose result will be
    /// delivered automatically, and finished results awaiting delivery.
    pub(crate) fn pending(&self) -> Vec<PendingRecord> {
        // Mirror `changed`: the current wake assignment owns wakeups; without one, only a root
        // owner wakes for unassigned records. Read it before taking the record mutex.
        let owner = self.owner.get().and_then(Weak::upgrade);
        let wake_assignment = owner.as_ref().and_then(|owner| {
            owner
                .services
                .local_agent_runtime
                .current_wake_assignment(owner.thread_id())
        });
        let root_owner = self.root_owner.get() == Some(&true);
        self.state()
            .records
            .iter()
            .filter(|(_, record)| record.yielded || record.cell_yielded_for_visibility)
            .map(|(id, record)| {
                let wakes_owner = match &wake_assignment {
                    Some(assignment) => record.assignment.as_ref() == Some(assignment),
                    None => root_owner && record.assignment.is_none(),
                };
                record.pending(id, wakes_owner)
            })
            .collect()
    }

    /// The yielded finite records that keep `assignment` open, leaving out the code cell
    /// `asking_cell`. Running inline producers have not yielded and are not listed. Takes only the
    /// record mutex, so the coordinator may call it under its own lock.
    pub(crate) fn pending_for_assignment(
        &self,
        assignment: &AgentAssignmentId,
        asking_cell: Option<&str>,
    ) -> Vec<PendingRecord> {
        self.state()
            .records
            .iter()
            .filter(|(_, record)| {
                record.owner == assignment.thread_id
                    && record.assignment.as_ref() == Some(assignment)
                    && record.work_kind.is_finite()
                    && (record.yielded || record.cell_yielded_for_visibility)
                    && match &record.producer {
                        Producer::Process { .. } => true,
                        Producer::Cell { cell_id } => {
                            cell_id.is_some() && cell_id.as_deref() != asking_cell
                        }
                    }
            })
            .map(|(id, record)| record.pending(id, /*wakes_owner*/ true))
            .collect()
    }

    /// Whether any finite record, yielded or still inline, keeps `assignment` open at turn end.
    /// Broader than [`Self::pending_for_assignment`], which names only yielded records.
    pub(crate) fn owns_assignment(&self, assignment: &AgentAssignmentId) -> bool {
        self.state().records.values().any(|record| {
            record.owner == assignment.thread_id
                && record.assignment.as_ref() == Some(assignment)
                && record.work_kind.is_finite()
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

    #[cfg(test)]
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
            None,
        )
    }

    /// Reserve using an already-admitted lifecycle identity, including retained Cell calls.
    pub(crate) fn reserve_with_lifecycle(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        call_id: &str,
        process_id: i32,
        cell_id: Option<&str>,
        lifecycle: &OwnedWorkRegistration,
    ) -> Result<CompletionReservation, &'static str> {
        self.reserve_producer(
            owner,
            turn,
            call_id,
            Producer::Process {
                process_id,
                cell_id: cell_id.map(str::to_owned),
            },
            Some(lifecycle),
        )
    }

    pub(crate) fn reserve_cell_with_lifecycle(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        call_id: &str,
        lifecycle: &OwnedWorkRegistration,
    ) -> Result<CompletionReservation, &'static str> {
        self.reserve_producer(
            owner,
            turn,
            call_id,
            Producer::Cell { cell_id: None },
            Some(lifecycle),
        )
    }

    /// Reserves capacity for a Code Mode cell before it starts; bind the provider's cell ID with
    /// [`CompletionReservation::bind_cell`] before publishing.
    #[cfg(test)]
    pub(crate) fn reserve_cell(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        call_id: &str,
    ) -> Result<CompletionReservation, &'static str> {
        self.reserve_producer(owner, turn, call_id, Producer::Cell { cell_id: None }, None)
    }

    fn reserve_producer(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        call_id: &str,
        producer: Producer,
        lifecycle: Option<&OwnedWorkRegistration>,
    ) -> Result<CompletionReservation, &'static str> {
        if lifecycle.is_some_and(|registration| {
            !registration.matches(owner, &turn.sub_id, call_id, turn.agent_assignment.get())
        }) {
            return Err("async completion lifecycle identity does not match its record");
        }
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
            let mut record = Record {
                lifecycle: None,
                work_kind: lifecycle.map_or(WorkKind::Task, OwnedWorkRegistration::kind),
                owner,
                turn_id: turn.sub_id.clone(),
                create_time: Session::response_item_create_time(),
                call_id: call_id.to_owned(),
                producer,
                cell_identity_current: true,
                assignment: turn.agent_assignment.get().cloned(),
                cell_yielded_for_visibility: false,
                yielded: false,
                claimed: false,
                terminal: None,
                recorded: false,
                command: None,
            };
            // Preserve every identifier; reject before launch if escaped metadata cannot fit.
            if !record.metadata_fits(&id) {
                return Err("async completion metadata exceeds the encoded fragment limit");
            }
            // No fallible path or replacement may drop a registration under the store lock.
            record.lifecycle = lifecycle.cloned();
            records.insert(id.clone(), record);
            drop(state);
            Ok(CompletionReservation {
                store: Arc::clone(self),
                id,
                registered: false,
            })
        };
        let reservation = if let Some(lifecycle) = lifecycle {
            lifecycle.admit_completion(self, admit)?
        } else if let Some(assignment) = turn.agent_assignment.get()
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
        let removed = ids
            .iter()
            .filter_map(|id| state.records.remove(id))
            .collect::<Vec<_>>();
        drop(state);
        drop(removed);
        self.changed();
    }

    pub(crate) fn retire(self: &Arc<Self>) {
        let mut state = self.state();
        state.closed = true;
        state.collected_cells.clear();
        let removed = std::mem::take(&mut state.records);
        drop(state);
        drop(removed);
        self.changed();
    }
}

impl Record {
    fn pending(&self, id: &ResponseItemId, wakes_owner: bool) -> PendingRecord {
        let (process_id, cell_id) = match &self.producer {
            Producer::Process {
                process_id,
                cell_id,
            } => (Some(*process_id), cell_id.clone()),
            Producer::Cell { cell_id } => (None, cell_id.clone()),
        };
        PendingRecord {
            id: id.to_string(),
            call_id: self.call_id.clone(),
            process_id,
            cell_id,
            command: self.command.clone(),
            created_at: self.create_time.as_f64().map_or(0, |time| time as i64),
            ready: self.yielded && self.terminal.is_some(),
            wakes_owner,
        }
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
    /// Shares the producer's one registration with ordinary result acceptance.
    #[cfg(test)]
    pub(crate) fn attach_lifecycle(
        &mut self,
        lifecycle: &OwnedWorkRegistration,
    ) -> Result<(), &'static str> {
        let mut state = self.store.state();
        let record = state
            .records
            .get_mut(&self.id)
            .ok_or("async completion reservation is no longer held")?;
        if record.lifecycle.is_some()
            || !lifecycle.matches(
                record.owner,
                &record.turn_id,
                &record.call_id,
                record.assignment.as_ref(),
            )
        {
            return Err("async completion lifecycle identity does not match its record");
        }
        let previous = record.work_kind;
        record.work_kind = lifecycle.kind();
        if !record.metadata_fits(&self.id) {
            record.work_kind = previous;
            return Err("async completion metadata exceeds the encoded fragment limit");
        }
        record.lifecycle = Some(lifecycle.clone());
        Ok(())
    }

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
        // A provider reusing an ID starts a new cell, not a reference to an older result.
        for (id, record) in &mut state.records {
            if id != &self.id
                && matches!(&record.producer, Producer::Cell { cell_id: Some(bound) } if bound == cell_id)
            {
                record.cell_identity_current = false;
            }
        }
        state.collected_cells.retain(|(bound, _)| bound != cell_id);
        Ok(())
    }

    /// Records the producer's command for status views, truncated at a character boundary.
    pub(crate) fn set_command(&mut self, command: &str) {
        let command =
            codex_utils_string::take_bytes_at_char_boundary(command, MAX_COMMAND_LABEL_BYTES);
        if let Some(record) = self.store.state().records.get_mut(&self.id) {
            record.command = Some(command.to_owned());
        }
    }

    pub(crate) fn register(&mut self) -> CompletionPublisher {
        self.registered = true;
        CompletionPublisher {
            store: Arc::downgrade(&self.store),
            id: self.id.clone(),
        }
    }

    /// Exposes an actually yielded Cell in status views without releasing its delivery fence.
    /// Call only after registering automatic delivery for the initial yielded response.
    pub(crate) fn mark_cell_yielded_for_visibility(&self) {
        if !self.registered {
            return;
        }
        if let Some(record) = self.store.state().records.get_mut(&self.id)
            && matches!(record.producer, Producer::Cell { cell_id: Some(_) })
        {
            record.cell_yielded_for_visibility = true;
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
        let removed = if self.registered {
            if let Some(record) = records.get_mut(&self.id) {
                record.yielded = true;
            }
            None
        } else {
            records.remove(&self.id)
        };
        drop(state);
        drop(removed);
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
        if let Producer::Cell {
            cell_id: Some(cell_id),
        } = &record.producer
            && record.cell_identity_current
        {
            let cell_id = cell_id.clone();
            state.collected_cells.push_back((cell_id, self.id.clone()));
            if state.collected_cells.len() > MAX_RESERVATIONS {
                state.collected_cells.pop_front();
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
        let classification = if self.work_kind == WorkKind::Service {
            " work_kind=service"
        } else {
            ""
        };
        // `truncated` is always present so the header shape is stable; it counts output bytes
        // omitted before the tail, including any the producer dropped before publication.
        AsyncToolCompletion {
            text: format!(
                "id={id} owner={} turn={} call={} {producer}{classification} assignment={generation} status={status} truncated={omitted}\n{tail}",
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
    if let Some(assignment) = turn.agent_assignment.get() {
        sess.services
            .local_agent_runtime
            .acknowledge_finite_terminal_hint(assignment, &turn.sub_id);
    }
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
