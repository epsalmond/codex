//! Bounded detail and freshness state for the agent picker.

use crate::multi_agents::AgentPickerContextUsage;
use crate::multi_agents::AgentPickerThreadDetails;
use codex_protocol::ThreadId;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Semaphore;
use uuid::Uuid;

/// Owns cached row details and guards asynchronous picker refreshes against stale results.
#[derive(Debug)]
pub(crate) struct AgentPickerState {
    details: HashMap<ThreadId, AgentPickerThreadDetails>,
    preview_revisions: HashMap<ThreadId, u64>,
    preview_revision_clock: u64,
    preview_requests_in_flight: HashMap<ThreadId, (Uuid, u64)>,
    preview_backfill_attempted: HashSet<ThreadId>,
    status_revisions: HashMap<ThreadId, u64>,
    status_revision_clock: u64,
    completed_preview_turns: HashMap<ThreadId, String>,
    pub(super) preview_backfill_semaphore: Arc<Semaphore>,
    preview_root: Option<ThreadId>,
    preview_generation: Option<Uuid>,
}

impl Default for AgentPickerState {
    fn default() -> Self {
        Self {
            details: HashMap::new(),
            preview_revisions: HashMap::new(),
            preview_revision_clock: 0,
            preview_requests_in_flight: HashMap::new(),
            preview_backfill_attempted: HashSet::new(),
            status_revisions: HashMap::new(),
            status_revision_clock: 0,
            completed_preview_turns: HashMap::new(),
            preview_backfill_semaphore: Arc::new(Semaphore::new(/*permits*/ 16)),
            preview_root: None,
            preview_generation: None,
        }
    }
}

impl AgentPickerState {
    pub(super) fn start_thread(&mut self, thread_id: ThreadId) {
        self.advance_preview_revision(thread_id);
        self.bump_status_revision(thread_id);
    }

    pub(super) fn details(&self, thread_id: &ThreadId) -> Option<&AgentPickerThreadDetails> {
        self.details.get(thread_id)
    }

    pub(super) fn ensure_preview_generation(&mut self, root: ThreadId) -> Uuid {
        if self.preview_root != Some(root) {
            self.preview_root = Some(root);
            self.preview_generation = Some(Uuid::new_v4());
            self.preview_requests_in_flight.clear();
            self.preview_backfill_attempted.clear();
        }
        *self.preview_generation.get_or_insert_with(Uuid::new_v4)
    }

    pub(super) fn start_preview_generation(&mut self, root: ThreadId) -> Uuid {
        self.preview_root = Some(root);
        let generation = Uuid::new_v4();
        self.preview_generation = Some(generation);
        self.preview_requests_in_flight.clear();
        self.preview_backfill_attempted.clear();
        generation
    }

    pub(super) fn preview_generation_matches(&self, root: ThreadId, generation: Uuid) -> bool {
        self.preview_root == Some(root) && self.preview_generation == Some(generation)
    }

    pub(super) fn invalidate_preview_generation(&mut self) {
        self.preview_root = None;
        self.preview_generation = None;
        self.preview_requests_in_flight.clear();
        self.preview_backfill_attempted.clear();
    }

    pub(super) fn begin_preview_backfill(&mut self, thread_id: ThreadId) -> Option<u64> {
        let generation = self.preview_generation?;
        if self.preview_backfill_attempted.contains(&thread_id)
            || self.preview_requests_in_flight.contains_key(&thread_id)
            || self
                .details(&thread_id)
                .is_some_and(|details| details.response_preview.is_some())
        {
            return None;
        }
        let revision = if let Some(revision) = self.preview_revisions.get(&thread_id).copied() {
            revision
        } else {
            self.advance_preview_revision(thread_id);
            self.preview_revisions[&thread_id]
        };
        self.preview_requests_in_flight
            .insert(thread_id, (generation, revision));
        self.preview_backfill_attempted.insert(thread_id);
        Some(revision)
    }

    pub(super) fn finish_preview_backfill(
        &mut self,
        root: ThreadId,
        generation: Uuid,
        thread_id: ThreadId,
        revision: u64,
        result: Result<Option<String>, String>,
    ) {
        if self.preview_requests_in_flight.get(&thread_id) != Some(&(generation, revision)) {
            return;
        }
        self.preview_requests_in_flight.remove(&thread_id);
        if !self.preview_generation_matches(root, generation)
            || self
                .preview_revisions
                .get(&thread_id)
                .copied()
                .unwrap_or_default()
                != revision
        {
            return;
        }
        if let Ok(Some(preview)) = result {
            self.details.entry(thread_id).or_default().response_preview = Some(preview);
            self.advance_preview_revision(thread_id);
        }
    }

    pub(super) fn set_response_preview(&mut self, thread_id: ThreadId, preview: Option<String>) {
        self.details.entry(thread_id).or_default().response_preview = preview;
        self.preview_requests_in_flight.remove(&thread_id);
        self.advance_preview_revision(thread_id);
        self.preview_backfill_attempted.remove(&thread_id);
    }

    pub(super) fn note_completed_preview_turn(
        &mut self,
        thread_id: ThreadId,
        turn_id: &str,
        preview: Option<String>,
    ) -> bool {
        if self
            .completed_preview_turns
            .get(&thread_id)
            .is_some_and(|previous| previous == turn_id)
        {
            return false;
        }
        self.completed_preview_turns
            .insert(thread_id, turn_id.to_string());
        if let Some(preview) = preview {
            self.set_response_preview(thread_id, Some(preview));
        } else if self
            .details
            .get(&thread_id)
            .is_none_or(|details| details.response_preview.is_none())
        {
            self.advance_preview_revision(thread_id);
            self.preview_requests_in_flight.remove(&thread_id);
            self.preview_backfill_attempted.remove(&thread_id);
        }
        self.details
            .get(&thread_id)
            .is_none_or(|details| details.response_preview.is_none())
    }

    pub(super) fn set_context_usage(
        &mut self,
        thread_id: ThreadId,
        context_usage: Option<AgentPickerContextUsage>,
    ) {
        self.details.entry(thread_id).or_default().context_usage = context_usage;
    }

    pub(super) fn set_context_snapshot(
        &mut self,
        thread_id: ThreadId,
        snapshot: codex_app_server_protocol::ThreadContextUsage,
    ) {
        self.details.entry(thread_id).or_default().context_snapshot = Some(snapshot);
    }

    pub(super) fn set_error(&mut self, thread_id: ThreadId, is_error: bool) {
        self.details.entry(thread_id).or_default().is_error = is_error;
        self.bump_status_revision(thread_id);
    }

    pub(super) fn set_error_without_revision(&mut self, thread_id: ThreadId, is_error: bool) {
        self.details.entry(thread_id).or_default().is_error = is_error;
    }

    pub(super) fn status_revision_snapshot(&self) -> HashMap<ThreadId, u64> {
        self.status_revisions.clone()
    }

    pub(super) fn status_revision_matches(
        &self,
        thread_id: ThreadId,
        snapshot: &HashMap<ThreadId, u64>,
    ) -> bool {
        self.status_revisions
            .get(&thread_id)
            .copied()
            .unwrap_or_default()
            == snapshot.get(&thread_id).copied().unwrap_or_default()
    }

    pub(super) fn bump_status_revision(&mut self, thread_id: ThreadId) {
        self.status_revision_clock += 1;
        self.status_revisions
            .insert(thread_id, self.status_revision_clock);
    }

    fn advance_preview_revision(&mut self, thread_id: ThreadId) {
        self.preview_revision_clock += 1;
        self.preview_revisions
            .insert(thread_id, self.preview_revision_clock);
    }

    pub(super) fn clear_details(&mut self, thread_id: ThreadId) {
        self.details.entry(thread_id).or_default().response_preview = None;
        self.details.entry(thread_id).or_default().context_usage = None;
        self.details.entry(thread_id).or_default().context_snapshot = None;
        self.preview_requests_in_flight.remove(&thread_id);
        self.preview_backfill_attempted.remove(&thread_id);
        self.advance_preview_revision(thread_id);
    }

    pub(super) fn clear_thread(&mut self, thread_id: ThreadId) {
        self.details.remove(&thread_id);
        self.preview_revisions.remove(&thread_id);
        self.preview_requests_in_flight.remove(&thread_id);
        self.preview_backfill_attempted.remove(&thread_id);
        self.status_revisions.remove(&thread_id);
        self.completed_preview_turns.remove(&thread_id);
    }

    pub(super) fn clear(&mut self) {
        self.details.clear();
        self.preview_revisions.clear();
        self.preview_requests_in_flight.clear();
        self.preview_backfill_attempted.clear();
        self.status_revisions.clear();
        self.completed_preview_turns.clear();
        self.invalidate_preview_generation();
    }
}

#[cfg(test)]
#[path = "agent_picker_state_tests.rs"]
mod tests;
