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
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItemEnvelope;
use codex_protocol::openai_models::ModelInfo;
use uuid::Uuid;

const MAX_RESERVATIONS: usize = 64;
// One UTF-8 byte costs at most one byte-fallback token, including the markers.
const MAX_FRAGMENT_BYTES: usize = 768;
const MAX_REQUEST_FRAGMENTS: usize = 8;

#[derive(Default)]
pub(crate) struct AsyncCompletions(Mutex<CompletionState>);

#[derive(Default)]
struct CompletionState {
    closed: bool,
    records: BTreeMap<Uuid, Record>,
}

struct Record {
    owner: ThreadId,
    turn_id: String,
    call_id: String,
    process_id: i32,
    cell_id: Option<String>,
    assignment: Option<AgentAssignmentId>,
    yielded: bool,
    claimed: bool,
    terminal: Option<AsyncToolCompletion>,
}

/// Releases an inline result; cancellation after registration retains its record.
pub(crate) struct CompletionReservation {
    store: Arc<AsyncCompletions>,
    id: Uuid,
    registered: bool,
}

#[derive(Clone)]
pub(crate) struct CompletionPublisher {
    store: Weak<AsyncCompletions>,
    id: Uuid,
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
    ids: Vec<Uuid>,
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
        if call_id.len() > 128
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
        let id = Uuid::new_v4();
        records.insert(
            id,
            Record {
                owner,
                turn_id: turn.sub_id.clone(),
                call_id: call_id.to_owned(),
                process_id,
                cell_id: cell_id.map(str::to_owned),
                assignment: turn.agent_assignment.get().cloned(),
                yielded: false,
                claimed: false,
                terminal: None,
            },
        );
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
    ) -> (CompletionClaim, Vec<ResponseItemEnvelope>) {
        let mut state = self.state();
        let records = &mut state.records;
        let mut ids = Vec::new();
        let mut items = Vec::new();
        for (id, record) in records.iter_mut() {
            if ids.len() == MAX_REQUEST_FRAGMENTS {
                break;
            }
            if !record.yielded
                || record.claimed
                || record.owner != owner
                || record.assignment.as_ref() != turn.agent_assignment.get()
            {
                continue;
            }
            if let Some(fragment) = &record.terminal {
                record.claimed = true;
                ids.push(*id);
                items.push(ResponseItemEnvelope::new(ContextualUserFragment::into(
                    fragment.clone(),
                )));
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
            id: self.id,
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
        let generation = record
            .assignment
            .as_ref()
            .map(|assignment| assignment.generation.to_string())
            .unwrap_or_else(|| "none".to_owned());
        let header = format!(
            "id={} owner={} turn={} call={} kind=process process={} cell={} assignment={generation} status={status}\n",
            self.id,
            record.owner,
            record.turn_id,
            record.call_id,
            record.process_id,
            record.cell_id.as_deref().unwrap_or("none"),
        );
        let markers = AsyncToolCompletion::type_markers();
        let marker_bytes = markers.0.len() + markers.1.len() + 2;
        // Reserve space for the largest possible omission counter before selecting a tail.
        let budget = MAX_FRAGMENT_BYTES.saturating_sub(header.len() + marker_bytes + 64);
        let start = output.len().saturating_sub(budget);
        let mut tail = String::from_utf8_lossy(&output[start..]).into_owned();
        // Lossy UTF-8 conversion can expand invalid bytes; preserve the newest valid tail.
        let mut trim = tail.len().saturating_sub(budget);
        while !tail.is_char_boundary(trim) {
            trim += 1;
        }
        tail.drain(..trim);
        let omitted = omitted_bytes.saturating_add(start).saturating_add(trim);
        let truncation = if omitted > 0 {
            format!("[output truncated; omitted about {omitted} bytes]\n")
        } else {
            String::new()
        };
        record.terminal = Some(AsyncToolCompletion {
            text: format!("{header}{truncation}{tail}"),
        });
    }
}

impl CompletionClaim {
    // Called synchronously with history insertion, before any further await.
    pub(crate) fn recorded(mut self) {
        let mut state = self.store.state();
        let records = &mut state.records;
        for id in self.ids.drain(..) {
            records.remove(&id);
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

pub(super) async fn record_ready(sess: &Session, turn: &TurnContext, model: &ModelInfo) {
    if !turn
        .config
        .features
        .enabled(codex_features::Feature::AsyncProcessCompletion)
    {
        return;
    }
    let (claim, items) = sess
        .services
        .async_completions
        .claim(sess.thread_id(), turn);
    if !items.is_empty() {
        sess.record_prepared_conversation_items(turn, model, items, Vec::new(), Some(claim))
            .await;
    }
}

#[cfg(test)]
#[path = "async_completion_tests.rs"]
mod tests;
