//! Bounded owner records. Publishing does not start a turn or grant tool authority.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use crate::agent::control::AgentAssignmentId;
use crate::context::AsyncToolCompletion;
use crate::context::ContextualUserFragment;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;

const MAX_RESERVATIONS: usize = 64;
// Byte-fallback bounds tokens by encoded UTF-8 bytes, including IDs, metadata and escapes.
const MAX_FRAGMENT_BYTES: usize = 768;
const MAX_NEW_REQUEST_FRAGMENTS: usize = 8;

#[derive(Default)]
pub(crate) struct AsyncCompletions(Mutex<CompletionState>);

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
    process_id: i32,
    cell_id: Option<String>,
    assignment: Option<AgentAssignmentId>,
    yielded: bool,
    claimed: bool,
    terminal: Option<AsyncToolCompletion>,
    recorded: bool,
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
    Cancelled,
    TimedOut,
}

/// Returning an unrecorded claim makes it available to the next permitted turn.
pub(crate) struct CompletionClaim {
    store: Arc<AsyncCompletions>,
    ids: Vec<ResponseItemId>,
}

impl AsyncCompletions {
    fn state(&self) -> std::sync::MutexGuard<'_, CompletionState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        call_id: &str,
        process_id: i32,
        cell_id: Option<&str>,
    ) -> Result<CompletionReservation, &'static str> {
        // Preserve correlation exactly; do not silently truncate identifiers.
        if turn.sub_id.is_empty()
            || call_id.len() > 128
            || turn.sub_id.len() > 128
            || cell_id.is_some_and(|id| id.len() > 128)
        {
            return Err("async completion identifiers exceed the bounded record limit");
        }
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
            process_id,
            cell_id: cell_id.map(str::to_owned),
            assignment: turn.agent_assignment.get().cloned(),
            yielded: false,
            claimed: false,
            terminal: None,
            recorded: false,
        };
        // Preserve every identifier; reject before launch if escaped metadata cannot fit.
        let metadata = record.fragment(&id, "exited(-2147483648)", "", usize::MAX);
        if record.encoded_size(&metadata, &id) > MAX_FRAGMENT_BYTES {
            return Err("async completion metadata exceeds the encoded fragment limit");
        }
        records.insert(id.clone(), record);
        Ok(CompletionReservation {
            store: Arc::clone(self),
            id,
            registered: false,
        })
    }

    fn claim(
        self: &Arc<Self>,
        owner: ThreadId,
        turn: &TurnContext,
        history: &[ResponseItem],
    ) -> (CompletionClaim, Vec<ResponseItemEnvelope>) {
        let mut state = self.state();
        let records = &mut state.records;
        let eligible = |record: &Record| {
            record.yielded
                && !record.claimed
                && record.terminal.is_some()
                && record.owner == owner
                && record.assignment.as_ref() == turn.agent_assignment.get()
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
        input: &[ResponseItem],
    ) -> Vec<ResponseItemId> {
        let state = self.state();
        input
            .iter()
            .filter_map(ResponseItem::id)
            .filter(|id| {
                state.records.get(*id).is_some_and(|record| {
                    record.recorded
                        && record.owner == owner
                        && record.assignment.as_ref() == turn.agent_assignment.get()
                })
            })
            .cloned()
            .collect()
    }

    pub(super) fn accept(&self, ids: &[ResponseItemId]) {
        let mut state = self.state();
        for id in ids {
            state.records.remove(id);
        }
    }

    pub(crate) fn retire(&self) {
        let mut state = self.state();
        state.closed = true;
        state.records.clear();
    }
}

impl CompletionReservation {
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
        if record.terminal.is_some() {
            return;
        }
        let status = match status {
            CompletionStatus::Exited(code) => format!("exited({code})"),
            CompletionStatus::Failed => "failed".to_owned(),
            CompletionStatus::Cancelled => "cancelled".to_owned(),
            CompletionStatus::TimedOut => "timedOut".to_owned(),
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
            // Escapes can expand bytes; trim whole UTF-8 characters from the oldest end.
            trim = (trim + encoded - MAX_FRAGMENT_BYTES).min(tail.len());
            while !tail.is_char_boundary(trim) {
                trim += 1;
            }
        }
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
        let header = format!(
            "id={id} owner={} turn={} call={} kind=process process={} cell={} assignment={generation} status={status}\n",
            self.owner,
            self.turn_id,
            self.call_id,
            self.process_id,
            self.cell_id.as_deref().unwrap_or("none"),
        );
        let truncation = if omitted > 0 {
            format!("[output truncated; omitted about {omitted} bytes]\n")
        } else {
            String::new()
        };
        AsyncToolCompletion {
            text: format!("{header}{truncation}{tail}"),
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
