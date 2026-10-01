//! Bounded, root-owned coordination state for delegated assignments and reports.

use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use uuid::Uuid;

pub(crate) const MAX_OUTSTANDING_ASSIGNMENTS: usize = 1_024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AgentAssignmentId {
    pub(crate) thread_id: ThreadId,
    pub(crate) generation: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AssignmentPhase {
    Reserved,
    Running,
    Waiting,
    Completed,
    Errored,
    Interrupted,
    Cancelled,
}

impl AssignmentPhase {
    fn is_open(self) -> bool {
        matches!(self, Self::Reserved | Self::Running | Self::Waiting)
    }

    fn is_terminal(self) -> bool {
        !self.is_open()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnEndDisposition {
    Succeeded,
    Errored,
    Interrupted,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReportDeliveryState {
    PendingMailbox,
    Recorded,
}

struct Assignment {
    parent: Option<AgentAssignmentId>,
    phase: AssignmentPhase,
    direct_children: HashSet<AgentAssignmentId>,
    terminal_turn_id: Option<String>,
    terminal_report_id: Option<ResponseItemId>,
    counts_toward_limit: bool,
}

struct TerminalReport {
    child: AgentAssignmentId,
    parent: AgentAssignmentId,
    terminal_turn_id: String,
    communication: InterAgentCommunication,
    delivery: ReportDeliveryState,
}

#[derive(Default)]
struct CoordinatorState {
    current_by_thread: HashMap<ThreadId, AgentAssignmentId>,
    assignments: HashMap<AgentAssignmentId, Assignment>,
    reports: HashMap<ResponseItemId, TerminalReport>,
    pending_by_parent: HashMap<AgentAssignmentId, VecDeque<ResponseItemId>>,
    outstanding_assignments: usize,
}

/// The runtime shares this coordinator across every resident session in an agent tree.
#[derive(Default)]
pub(crate) struct AgentWakeCoordinator {
    state: Mutex<CoordinatorState>,
}

impl AgentWakeCoordinator {
    fn lock_state(&self) -> MutexGuard<'_, CoordinatorState> {
        self.state
            .lock()
            .expect("agent wake coordinator lock poisoned")
    }

    /// Returns the active generation or starts a fresh generation after an explicit followup.
    pub(crate) fn begin_or_continue_assignment(
        &self,
        thread_id: ThreadId,
        parent: Option<AgentAssignmentId>,
        allow_new_generation: bool,
    ) -> Result<AgentAssignmentId, &'static str> {
        let mut state = self.lock_state();
        if let Some(current) = state.current_by_thread.get(&thread_id).cloned() {
            let phase = state
                .assignments
                .get(&current)
                .map(|assignment| assignment.phase)
                .ok_or("current assignment is missing")?;
            if phase.is_open() {
                state
                    .assignments
                    .get_mut(&current)
                    .expect("current assignment checked above")
                    .phase = AssignmentPhase::Running;
                return Ok(current);
            }
            if !allow_new_generation {
                return Err("assignment is terminal; an explicit followup is required");
            }
            if state.assignments.get(&current).is_some_and(|assignment| {
                assignment.terminal_report_id.is_none() && assignment.direct_children.is_empty()
            }) {
                Self::release_assignment(&mut state, &current);
            }
        }
        Self::create_assignment(&mut state, thread_id, parent)
    }

    /// Reserves the bounded assignment and parent obligation before delegated work starts.
    pub(crate) fn reserve_child_assignment(
        self: &Arc<Self>,
        parent: AgentAssignmentId,
        thread_id: ThreadId,
    ) -> Result<AssignmentReservation, &'static str> {
        let mut state = self.lock_state();
        let parent_assignment = state
            .assignments
            .get(&parent)
            .ok_or("parent assignment is missing")?;
        if !parent_assignment.phase.is_open() {
            return Err("parent assignment is no longer active");
        }
        if state.outstanding_assignments >= MAX_OUTSTANDING_ASSIGNMENTS {
            return Err("outstanding delegated assignment limit reached");
        }
        if state.current_by_thread.contains_key(&thread_id) {
            return Err("thread already has a current assignment");
        }
        let id = AgentAssignmentId {
            thread_id: thread_id.clone(),
            generation: Uuid::now_v7(),
        };
        state.assignments.insert(
            id.clone(),
            Assignment {
                parent: Some(parent.clone()),
                phase: AssignmentPhase::Reserved,
                direct_children: HashSet::new(),
                terminal_turn_id: None,
                terminal_report_id: None,
                counts_toward_limit: true,
            },
        );
        state
            .assignments
            .get_mut(&parent)
            .expect("parent assignment checked above")
            .direct_children
            .insert(id.clone());
        state.current_by_thread.insert(thread_id, id.clone());
        state.outstanding_assignments += 1;
        Ok(AssignmentReservation {
            coordinator: Arc::clone(self),
            id,
            committed: false,
        })
    }

    pub(crate) fn classify_turn_end(
        &self,
        id: &AgentAssignmentId,
        terminal_turn_id: impl Into<String>,
        disposition: TurnEndDisposition,
    ) -> Result<AssignmentPhase, &'static str> {
        let mut state = self.lock_state();
        let assignment = state
            .assignments
            .get_mut(id)
            .ok_or("assignment is missing")?;
        assignment.terminal_turn_id = Some(terminal_turn_id.into());
        assignment.phase = match disposition {
            TurnEndDisposition::Succeeded if !assignment.direct_children.is_empty() => {
                AssignmentPhase::Waiting
            }
            TurnEndDisposition::Succeeded => AssignmentPhase::Completed,
            TurnEndDisposition::Errored => AssignmentPhase::Errored,
            TurnEndDisposition::Interrupted => AssignmentPhase::Interrupted,
            TurnEndDisposition::Cancelled => AssignmentPhase::Cancelled,
        };
        let phase = assignment.phase;
        if phase.is_terminal() {
            Self::detach_direct_children(&mut state, id);
            if state
                .assignments
                .get(id)
                .is_some_and(|assignment| assignment.parent.is_none())
            {
                Self::release_assignment(&mut state, id);
            }
        }
        Ok(phase)
    }

    /// Publishes a terminal report once and gives the persisted AgentMessage a stable ID.
    pub(crate) fn publish_terminal_report(
        &self,
        child: &AgentAssignmentId,
        terminal_turn_id: &str,
        mut communication: InterAgentCommunication,
    ) -> Option<InterAgentCommunication> {
        let mut state = self.lock_state();
        let child_assignment = state.assignments.get(child)?;
        if let Some(report_id) = &child_assignment.terminal_report_id {
            return state
                .reports
                .get(report_id)
                .filter(|report| report.terminal_turn_id == terminal_turn_id)
                .map(|report| report.communication.clone());
        }
        if !child_assignment.phase.is_terminal()
            || child_assignment.terminal_turn_id.as_deref() != Some(terminal_turn_id)
        {
            return None;
        }
        let parent = child_assignment.parent.clone()?;
        let parent_is_current_and_open = state.current_by_thread.get(&parent.thread_id)
            == Some(&parent)
            && state
                .assignments
                .get(&parent)
                .is_some_and(|assignment| assignment.phase.is_open());
        if !parent_is_current_and_open {
            Self::release_assignment(&mut state, child);
            return None;
        }

        let report_id = ResponseItemId::with_suffix("amsg", Uuid::now_v7());
        communication.id = Some(report_id.clone());
        let report = TerminalReport {
            child: child.clone(),
            parent: parent.clone(),
            terminal_turn_id: terminal_turn_id.to_string(),
            communication: communication.clone(),
            delivery: ReportDeliveryState::PendingMailbox,
        };
        state.reports.insert(report_id.clone(), report);
        state
            .pending_by_parent
            .entry(parent)
            .or_default()
            .push_back(report_id.clone());
        state
            .assignments
            .get_mut(child)
            .expect("child assignment checked above")
            .terminal_report_id = Some(report_id);
        Some(communication)
    }

    /// Returns mailbox reports without consuming them; only durable history recording advances state.
    pub(crate) fn pending_mailbox_reports(
        &self,
        parent: &AgentAssignmentId,
    ) -> Vec<InterAgentCommunication> {
        let state = self.lock_state();
        state
            .pending_by_parent
            .get(parent)
            .into_iter()
            .flatten()
            .filter_map(|id| state.reports.get(id))
            .filter(|report| report.delivery == ReportDeliveryState::PendingMailbox)
            .map(|report| report.communication.clone())
            .collect()
    }

    pub(crate) fn mark_report_recorded(&self, report_id: &ResponseItemId) -> bool {
        let mut state = self.lock_state();
        let Some(report) = state.reports.get_mut(report_id) else {
            return false;
        };
        report.delivery = ReportDeliveryState::Recorded;
        true
    }

    /// Captures only recorded reports whose stable IDs are present in this exact prompt snapshot.
    pub(crate) fn report_ids_in_prompt(
        &self,
        parent: &AgentAssignmentId,
        prompt_input: &[ResponseItem],
    ) -> Vec<ResponseItemId> {
        let prompt_ids = prompt_input
            .iter()
            .filter_map(ResponseItem::id)
            .collect::<HashSet<_>>();
        let state = self.lock_state();
        state
            .pending_by_parent
            .get(parent)
            .into_iter()
            .flatten()
            .filter(|id| prompt_ids.contains(id))
            .filter(|id| {
                state
                    .reports
                    .get(*id)
                    .is_some_and(|report| report.delivery == ReportDeliveryState::Recorded)
            })
            .cloned()
            .collect()
    }

    /// Consumes reports only after their IDs appeared in an accepted model request.
    pub(crate) fn accept_reports(
        &self,
        parent: &AgentAssignmentId,
        report_ids: &[ResponseItemId],
    ) -> usize {
        let mut state = self.lock_state();
        let mut accepted = 0;
        for report_id in report_ids {
            let Some(report) = state.reports.get(report_id) else {
                continue;
            };
            if &report.parent != parent || report.delivery != ReportDeliveryState::Recorded {
                continue;
            }
            let report = state
                .reports
                .remove(report_id)
                .expect("report checked above");
            Self::remove_pending_report(&mut state, &report.parent, report_id);
            if let Some(parent_assignment) = state.assignments.get_mut(parent) {
                parent_assignment.direct_children.remove(&report.child);
            }
            if state.current_by_thread.get(&report.child.thread_id) == Some(&report.child) {
                if let Some(child_assignment) = state.assignments.get_mut(&report.child) {
                    child_assignment.parent = None;
                    child_assignment.terminal_report_id = None;
                    child_assignment.counts_toward_limit = false;
                }
                state.outstanding_assignments = state.outstanding_assignments.saturating_sub(1);
            } else {
                Self::release_assignment(&mut state, &report.child);
            }
            accepted += 1;
        }
        accepted
    }

    fn create_assignment(
        state: &mut CoordinatorState,
        thread_id: ThreadId,
        parent: Option<AgentAssignmentId>,
    ) -> Result<AgentAssignmentId, &'static str> {
        if let Some(parent) = &parent {
            if state.outstanding_assignments >= MAX_OUTSTANDING_ASSIGNMENTS {
                return Err("outstanding delegated assignment limit reached");
            }
            if state.current_by_thread.get(&parent.thread_id) != Some(parent)
                || !state
                    .assignments
                    .get(parent)
                    .is_some_and(|assignment| assignment.phase.is_open())
            {
                return Err("parent assignment is no longer active");
            }
        }
        let id = AgentAssignmentId {
            thread_id: thread_id.clone(),
            generation: Uuid::now_v7(),
        };
        state.assignments.insert(
            id.clone(),
            Assignment {
                parent: parent.clone(),
                phase: AssignmentPhase::Running,
                direct_children: HashSet::new(),
                terminal_turn_id: None,
                terminal_report_id: None,
                counts_toward_limit: parent.is_some(),
            },
        );
        if let Some(parent) = parent {
            state
                .assignments
                .get_mut(&parent)
                .expect("parent assignment checked above")
                .direct_children
                .insert(id.clone());
            state.outstanding_assignments += 1;
        }
        state.current_by_thread.insert(thread_id, id.clone());
        Ok(id)
    }

    fn detach_direct_children(state: &mut CoordinatorState, parent: &AgentAssignmentId) {
        let children = state
            .assignments
            .get_mut(parent)
            .map(|assignment| std::mem::take(&mut assignment.direct_children))
            .unwrap_or_default();
        for child in children {
            if let Some(report_id) = state
                .assignments
                .get(&child)
                .and_then(|assignment| assignment.terminal_report_id.clone())
            {
                state.reports.remove(&report_id);
                Self::remove_pending_report(state, parent, &report_id);
            }
            let terminal = state
                .assignments
                .get(&child)
                .is_some_and(|assignment| assignment.phase.is_terminal());
            if terminal {
                Self::release_assignment(state, &child);
            } else if let Some(child_assignment) = state.assignments.get_mut(&child) {
                child_assignment.parent = None;
            }
        }
    }

    fn release_assignment(state: &mut CoordinatorState, child: &AgentAssignmentId) {
        let Some(assignment) = state.assignments.remove(child) else {
            return;
        };
        if assignment.counts_toward_limit {
            state.outstanding_assignments = state.outstanding_assignments.saturating_sub(1);
        }
        if state.current_by_thread.get(&child.thread_id) == Some(child) {
            state.current_by_thread.remove(&child.thread_id);
        }
        let parent = assignment.parent;
        if let Some(parent) = &parent
            && let Some(parent_assignment) = state.assignments.get_mut(&parent)
        {
            parent_assignment.direct_children.remove(child);
        }
        if let Some(report_id) = assignment.terminal_report_id {
            state.reports.remove(&report_id);
            if let Some(parent) = parent {
                Self::remove_pending_report(state, &parent, &report_id);
            }
        }
    }

    fn remove_pending_report(
        state: &mut CoordinatorState,
        parent: &AgentAssignmentId,
        report_id: &ResponseItemId,
    ) {
        if let Some(pending) = state.pending_by_parent.get_mut(parent) {
            pending.retain(|pending_id| pending_id != report_id);
            if pending.is_empty() {
                state.pending_by_parent.remove(parent);
            }
        }
    }

    fn rollback_reservation(&self, id: &AgentAssignmentId) {
        let mut state = self.lock_state();
        if state
            .assignments
            .get(id)
            .is_some_and(|assignment| assignment.phase == AssignmentPhase::Reserved)
        {
            Self::release_assignment(&mut state, id);
        }
    }
}

pub(crate) struct AssignmentReservation {
    coordinator: Arc<AgentWakeCoordinator>,
    id: AgentAssignmentId,
    committed: bool,
}

impl AssignmentReservation {
    pub(crate) fn commit(mut self) -> AgentAssignmentId {
        if let Some(assignment) = self.coordinator.lock_state().assignments.get_mut(&self.id) {
            assignment.phase = AssignmentPhase::Running;
        }
        self.committed = true;
        self.id.clone()
    }
}

impl Drop for AssignmentReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.coordinator.rollback_reservation(&self.id);
        }
    }
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
