use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use super::AssignmentPhase;
use super::InterruptedChild;
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

    /// Retains a generation's in-flight startup/reload until its cleanup has completed.
    pub(crate) fn begin_generation_operation(
        &self,
        id: &AgentAssignmentId,
    ) -> Result<std::sync::Arc<crate::agent::control::GenerationOperation>, &'static str> {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&id.thread_id) != Some(id) {
            return Err("assignment is no longer current");
        }
        let record = state
            .assignments
            .get_mut(id)
            .ok_or("assignment is missing")?;
        if !record.phase.is_open() {
            return Err("assignment is terminal");
        }
        if let Some((operation, _)) = &record.in_flight
            && let Some(operation) = operation.upgrade()
        {
            return Ok(operation);
        }
        let (operation, completion) = super::GenerationOperation::new();
        record.in_flight = Some((std::sync::Arc::downgrade(&operation), completion));
        Ok(operation)
    }

    pub(crate) fn cancel_assignment(&self, id: &AgentAssignmentId) {
        let mut state = self.lock_state();
        Self::release_assignment(&mut state, id);
        state.signal_wake_event();
        drop(state);
    }

    /// Releases every generation of the closed threads and their descendants. A subtree root whose
    /// parent is outside the subtree is that parent's child, so the parent must still hear from
    /// it. A root that already has its result keeps it: a published report is kept for delivery,
    /// and a root whose turn ended but has not published yet is kept so that its own publish still
    /// finds it. An open root its parent is waiting on is interrupted instead and returned for its
    /// report to be published.
    pub(crate) fn cancel_subtree(&self, thread_ids: &[ThreadId]) -> Vec<InterruptedChild> {
        let mut state = self.lock_state();
        let mut pending = state
            .assignments
            .keys()
            .filter(|assignment| thread_ids.contains(&assignment.thread_id))
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Vec::new();
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
        let mut kept = Vec::new();
        let mut interrupted = Vec::new();
        for assignment in &subtree {
            let Some(record) = state.assignments.get(assignment) else {
                continue;
            };
            let Some(parent) = record
                .parent
                .as_ref()
                .filter(|parent| !thread_ids.contains(&parent.thread_id))
            else {
                continue;
            };
            let parent_is_current_and_open = state.current_by_thread.get(&parent.thread_id)
                == Some(parent)
                && state
                    .assignments
                    .get(parent)
                    .is_some_and(|parent| parent.phase.is_open());
            if !parent_is_current_and_open {
                continue;
            }
            if record.terminal_report_id.is_some() || record.phase.is_terminal() {
                kept.push(assignment.clone());
                continue;
            }
            // A reservation belongs to the parent's in-flight spawn, which rolls it back.
            if record.phase == AssignmentPhase::Reserved {
                continue;
            }
            if let Some(child) = Self::interrupt_for_waiting_parent(&mut state, assignment) {
                kept.push(assignment.clone());
                interrupted.push(child);
            }
        }
        for assignment in subtree.into_iter().rev() {
            if !kept.contains(&assignment) {
                Self::release_assignment(&mut state, &assignment);
            }
        }
        state.signal_wake_event();
        drop(state);
        interrupted
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
        state.signal_wake_event();
        drop(state);
        Ok(true)
    }
}

pub(super) mod lifecycle;
pub(crate) use lifecycle::AssignmentReservation;
