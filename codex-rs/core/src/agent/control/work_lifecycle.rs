//! Root-turn tracking and guarded shutdown for one agent tree, layered on the wake coordinator.

use super::coordinator::AgentWakeCoordinator;
use super::coordinator::TreeWorkSummary;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_TRACKED_ROOT_TURNS: usize = 32;

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkAdmissionError {
    #[error("agent work coordinator is closed")]
    Closed,
    #[error("too many root turns are tracked")]
    CapacityReached,
}

/// A stable coordinator view used to subscribe to tree work and request guarded shutdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkObservationSnapshot {
    /// Opaque coordinator-incarnation and revision token. Pass it back unchanged to guard close.
    pub revision: String,
    /// Finite work that has not been fully delivered, including open agent assignments.
    pub outstanding_work: usize,
    /// Running finite work. Open agent assignments count here, including the root while it
    /// runs or waits on children.
    pub running_finite_work: usize,
    /// Undelivered child reports and queued agent wakes.
    pub pending_notifications: usize,
    pub active_root_turns: usize,
    pub pending_terminal_outputs: usize,
    pub output_forwarding_observed: bool,
    pub closed: bool,
    pub quiescent: bool,
}

/// Result of closing a tree against a specific observed revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardedShutdownOutcome {
    Closed(WorkObservationSnapshot),
    AlreadyClosed(WorkObservationSnapshot),
    ObservationInactive(WorkObservationSnapshot),
    StaleRevision(WorkObservationSnapshot),
    NotQuiescent(WorkObservationSnapshot),
}

/// Read-only capability for observing one tree and atomically closing it after drain.
///
/// Hosts must explicitly pass this capability to a waiter before it can claim that the tree is
/// quiescent. The receiver yields snapshots only; it cannot admit or mutate work.
#[derive(Clone)]
pub struct WorkObservation {
    coordinator: Arc<WorkCoordinator>,
}

impl WorkObservation {
    pub fn snapshot(&self) -> WorkObservationSnapshot {
        self.coordinator.observation_snapshot()
    }

    /// Subscribe without a gap between the returned snapshot and receiver registration.
    pub fn subscribe(
        &self,
    ) -> (
        WorkObservationSnapshot,
        watch::Receiver<WorkObservationSnapshot>,
    ) {
        self.coordinator.subscribe_snapshot()
    }

    /// Close only if the supplied observation is still current and the finite tree has drained.
    pub fn shutdown_if_quiescent(&self, expected_revision: &str) -> GuardedShutdownOutcome {
        self.coordinator.shutdown_if_quiescent(expected_revision)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootTurnState {
    Active,
    AwaitingOutput,
}

struct CoordinatorState {
    /// Advanced on every change to the fields below. The wake epoch covers agent-tree changes,
    /// so `(generation, epoch)` together change whenever the snapshot can change. A u64 bumped
    /// once per event cannot wrap back to an observed value within any real process lifetime.
    generation: u64,
    root_turns: HashMap<String, RootTurnState>,
    output_forwarding_observed: bool,
    forwarder_started: bool,
    closed: bool,
}

impl CoordinatorState {
    fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn snapshot(
        &self,
        incarnation: Uuid,
        tree: TreeWorkSummary,
        epoch: u64,
    ) -> WorkObservationSnapshot {
        let active_root_turns = self
            .root_turns
            .values()
            .filter(|state| **state == RootTurnState::Active)
            .count();
        let pending_terminal_outputs = self.root_turns.len() - active_root_turns;
        let running_finite_work = tree.open_assignments;
        let pending_notifications = tree.undelivered_reports + tree.queued_wakes;
        WorkObservationSnapshot {
            revision: format!("{incarnation}:{}:{epoch}", self.generation),
            outstanding_work: tree.open_assignments + tree.undelivered_reports,
            running_finite_work,
            pending_notifications,
            active_root_turns,
            pending_terminal_outputs,
            output_forwarding_observed: self.output_forwarding_observed,
            closed: self.closed,
            quiescent: self.output_forwarding_observed
                && running_finite_work == 0
                && pending_notifications == 0
                && active_root_turns == 0
                && pending_terminal_outputs == 0,
        }
    }
}

/// Tree-owned adapter: root-turn tracking, snapshot publication, and subscription registration
/// serialize through one lock. Agent assignments, reports, and wakes come from the tree's wake
/// coordinator, read under this lock (lock order: this coordinator, then the wake coordinator).
pub(crate) struct WorkCoordinator {
    state: Mutex<CoordinatorState>,
    updates: watch::Sender<WorkObservationSnapshot>,
    incarnation: Uuid,
    wake: Arc<AgentWakeCoordinator>,
    forwarder_stop: CancellationToken,
}

impl WorkCoordinator {
    pub(crate) fn new(wake: Arc<AgentWakeCoordinator>) -> Self {
        let incarnation = Uuid::new_v4();
        let state = CoordinatorState {
            generation: 0,
            root_turns: HashMap::new(),
            output_forwarding_observed: false,
            forwarder_started: false,
            closed: false,
        };
        let (tree, epoch) = wake.work_summary();
        let (updates, _receiver) = watch::channel(state.snapshot(incarnation, tree, epoch));
        Self {
            state: Mutex::new(state),
            updates,
            incarnation,
            wake,
            forwarder_stop: CancellationToken::new(),
        }
    }

    pub(crate) fn observation(self: &Arc<Self>) -> WorkObservation {
        WorkObservation {
            coordinator: Arc::clone(self),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, CoordinatorState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Snapshot of the locked state together with the wake epoch it reflects.
    fn snapshot_locked(&self, state: &CoordinatorState) -> (WorkObservationSnapshot, u64) {
        let (tree, epoch) = self.wake.work_summary();
        (state.snapshot(self.incarnation, tree, epoch), epoch)
    }

    fn observation_snapshot(&self) -> WorkObservationSnapshot {
        let state = self.lock_state();
        self.snapshot_locked(&state).0
    }

    /// Return a snapshot and receiver that cannot miss a concurrent coordinator update.
    fn subscribe_snapshot(
        self: &Arc<Self>,
    ) -> (
        WorkObservationSnapshot,
        watch::Receiver<WorkObservationSnapshot>,
    ) {
        let mut state = self.lock_state();
        if !state.output_forwarding_observed {
            state.advance_generation();
            state.output_forwarding_observed = true;
        }
        self.updates.send_replace(self.snapshot_locked(&state).0);
        if !state.forwarder_started {
            state.forwarder_started = true;
            self.spawn_forwarder();
        }
        let mut receiver = self.updates.subscribe();
        let snapshot = receiver.borrow_and_update().clone();
        (snapshot, receiver)
    }

    /// Republishes after each wake-coordinator event, so agent-tree changes reach subscribers.
    fn spawn_forwarder(self: &Arc<Self>) {
        let coordinator = Arc::downgrade(self);
        let wake = Arc::clone(&self.wake);
        let stop = self.forwarder_stop.clone();
        tokio::spawn(async move {
            while let Some(epoch) = Self::republish_from(&coordinator) {
                tokio::select! {
                    _ = wake.wait_for_wake_event_after(epoch) => {}
                    _ = stop.cancelled() => return,
                }
            }
        });
    }

    /// Returns the epoch the published snapshot reflects, or `None` once the scope is closed.
    fn republish_from(coordinator: &Weak<Self>) -> Option<u64> {
        let coordinator = coordinator.upgrade()?;
        let state = coordinator.lock_state();
        let (snapshot, epoch) = coordinator.snapshot_locked(&state);
        if state.closed {
            return None;
        }
        coordinator.updates.send_if_modified(|current| {
            if *current == snapshot {
                false
            } else {
                *current = snapshot;
                true
            }
        });
        Some(epoch)
    }

    pub(crate) fn root_turn_started(&self, turn_id: &str) -> Result<(), WorkAdmissionError> {
        self.update(|state| {
            if state.closed {
                return Err(WorkAdmissionError::Closed);
            }
            if state.root_turns.contains_key(turn_id) {
                return Ok(());
            }
            if state.root_turns.len() >= MAX_TRACKED_ROOT_TURNS {
                return Err(WorkAdmissionError::CapacityReached);
            }
            state.advance_generation();
            state
                .root_turns
                .insert(turn_id.to_string(), RootTurnState::Active);
            Ok(())
        })
    }

    pub(crate) fn root_turn_terminal(&self, turn_id: &str) {
        self.update(|state| {
            if state.root_turns.get(turn_id) == Some(&RootTurnState::Active) {
                state.advance_generation();
                if state.output_forwarding_observed {
                    state
                        .root_turns
                        .insert(turn_id.to_string(), RootTurnState::AwaitingOutput);
                } else {
                    state.root_turns.remove(turn_id);
                }
            }
        })
    }

    pub(crate) fn root_turn_output_forwarded(&self, turn_id: &str) {
        self.update(|state| {
            if state.root_turns.get(turn_id) == Some(&RootTurnState::AwaitingOutput) {
                state.advance_generation();
                state.root_turns.remove(turn_id);
            }
        })
    }

    /// Forgets a root turn that was admitted but never started, so it emits no terminal event.
    pub(crate) fn root_turn_abandoned(&self, turn_id: &str) {
        self.update(|state| {
            if state.root_turns.get(turn_id) == Some(&RootTurnState::Active) {
                state.advance_generation();
                state.root_turns.remove(turn_id);
            }
        })
    }

    /// Holds both locks from the revision check through `closed = true`, so no root-turn or
    /// agent-tree change can land between deciding the tree is drained and closing it.
    fn shutdown_if_quiescent(&self, expected_revision: &str) -> GuardedShutdownOutcome {
        let mut state = self.lock_state();
        self.wake.with_work_summary(|tree, epoch| {
            let snapshot = state.snapshot(self.incarnation, tree, epoch);
            if !snapshot.output_forwarding_observed {
                return GuardedShutdownOutcome::ObservationInactive(snapshot);
            }
            if snapshot.closed {
                return GuardedShutdownOutcome::AlreadyClosed(snapshot);
            }
            if snapshot.revision != expected_revision {
                return GuardedShutdownOutcome::StaleRevision(snapshot);
            }
            if !snapshot.quiescent {
                return GuardedShutdownOutcome::NotQuiescent(snapshot);
            }
            state.advance_generation();
            state.closed = true;
            let snapshot = state.snapshot(self.incarnation, tree, epoch);
            self.updates.send_replace(snapshot.clone());
            GuardedShutdownOutcome::Closed(snapshot)
        })
    }

    fn update<R>(&self, update: impl FnOnce(&mut CoordinatorState) -> R) -> R {
        let mut state = self.lock_state();
        let (before, _) = self.snapshot_locked(&state);
        let result = update(&mut state);
        let (after, _) = self.snapshot_locked(&state);
        if after != before {
            self.updates.send_replace(after);
        }
        result
    }
}

impl Drop for WorkCoordinator {
    fn drop(&mut self) {
        self.forwarder_stop.cancel();
    }
}

#[cfg(test)]
#[path = "work_lifecycle_tests.rs"]
mod tests;
