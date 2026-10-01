use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use super::CoordinatorState;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;

#[derive(Default)]
pub(super) struct WakeQueue {
    pending: VecDeque<AgentAssignmentId>,
    queued: HashSet<AgentAssignmentId>,
    in_flight: HashSet<AgentAssignmentId>,
    requested_again: HashSet<AgentAssignmentId>,
}

impl WakeQueue {
    pub(super) fn remove_assignment(&mut self, assignment: &AgentAssignmentId) {
        self.pending.retain(|queued| queued != assignment);
        self.queued.remove(assignment);
        self.in_flight.remove(assignment);
        self.requested_again.remove(assignment);
    }
}

fn is_current_and_open(state: &CoordinatorState, assignment: &AgentAssignmentId) -> bool {
    state.current_by_thread.get(&assignment.thread_id) == Some(assignment)
        && state
            .assignments
            .get(assignment)
            .is_some_and(|entry| entry.phase.is_open())
}

fn push_if_current_and_open(state: &mut CoordinatorState, assignment: AgentAssignmentId) -> bool {
    if !is_current_and_open(state, &assignment) {
        return false;
    }
    if state.wake_queue.in_flight.contains(&assignment) {
        state.wake_queue.requested_again.insert(assignment);
        return false;
    }
    if !state.wake_queue.queued.insert(assignment.clone()) {
        return false;
    }
    state.wake_queue.pending.push_back(assignment);
    true
}

impl AgentWakeCoordinator {
    /// Coalesces wake requests by current assignment generation and signals the event loop.
    pub(crate) fn request_wake(&self, assignment: AgentAssignmentId) -> bool {
        let queued = {
            let mut state = self.lock_state();
            push_if_current_and_open(&mut state, assignment)
        };
        if queued {
            self.wake_events.notify_one();
        }
        queued
    }

    /// Claims the oldest still-current wake request for a fair delivery attempt.
    pub(crate) fn claim_next_wake_request(self: &Arc<Self>) -> Option<WakeRequest> {
        let assignment = self.next_wake_assignment()?;
        Some(WakeRequest {
            coordinator: Arc::downgrade(self),
            assignment: Some(assignment),
        })
    }

    fn next_wake_assignment(&self) -> Option<AgentAssignmentId> {
        let mut state = self.lock_state();
        while let Some(assignment) = state.wake_queue.pending.pop_front() {
            state.wake_queue.queued.remove(&assignment);
            if is_current_and_open(&state, &assignment) {
                state.wake_queue.in_flight.insert(assignment.clone());
                return Some(assignment);
            }
        }
        None
    }

    /// Returns a temporarily blocked request to the end of the queue without a busy wake.
    pub(crate) fn defer_wake_request(&self, assignment: AgentAssignmentId) {
        let mut state = self.lock_state();
        state.wake_queue.in_flight.remove(&assignment);
        state.wake_queue.requested_again.remove(&assignment);
        push_if_current_and_open(&mut state, assignment);
    }

    /// Completes a delivery attempt and queues one more pass for reports that arrived in flight.
    pub(crate) fn complete_wake_request(&self, assignment: &AgentAssignmentId) {
        let queued = {
            let mut state = self.lock_state();
            state.wake_queue.in_flight.remove(assignment);
            let requested_again = state.wake_queue.requested_again.remove(assignment);
            requested_again && push_if_current_and_open(&mut state, assignment.clone())
        };
        if queued {
            self.wake_events.notify_one();
        }
    }

    /// Sleeps until a report or a resource/lifecycle event may allow queued work to progress.
    pub(crate) async fn wait_for_wake_event(&self) {
        self.wake_events.notified().await;
    }

    pub(crate) fn notify_capacity_available(&self) {
        self.wake_events.notify_one();
    }

    pub(crate) fn notify_active_turn_cleared(&self) {
        self.wake_events.notify_one();
    }

    pub(crate) fn notify_residency_available(&self) {
        self.wake_events.notify_one();
    }
}

/// A claimed wake returns to the queue if its async delivery task is cancelled.
pub(crate) struct WakeRequest {
    coordinator: Weak<AgentWakeCoordinator>,
    assignment: Option<AgentAssignmentId>,
}

impl WakeRequest {
    pub(crate) fn assignment(&self) -> &AgentAssignmentId {
        self.assignment
            .as_ref()
            .expect("wake request is active until completed or deferred")
    }

    pub(crate) fn complete(mut self) {
        let Some(assignment) = self.assignment.take() else {
            return;
        };
        if let Some(coordinator) = self.coordinator.upgrade() {
            coordinator.complete_wake_request(&assignment);
        }
    }

    pub(crate) fn defer(mut self) {
        let Some(assignment) = self.assignment.take() else {
            return;
        };
        if let Some(coordinator) = self.coordinator.upgrade() {
            coordinator.defer_wake_request(assignment);
        }
    }
}

impl Drop for WakeRequest {
    fn drop(&mut self) {
        let Some(assignment) = self.assignment.take() else {
            return;
        };
        if let Some(coordinator) = self.coordinator.upgrade() {
            coordinator.defer_wake_request(assignment);
            coordinator.wake_events.notify_one();
        }
    }
}
