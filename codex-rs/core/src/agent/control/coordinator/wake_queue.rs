use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use super::CoordinatorState;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::sync::Weak;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WakeDispatchResult {
    Complete,
    Defer,
}

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

fn is_current_and_wakeable(state: &CoordinatorState, assignment: &AgentAssignmentId) -> bool {
    state.current_by_thread.get(&assignment.thread_id) == Some(assignment)
        && state.assignments.get(assignment).is_some_and(|entry| {
            entry.phase == super::AssignmentPhase::Waiting
                || (entry.phase == super::AssignmentPhase::Running
                    && entry.active_turn_id.is_some())
        })
}

fn push_if_current_and_open(state: &mut CoordinatorState, assignment: AgentAssignmentId) -> bool {
    if !is_current_and_wakeable(state, &assignment) {
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
        let (queued, signal_event) = {
            let mut state = self.lock_state();
            if !is_current_and_wakeable(&state, &assignment) {
                (false, false)
            } else if state.wake_queue.in_flight.contains(&assignment) {
                let first_in_flight_request = state
                    .wake_queue
                    .requested_again
                    .insert(assignment);
                (false, first_in_flight_request)
            } else {
                let queued = push_if_current_and_open(&mut state, assignment);
                (queued, queued)
            }
        };
        if signal_event {
            self.signal_wake_event();
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

    /// Runs one fair attempt per queued target, then waits for a resource or report event.
    pub(crate) async fn run_wake_dispatcher<F, Fut>(
        self: Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<()>,
        mut dispatch: F,
    ) where
        F: FnMut(AgentAssignmentId) -> Fut + Send + 'static,
        Fut: Future<Output = WakeDispatchResult> + Send + 'static,
    {
        loop {
            let observed_epoch = self.wake_event_epoch();
            let pass_size = self.pending_wake_count();
            if pass_size == 0 {
                tokio::select! {
                    _ = self.wait_for_wake_event_after(observed_epoch) => {}
                    _ = shutdown.changed() => return,
                }
                continue;
            }

            for _ in 0..pass_size {
                let Some(request) = self.claim_next_wake_request() else {
                    break;
                };
                let Some(assignment) = request.assignment().cloned() else {
                    request.complete();
                    continue;
                };
                match dispatch(assignment).await {
                    WakeDispatchResult::Complete => request.complete(),
                    WakeDispatchResult::Defer => request.defer(),
                }
            }

            // Deferred requests stay queued but do not wake this worker. Registration above
            // preserves events that arrive while the bounded pass is running.
            tokio::select! {
                _ = self.wait_for_wake_event_after(observed_epoch) => {}
                _ = shutdown.changed() => return,
            }
        }
    }

    fn pending_wake_count(&self) -> usize {
        self.lock_state().wake_queue.pending.len()
    }

    fn next_wake_assignment(&self) -> Option<AgentAssignmentId> {
        let mut state = self.lock_state();
        while let Some(assignment) = state.wake_queue.pending.pop_front() {
            state.wake_queue.queued.remove(&assignment);
            if is_current_and_wakeable(&state, &assignment) {
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
            self.signal_wake_event();
        }
    }

    pub(crate) fn notify_capacity_available(&self) {
        self.signal_wake_event();
    }

    pub(crate) fn notify_active_turn_cleared(&self) {
        self.signal_wake_event();
    }

    pub(crate) fn notify_residency_available(&self) {
        self.signal_wake_event();
    }
}

/// A claimed wake returns to the queue if its async delivery task is cancelled.
pub(crate) struct WakeRequest {
    coordinator: Weak<AgentWakeCoordinator>,
    assignment: Option<AgentAssignmentId>,
}

impl WakeRequest {
    pub(crate) fn assignment(&self) -> Option<&AgentAssignmentId> {
        self.assignment.as_ref()
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
            coordinator.signal_wake_event();
        }
    }
}
