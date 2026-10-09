//! Retains finite producers in their existing assignment without copying their records.

use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use crate::session::async_completion::AsyncCompletions;
use std::sync::Arc;

impl AgentWakeCoordinator {
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
        if record.phase != super::AssignmentPhase::Waiting
            || !record
                .owned_completions
                .upgrade()
                .is_some_and(|store| store.has_ready(id.thread_id, Some(id)))
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
        record.active_turn_id = Some(turn_id.to_owned());
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
                .and_then(|record| record.owned_completions.upgrade())
                .is_some_and(|store| store.has_ready(assignment.thread_id, Some(assignment)))
    }
}

#[cfg(test)]
#[path = "owned_work_tests.rs"]
mod tests;
