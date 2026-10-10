//! Generation transitions, terminal classification, and reservation commit.

use super::super::ActiveTurnOrigin;
use super::super::AgentAssignmentId;
use super::super::AgentWakeCoordinator;
use super::super::Assignment;
use super::super::AssignmentPhase;
use super::super::AssignmentReloadAuthority;
use super::super::CoordinatorState;
use super::super::InterruptedChild;
use super::super::MAX_OUTSTANDING_ASSIGNMENTS;
use super::super::ReportDeliveryState;
use super::super::TurnEndClassification;
use super::super::TurnEndDisposition;
use crate::config::Config;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

impl AgentWakeCoordinator {
    /// Returns the active generation or starts a fresh generation after an explicit followup.
    pub(crate) fn begin_or_continue_assignment(
        &self,
        thread_id: ThreadId,
        parent: Option<AgentAssignmentId>,
        turn_id: impl Into<String>,
        allow_new_generation: bool,
    ) -> Result<AgentAssignmentId, &'static str> {
        let mut state = self.lock_state();
        let turn_id = turn_id.into();
        if let Some(current) = state.current_by_thread.get(&thread_id).cloned() {
            let phase = state
                .assignments
                .get(&current)
                .map(|assignment| assignment.phase)
                .ok_or("current assignment is missing")?;
            if phase.is_open() {
                if phase == AssignmentPhase::Reserved {
                    return Err("assignment reservation must commit before a turn starts");
                }
                let Some(assignment) = state.assignments.get_mut(&current) else {
                    return Err("current assignment is missing");
                };
                if assignment.parent != parent {
                    return Err("assignment belongs to a different parent generation");
                }
                if phase == AssignmentPhase::Waiting
                    && assignment.terminal_turn_id.as_deref() == Some(turn_id.as_str())
                {
                    return Err("waiting assignment requires a new turn ID");
                }
                if assignment.phase == AssignmentPhase::Running {
                    match assignment.active_turn_id.as_deref() {
                        Some(active_turn_id) if active_turn_id == turn_id => return Ok(current),
                        Some(_) => {
                            return Err("another turn is already active for this assignment");
                        }
                        None => {
                            assignment.active_turn_id = Some(turn_id);
                            assignment.active_turn_origin = ActiveTurnOrigin::Unheld;
                        }
                    }
                } else {
                    assignment.active_turn_id = Some(turn_id);
                    assignment.active_turn_origin = ActiveTurnOrigin::Waiting;
                }
                assignment.phase = AssignmentPhase::Running;
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
        if !allow_new_generation {
            return Err("assignment is missing; an explicit followup is required");
        }
        let assignment = Self::create_assignment(&mut state, thread_id, parent, turn_id)?;
        state.signal_wake_event();
        drop(state);
        Ok(assignment)
    }

    /// Moves a running assignment from a turn that lost its reserved slot before its task started
    /// to the turn that replaced it, so the replaced turn's later abort cannot end the assignment.
    /// The new turn holds it as the old one found it; if the new turn never starts either,
    /// `release_unstarted_turn` gives it back.
    pub(crate) fn hand_off_running_turn(
        &self,
        thread_id: ThreadId,
        from_turn_id: &str,
        to_turn_id: &str,
    ) {
        let mut state = self.lock_state();
        let Some(current) = state.current_by_thread.get(&thread_id).cloned() else {
            return;
        };
        if let Some(assignment) = state.assignments.get_mut(&current)
            && assignment.phase == AssignmentPhase::Running
            && assignment.active_turn_id.as_deref() == Some(from_turn_id)
        {
            assignment.active_turn_id = Some(to_turn_id.to_owned());
        }
    }

    /// Undoes the binding of a turn that held a running assignment but never started: the
    /// assignment returns to how the turn found it, so the next wake or input can take it. A turn
    /// that resumed a waiting assignment leaves it waiting again (and requests the wake its ready
    /// work or pending reports need), and one that took an unheld running assignment leaves it
    /// unheld. One that created the generation releases it, as a reservation that never commits is
    /// rolled back: no turn ran in it, so it leaves its parent's children and no report is owed.
    /// If the parent is already waiting on it, though, only a report can end that wait, so the
    /// generation is interrupted instead and returned for the caller to publish its report. Does
    /// nothing unless `turn_id` still holds the thread's running assignment.
    pub(crate) fn release_unstarted_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Option<InterruptedChild> {
        let mut state = self.lock_state();
        let current = state.current_by_thread.get(&thread_id).cloned()?;
        let assignment = state.assignments.get_mut(&current)?;
        if assignment.phase != AssignmentPhase::Running
            || assignment.active_turn_id.as_deref() != Some(turn_id)
        {
            return None;
        }
        match assignment.active_turn_origin {
            ActiveTurnOrigin::NewGeneration => {
                let interrupted = Self::interrupt_for_waiting_parent(&mut state, &current);
                if interrupted.is_none() {
                    Self::release_assignment(&mut state, &current);
                }
                state.signal_wake_event();
                drop(state);
                return interrupted;
            }
            ActiveTurnOrigin::Unheld => {
                assignment.active_turn_id = None;
                state.signal_wake_event();
            }
            ActiveTurnOrigin::Waiting => {
                assignment.active_turn_id = None;
                assignment.phase = AssignmentPhase::Waiting;
                let has_ready_completions = assignment
                    .owned_completions
                    .upgrade()
                    .is_some_and(|store| store.has_ready(thread_id, Some(&current)));
                // Any pending report needs the wake this turn would have been: one still in the
                // mailbox (Enqueued) is as unrecorded as one never claimed.
                let has_pending_reports = state
                    .pending_by_parent
                    .get(&current)
                    .is_some_and(|report_ids| !report_ids.is_empty());
                state.signal_wake_event();
                drop(state);
                if has_ready_completions || has_pending_reports {
                    self.request_wake(current);
                }
            }
        }
        None
    }

    /// Captures the recorded parent and root for a current child assignment reload.
    pub(crate) fn reload_authority(
        &self,
        id: &AgentAssignmentId,
    ) -> Option<AssignmentReloadAuthority> {
        let state = self.lock_state();
        if state.current_by_thread.get(&id.thread_id) != Some(id) {
            return None;
        }
        let child = state.assignments.get(id)?;
        if !child.phase.is_open() {
            return None;
        }
        let parent = child.parent.as_ref()?;
        let mut root = parent.clone();
        loop {
            if state.current_by_thread.get(&root.thread_id) != Some(&root) {
                return None;
            }
            let assignment = state.assignments.get(&root)?;
            if !assignment.phase.is_open() {
                return None;
            }
            match &assignment.parent {
                Some(parent) => root = parent.clone(),
                None => {
                    return Some(AssignmentReloadAuthority {
                        parent_thread_id: parent.thread_id,
                        root_thread_id: root.thread_id,
                    });
                }
            }
        }
    }

    /// Captures the last resident configuration needed to reload an open target assignment.
    pub(crate) fn store_reload_config(&self, id: &AgentAssignmentId, config: Config) -> bool {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&id.thread_id) != Some(id) {
            return false;
        }
        let Some(assignment) = state.assignments.get_mut(id) else {
            return false;
        };
        if !assignment.phase.is_open() {
            return false;
        }
        assignment.reload_config = Some(config);
        true
    }

    /// Returns the root-owned reload configuration for a current open child assignment.
    pub(crate) fn reload_config(&self, id: &AgentAssignmentId) -> Option<Config> {
        let state = self.lock_state();
        if state.current_by_thread.get(&id.thread_id) != Some(id) {
            return None;
        }
        state
            .assignments
            .get(id)
            .filter(|assignment| assignment.phase.is_open())
            .and_then(|assignment| assignment.reload_config.clone())
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
        if state.current_by_thread.get(&parent.thread_id) != Some(&parent)
            || !parent_assignment.phase.is_open()
        {
            return Err("parent assignment is no longer active");
        }
        if state.outstanding_assignments >= MAX_OUTSTANDING_ASSIGNMENTS {
            return Err("outstanding delegated assignment limit reached");
        }
        if state.current_by_thread.contains_key(&thread_id) {
            return Err("thread already has a current assignment");
        }
        let id = AgentAssignmentId {
            thread_id,
            generation: Uuid::now_v7(),
        };
        let (operation, completion) = super::super::GenerationOperation::new();
        let parent_record = state
            .assignments
            .get_mut(&parent)
            .ok_or("parent assignment is missing")?;
        parent_record
            .spawn_cleanups
            .retain(|cleanup| cleanup.completion.has_changed().is_ok());
        if parent_record.spawn_cleanups.len() >= MAX_OUTSTANDING_ASSIGNMENTS {
            return Err("in-flight child cleanup limit reached");
        }
        parent_record
            .spawn_cleanups
            .push(super::super::SpawnCleanup {
                thread_id,
                operation: Arc::downgrade(&operation),
                completion: completion.clone(),
            });
        parent_record.direct_children.insert(id.clone());
        state.assignments.insert(
            id.clone(),
            Assignment {
                owned_completions: Default::default(),
                parent: Some(parent.clone()),
                phase: AssignmentPhase::Reserved,
                direct_children: HashSet::new(),
                active_turn_id: None,
                active_turn_origin: ActiveTurnOrigin::Unheld,
                terminal_turn_id: None,
                terminal_disposition: None,
                last_classified_phase: None,
                terminal_report_id: None,
                counts_toward_limit: true,
                reload_config: None,
                spawn_cleanups: Vec::new(),
                in_flight: Some((Arc::downgrade(&operation), completion)),
            },
        );
        state.current_by_thread.insert(thread_id, id.clone());
        state.outstanding_assignments += 1;
        Ok(AssignmentReservation {
            coordinator: Arc::clone(self),
            id,
            committed: false,
            operation,
        })
    }

    pub(crate) fn classify_turn_end(
        &self,
        id: &AgentAssignmentId,
        terminal_turn_id: impl Into<String>,
        disposition: TurnEndDisposition,
    ) -> Result<AssignmentPhase, &'static str> {
        self.prepare_turn_end(id, terminal_turn_id, disposition, || {})
            .map(|classification| classification.phase)
    }

    pub(crate) fn prepare_turn_end(
        &self,
        id: &AgentAssignmentId,
        terminal_turn_id: impl Into<String>,
        disposition: TurnEndDisposition,
        pause_wakeups: impl FnOnce(),
    ) -> Result<TurnEndClassification, &'static str> {
        let mut state = self.lock_state();
        let terminal_turn_id = terminal_turn_id.into();
        if state.current_by_thread.get(&id.thread_id) != Some(id) {
            return Err("assignment is no longer current");
        }
        let assignment = state
            .assignments
            .get_mut(id)
            .ok_or("assignment is missing")?;
        if assignment.active_turn_id.as_deref() != Some(terminal_turn_id.as_str()) {
            if assignment.active_turn_id.is_some() {
                return Err("turn does not own the active assignment");
            }
            return if assignment.terminal_turn_id.as_deref() == Some(terminal_turn_id.as_str())
                && assignment.terminal_disposition == Some(disposition)
            {
                assignment
                    .last_classified_phase
                    .map(|phase| TurnEndClassification {
                        phase,
                        newly_classified: false,
                        cancelled_descendants: Vec::new(),
                    })
                    .ok_or("classified turn has no recorded phase")
            } else {
                Err("turn is not active for this assignment")
            };
        }
        if assignment.terminal_turn_id.as_deref() == Some(terminal_turn_id.as_str()) {
            return if assignment.terminal_disposition == Some(disposition) {
                assignment
                    .last_classified_phase
                    .map(|phase| TurnEndClassification {
                        phase,
                        newly_classified: false,
                        cancelled_descendants: Vec::new(),
                    })
                    .ok_or("classified turn has no recorded phase")
            } else {
                Err("turn already has a different terminal outcome")
            };
        }
        if assignment.phase.is_terminal() {
            return Err("assignment already has a different terminal outcome");
        }
        if assignment.phase == AssignmentPhase::Waiting {
            return Err("waiting assignment must start another turn before classification");
        }
        assignment.terminal_turn_id = Some(terminal_turn_id);
        assignment.terminal_disposition = Some(disposition);
        assignment.active_turn_id = None;
        assignment.phase = match disposition {
            TurnEndDisposition::Succeeded
                if !assignment.direct_children.is_empty()
                    || assignment
                        .owned_completions
                        .upgrade()
                        .is_some_and(|store| store.owns_assignment(id)) =>
            {
                AssignmentPhase::Waiting
            }
            TurnEndDisposition::Succeeded => AssignmentPhase::Completed,
            TurnEndDisposition::Errored => AssignmentPhase::Errored,
            TurnEndDisposition::Interrupted => AssignmentPhase::Interrupted,
            TurnEndDisposition::Cancelled => AssignmentPhase::Cancelled,
        };
        let phase = assignment.phase;
        assignment.last_classified_phase = Some(phase);
        let wake_recorded_reports = phase == AssignmentPhase::Waiting
            && state.pending_by_parent.get(id).is_some_and(|report_ids| {
                report_ids.iter().any(|report_id| {
                    state
                        .reports
                        .get(report_id)
                        .is_some_and(|report| report.delivery == ReportDeliveryState::Recorded)
                })
            });
        let mut cancelled_descendants = Vec::new();
        if phase.is_terminal() {
            state.wake_queue.remove_assignment(id);
            if phase == AssignmentPhase::Errored {
                pause_wakeups();
                let mut cleanups = state
                    .assignments
                    .get_mut(id)
                    .map(|record| std::mem::take(&mut record.spawn_cleanups))
                    .unwrap_or_default();
                let mut pending = state
                    .assignments
                    .get(id)
                    .map(|assignment| {
                        assignment
                            .direct_children
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                while let Some(child) = pending.pop() {
                    if let Some(record) = state.assignments.get_mut(&child) {
                        pending.extend(record.direct_children.iter().cloned());
                        cleanups.append(&mut record.spawn_cleanups);
                        cancelled_descendants.push((
                            child.thread_id,
                            record.in_flight.take().map(|(operation, completion)| {
                                if let Some(operation) = operation.upgrade() {
                                    operation.cancellation.cancel();
                                }
                                completion
                            }),
                        ));
                    }
                    Self::release_assignment(&mut state, &child);
                }
                for cleanup in cleanups {
                    if let Some(operation) = cleanup.operation.upgrade() {
                        operation.cancellation.cancel();
                    }
                    cancelled_descendants.push((cleanup.thread_id, Some(cleanup.completion)));
                }
            } else {
                Self::detach_direct_children(&mut state, id);
            }
            if state
                .assignments
                .get(id)
                .is_some_and(|assignment| assignment.parent.is_none())
            {
                Self::release_assignment(&mut state, id);
            }
        }
        if phase.is_terminal() {
            state.signal_wake_event();
        }
        drop(state);
        if wake_recorded_reports {
            self.request_wake(id.clone());
        }
        Ok(TurnEndClassification {
            phase,
            newly_classified: true,
            cancelled_descendants,
        })
    }

    fn create_assignment(
        state: &mut CoordinatorState,
        thread_id: ThreadId,
        parent: Option<AgentAssignmentId>,
        active_turn_id: String,
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
            thread_id,
            generation: Uuid::now_v7(),
        };
        if let Some(parent) = &parent {
            state
                .assignments
                .get_mut(parent)
                .ok_or("parent assignment is missing")?
                .direct_children
                .insert(id.clone());
        }
        state.assignments.insert(
            id.clone(),
            Assignment {
                owned_completions: Default::default(),
                parent: parent.clone(),
                phase: AssignmentPhase::Running,
                direct_children: HashSet::new(),
                active_turn_id: Some(active_turn_id),
                active_turn_origin: ActiveTurnOrigin::NewGeneration,
                terminal_turn_id: None,
                terminal_disposition: None,
                last_classified_phase: None,
                terminal_report_id: None,
                counts_toward_limit: parent.is_some(),
                reload_config: None,
                spawn_cleanups: Vec::new(),
                in_flight: None,
            },
        );
        if parent.is_some() {
            state.outstanding_assignments += 1;
        }
        state.current_by_thread.insert(thread_id, id.clone());
        Ok(id)
    }

    /// Marks `child` Interrupted under a fresh terminal turn id if its parent is the current,
    /// waiting assignment, as `interrupt_idle_assignment` does, and returns it for its report to be
    /// published. A waiting parent moves on only when a turn ends, and only a report can start
    /// that turn, so releasing the child instead would leave the parent waiting forever. The
    /// fresh id also keeps any later end of the child's own turn from reporting a second time.
    pub(in crate::agent::control::coordinator) fn interrupt_for_waiting_parent(
        state: &mut CoordinatorState,
        child: &AgentAssignmentId,
    ) -> Option<InterruptedChild> {
        let parent = state.assignments.get(child)?.parent.clone()?;
        let parent_is_waiting = state.current_by_thread.get(&parent.thread_id) == Some(&parent)
            && state
                .assignments
                .get(&parent)
                .is_some_and(|assignment| assignment.phase == AssignmentPhase::Waiting);
        if !parent_is_waiting {
            return None;
        }
        let terminal_turn_id = Uuid::now_v7().to_string();
        let assignment = state.assignments.get_mut(child)?;
        assignment.phase = AssignmentPhase::Interrupted;
        assignment.active_turn_id = None;
        assignment.terminal_turn_id = Some(terminal_turn_id.clone());
        assignment.terminal_disposition = Some(TurnEndDisposition::Interrupted);
        assignment.last_classified_phase = Some(AssignmentPhase::Interrupted);
        state.wake_queue.remove_assignment(child);
        Self::detach_direct_children(state, child);
        Some(InterruptedChild {
            assignment: child.clone(),
            terminal_turn_id,
        })
    }

    pub(super) fn detach_direct_children(state: &mut CoordinatorState, parent: &AgentAssignmentId) {
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

    pub(in crate::agent::control::coordinator) fn release_assignment(
        state: &mut CoordinatorState,
        child: &AgentAssignmentId,
    ) {
        let Some(assignment) = state.assignments.remove(child) else {
            return;
        };
        state.wake_queue.remove_assignment(child);
        if assignment.counts_toward_limit {
            state.outstanding_assignments = state.outstanding_assignments.saturating_sub(1);
        }
        if state.current_by_thread.get(&child.thread_id) == Some(child) {
            state.current_by_thread.remove(&child.thread_id);
        }
        let parent = assignment.parent;
        if let Some(parent) = &parent
            && let Some(parent_assignment) = state.assignments.get_mut(parent)
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

    pub(in crate::agent::control::coordinator) fn remove_pending_report(
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
        Self::release_assignment(&mut state, id);
        state.signal_wake_event();
    }
}

pub(crate) struct AssignmentReservation {
    coordinator: Arc<AgentWakeCoordinator>,
    id: AgentAssignmentId,
    committed: bool,
    operation: Arc<crate::agent::control::GenerationOperation>,
}

impl AssignmentReservation {
    pub(crate) fn operation(&self) -> Arc<crate::agent::control::GenerationOperation> {
        Arc::clone(&self.operation)
    }

    #[cfg(test)]
    pub(in crate::agent::control::coordinator) fn id(&self) -> &AgentAssignmentId {
        &self.id
    }

    pub(crate) fn commit(mut self) -> Result<AgentAssignmentId, &'static str> {
        let mut state = self.coordinator.lock_state();
        let assignment = state
            .assignments
            .get(&self.id)
            .ok_or("assignment reservation was invalidated")?;
        let Some(parent) = assignment.parent.as_ref() else {
            return Err("assignment reservation was detached");
        };
        if assignment.phase != AssignmentPhase::Reserved
            || state.current_by_thread.get(&self.id.thread_id) != Some(&self.id)
            || state.current_by_thread.get(&parent.thread_id) != Some(parent)
            || !state
                .assignments
                .get(parent)
                .is_some_and(|parent_assignment| parent_assignment.phase.is_open())
            || !state
                .assignments
                .get(parent)
                .is_some_and(|parent_assignment| {
                    parent_assignment.direct_children.contains(&self.id)
                })
        {
            return Err("assignment reservation was invalidated");
        }
        let Some(assignment) = state.assignments.get_mut(&self.id) else {
            return Err("assignment reservation was invalidated");
        };
        assignment.phase = AssignmentPhase::Running;
        state.signal_wake_event();
        self.committed = true;
        Ok(self.id.clone())
    }
}

impl Drop for AssignmentReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.coordinator.rollback_reservation(&self.id);
        }
    }
}
