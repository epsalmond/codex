//! Retains finite producers in their existing assignment without copying their records.

use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use crate::session::async_completion::AsyncCompletions;
use crate::session::async_completion::PendingRecord;
use codex_protocol::ThreadId;
use std::sync::Arc;

/// Work a terminal report can name as holding its assignment open.
pub(crate) struct NamedPendingWork {
    pub(crate) children: Vec<ThreadId>,
    pub(crate) completions: Vec<PendingRecord>,
}

impl super::Assignment {
    pub(super) fn has_finite_terminal_hint(&self) -> bool {
        self.finite_terminal_revision != self.acknowledged_finite_terminal_revision
    }

    fn has_ready_owned_completion(&self, assignment: &AgentAssignmentId) -> bool {
        self.has_finite_terminal_hint()
            || self
                .owned_completions
                .upgrade()
                .is_some_and(|store| store.has_ready(assignment.thread_id, Some(assignment)))
    }
}

impl AgentWakeCoordinator {
    /// Named work holding the current assignment `id` open: its open direct children and its
    /// yielded finite completion records, leaving out the code cell `asking_cell`. One snapshot,
    /// in the coordinator -> store order of turn-end classification. A subset of that
    /// classification: live inline producers and unseen terminal hints are left to its hold.
    pub(crate) fn named_pending_work(
        &self,
        id: &AgentAssignmentId,
        asking_cell: Option<&str>,
    ) -> Option<NamedPendingWork> {
        let state = self.lock_state();
        if state.current_by_thread.get(&id.thread_id) != Some(id) {
            return None;
        }
        let assignment = state.assignments.get(id)?;
        Some(NamedPendingWork {
            children: assignment
                .direct_children
                .iter()
                .map(|child| child.thread_id)
                .collect(),
            completions: assignment
                .owned_completions
                .upgrade()
                .map(|store| store.pending_for_assignment(id, asking_cell))
                .unwrap_or_default(),
        })
    }

    /// Caller holds lifecycle; the derived scalar is mutated only in this lock order.
    pub(crate) fn admit_finite_producer(
        &self,
        assignment: &AgentAssignmentId,
        turn_id: &str,
        retained_cell: bool,
        finite: bool,
    ) -> Result<(), &'static str> {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&assignment.thread_id) != Some(assignment) {
            return Err("work assignment is no longer current");
        }
        let record = state
            .assignments
            .get_mut(assignment)
            .ok_or("work assignment is missing")?;
        if !record.phase.is_open()
            || (!retained_cell
                && (record.phase != super::AssignmentPhase::Running
                    || record.active_turn_id.as_deref() != Some(turn_id)))
        {
            return Err("work turn does not own the current assignment");
        }
        if finite {
            record.finite_owned_producers += 1;
        }
        state.signal_wake_event();
        Ok(())
    }

    pub(crate) fn release_finite_producer(
        &self,
        assignment: &AgentAssignmentId,
        terminal_hint: bool,
    ) -> bool {
        let mut state = self.lock_state();
        let current = state.current_by_thread.get(&assignment.thread_id) == Some(assignment);
        let Some(record) = state.assignments.get_mut(assignment) else {
            return false;
        };
        record.finite_owned_producers = record.finite_owned_producers.saturating_sub(1);
        if terminal_hint && current && record.phase.is_open() {
            record.finite_terminal_revision = record.finite_terminal_revision.wrapping_add(1);
        }
        let wake = current
            && record.phase == super::AssignmentPhase::Waiting
            && record.has_finite_terminal_hint();
        state.signal_wake_event();
        wake
    }

    /// Capture at ordinary model preparation: this active turn incorporates all observed hints.
    pub(crate) fn acknowledge_finite_terminal_hint(
        &self,
        assignment: &AgentAssignmentId,
        turn_id: &str,
    ) {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&assignment.thread_id) != Some(assignment) {
            return;
        }
        if let Some(record) = state.assignments.get_mut(assignment)
            && record.phase == super::AssignmentPhase::Running
            && record.active_turn_id.as_deref() == Some(turn_id)
        {
            record.acknowledged_finite_terminal_revision = record.finite_terminal_revision;
            record.bound_finite_hint = None;
            state.signal_wake_event();
        }
    }

    pub(crate) fn admit_registered_completion<T>(
        &self,
        assignment: &AgentAssignmentId,
        store: &Arc<AsyncCompletions>,
        admit: impl FnOnce() -> Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&assignment.thread_id) != Some(assignment) {
            return Err("work assignment is no longer current");
        }
        let record = state
            .assignments
            .get_mut(assignment)
            .ok_or("work assignment is missing")?;
        if !record.phase.is_open() {
            return Err("work assignment is no longer active");
        }
        let result = admit()?;
        record.owned_completions = Arc::downgrade(store);
        state.signal_wake_event();
        Ok(result)
    }

    pub(crate) fn continue_owned_completion(
        &self,
        id: &AgentAssignmentId,
        turn_id: &str,
    ) -> Result<(), &'static str> {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&id.thread_id) != Some(id) {
            return Err("async completion assignment is no longer current");
        }
        let record = state
            .assignments
            .get(id)
            .ok_or("async completion assignment is missing")?;
        if record.phase != super::AssignmentPhase::Waiting || !record.has_ready_owned_completion(id)
        {
            return Err("async completion assignment is not waiting for a ready result");
        }
        if record.parent.as_ref().is_some_and(|parent| {
            state.current_by_thread.get(&parent.thread_id) != Some(parent)
                || !state
                    .assignments
                    .get(parent)
                    .is_some_and(|parent| parent.phase.is_open())
        }) {
            return Err("async completion parent assignment is no longer active");
        }
        let record = state
            .assignments
            .get_mut(id)
            .ok_or("async completion assignment is missing")?;
        let previous = record.acknowledged_finite_terminal_revision;
        let captured = record.finite_terminal_revision;
        record.acknowledged_finite_terminal_revision = captured;
        record.bound_finite_hint = Some((previous, captured));
        record.active_turn_id = Some(turn_id.to_owned());
        record.active_turn_origin = super::ActiveTurnOrigin::Waiting;
        record.phase = super::AssignmentPhase::Running;
        state.signal_wake_event();
        Ok(())
    }

    // Admission and terminal classification share coordinator -> store lock ordering.
    pub(crate) fn admit_owned_completion<T>(
        &self,
        assignment: &AgentAssignmentId,
        turn_id: &str,
        store: &Arc<AsyncCompletions>,
        admit: impl FnOnce() -> Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&assignment.thread_id) != Some(assignment) {
            return Err("async completion assignment is no longer current");
        }
        let record = state
            .assignments
            .get_mut(assignment)
            .ok_or("async completion assignment is missing")?;
        if record.phase != super::AssignmentPhase::Running
            || record.active_turn_id.as_deref() != Some(turn_id)
        {
            return Err("async completion turn does not own the running assignment");
        }
        let result = admit()?;
        record.owned_completions = Arc::downgrade(store);
        state.signal_wake_event();
        Ok(result)
    }

    pub(crate) fn observe_owned_completions(
        &self,
        assignment: &AgentAssignmentId,
        store: &Arc<AsyncCompletions>,
    ) {
        let state = self.lock_state();
        if state.current_by_thread.get(&assignment.thread_id) != Some(assignment) {
            return;
        }
        state.signal_wake_event();
        drop(state);
        if store.has_ready(assignment.thread_id, Some(assignment)) {
            self.request_wake(assignment.clone());
        }
    }

    pub(crate) fn has_ready_owned_completions(&self, assignment: &AgentAssignmentId) -> bool {
        let state = self.lock_state();
        state.current_by_thread.get(&assignment.thread_id) == Some(assignment)
            && state
                .assignments
                .get(assignment)
                .is_some_and(|record| record.has_ready_owned_completion(assignment))
    }
}

#[cfg(test)]
#[path = "owned_work_tests.rs"]
mod tests;
