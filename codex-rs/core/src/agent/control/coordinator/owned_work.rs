//! Retains finite producers in their existing assignment without copying their records.

use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use crate::session::async_completion::AsyncCompletions;
use std::sync::Arc;

impl AgentWakeCoordinator {
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
}

#[cfg(test)]
#[path = "owned_work_tests.rs"]
mod tests;
