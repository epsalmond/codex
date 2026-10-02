use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use super::AssignmentPhase;
use super::TurnEndDisposition;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;

impl AgentWakeCoordinator {
    pub(crate) fn current_assignment(&self, thread_id: ThreadId) -> Option<AgentAssignmentId> {
        self.lock_state().current_by_thread.get(&thread_id).cloned()
    }

    pub(crate) fn assignment_status(&self, thread_id: ThreadId) -> Option<AgentStatus> {
        let state = self.lock_state();
        let id = state.current_by_thread.get(&thread_id)?;
        let assignment = state.assignments.get(id)?;
        match assignment.phase {
            AssignmentPhase::Reserved => None,
            AssignmentPhase::Running => Some(AgentStatus::Running),
            AssignmentPhase::Waiting => Some(AgentStatus::Waiting),
            AssignmentPhase::Interrupted | AssignmentPhase::Cancelled => {
                Some(AgentStatus::Interrupted)
            }
            AssignmentPhase::Completed | AssignmentPhase::Errored => None,
        }
    }

    pub(crate) fn parent_assignment(&self, id: &AgentAssignmentId) -> Option<AgentAssignmentId> {
        self.lock_state()
            .assignments
            .get(id)
            .and_then(|assignment| assignment.parent.clone())
    }

    pub(crate) fn active_assignment_for_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Option<AgentAssignmentId> {
        let state = self.lock_state();
        let id = state.current_by_thread.get(&thread_id)?;
        state
            .assignments
            .get(id)
            .filter(|assignment| {
                assignment.phase == AssignmentPhase::Running
                    && assignment.active_turn_id.as_deref() == Some(turn_id)
            })
            .map(|_| id.clone())
    }

    pub(crate) fn terminal_assignment_for_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Option<AgentAssignmentId> {
        let state = self.lock_state();
        let id = state.current_by_thread.get(&thread_id)?;
        state
            .assignments
            .get(id)
            .filter(|assignment| {
                assignment.phase.is_terminal()
                    && assignment.terminal_turn_id.as_deref() == Some(turn_id)
            })
            .map(|_| id.clone())
    }

    pub(crate) fn is_current_open_assignment(&self, id: &AgentAssignmentId) -> bool {
        let state = self.lock_state();
        state.current_by_thread.get(&id.thread_id) == Some(id)
            && state
                .assignments
                .get(id)
                .is_some_and(|assignment| assignment.phase.is_open())
    }

    pub(crate) fn is_current_waiting_assignment(&self, id: &AgentAssignmentId) -> bool {
        let state = self.lock_state();
        state.current_by_thread.get(&id.thread_id) == Some(id)
            && state
                .assignments
                .get(id)
                .is_some_and(|assignment| assignment.phase == AssignmentPhase::Waiting)
    }

    pub(crate) fn cancel_assignment(&self, id: &AgentAssignmentId) {
        let mut state = self.lock_state();
        Self::release_assignment(&mut state, id);
        drop(state);
        self.signal_wake_event();
    }

    pub(crate) fn cancel_subtree(&self, thread_ids: &[ThreadId]) {
        let mut state = self.lock_state();
        let mut pending = state
            .assignments
            .keys()
            .filter(|assignment| thread_ids.contains(&assignment.thread_id))
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return;
        }
        let mut subtree = Vec::new();
        while let Some(assignment) = pending.pop() {
            if subtree.contains(&assignment) {
                continue;
            }
            if let Some(record) = state.assignments.get(&assignment) {
                pending.extend(record.direct_children.iter().cloned());
            }
            subtree.push(assignment);
        }
        for assignment in subtree.into_iter().rev() {
            Self::release_assignment(&mut state, &assignment);
        }
        drop(state);
        self.signal_wake_event();
    }

    pub(crate) fn interrupt_idle_assignment(
        &self,
        id: &AgentAssignmentId,
        terminal_turn_id: &str,
    ) -> Result<bool, &'static str> {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&id.thread_id) != Some(id) {
            return Err("assignment is no longer current");
        }
        let assignment = state
            .assignments
            .get_mut(id)
            .ok_or("assignment no longer exists")?;
        if !matches!(
            assignment.phase,
            AssignmentPhase::Waiting | AssignmentPhase::Running
        ) {
            return Ok(false);
        }
        assignment.phase = AssignmentPhase::Interrupted;
        assignment.active_turn_id = None;
        assignment.terminal_turn_id = Some(terminal_turn_id.to_string());
        assignment.terminal_disposition = Some(TurnEndDisposition::Interrupted);
        assignment.last_classified_phase = Some(AssignmentPhase::Interrupted);
        state.wake_queue.remove_assignment(id);
        Self::detach_direct_children(&mut state, id);
        if state
            .assignments
            .get(id)
            .is_some_and(|assignment| assignment.parent.is_none())
        {
            Self::release_assignment(&mut state, id);
        }
        drop(state);
        self.signal_wake_event();
        Ok(true)
    }
}

pub(super) mod lifecycle;
pub(crate) use lifecycle::AssignmentReservation;
