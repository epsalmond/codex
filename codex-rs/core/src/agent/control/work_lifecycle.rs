//! Root-turn tracking and guarded shutdown for one agent tree, layered on the wake coordinator.

use super::coordinator::AgentAssignmentId;
use super::coordinator::AgentWakeCoordinator;
use super::coordinator::TreeWorkSummary;
use codex_protocol::ThreadId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_TRACKED_ROOT_TURNS: usize = 32;
const MAX_OWNED_REGISTRATIONS: usize = 64;
const MAX_OWNED_SERVICES: usize = 16;

/// Finite commands and cells hold drain; services remain owned but have zero drain weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkKind {
    Task,
    Cell,
    Service,
}

impl WorkKind {
    pub(crate) fn is_finite(self) -> bool {
        self != Self::Service
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkIdentity {
    owner: ThreadId,
    turn_id: String,
    call_id: String,
    assignment: Option<AgentAssignmentId>,
    kind: WorkKind,
}

struct ProducerRegistration {
    identity: WorkIdentity,
    terminal: bool,
    started: bool,
    yielded_nonautomatic: bool,
    cell_authority: bool,
}

struct RegistrationHandle {
    id: Uuid,
    identity: WorkIdentity,
    coordinator: Weak<WorkCoordinator>,
}

/// One authoritative producer registration shared by its native owner and existing result record.
/// No output, Session or coordinator is retained by this handle.
#[derive(Clone)]
pub(crate) struct OwnedWorkRegistration(Arc<RegistrationHandle>);

impl std::fmt::Debug for OwnedWorkRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedWorkRegistration")
            .field("id", &self.0.id)
            .field("identity", &self.0.identity)
            .finish()
    }
}

impl OwnedWorkRegistration {
    pub(crate) fn id(&self) -> Uuid {
        self.0.id
    }

    pub(crate) fn kind(&self) -> WorkKind {
        self.0.identity.kind
    }

    pub(crate) fn matches(
        &self,
        owner: ThreadId,
        turn_id: &str,
        call_id: &str,
        assignment: Option<&AgentAssignmentId>,
    ) -> bool {
        let identity = &self.0.identity;
        identity.owner == owner
            && identity.turn_id == turn_id
            && identity.call_id == call_id
            && identity.assignment.as_ref() == assignment
    }

    fn update(&self, update: impl FnOnce(&mut ProducerRegistration)) {
        if let Some(coordinator) = self.0.coordinator.upgrade() {
            coordinator.update(|state| {
                if let Some(record) = state.producers.get_mut(&self.0.id) {
                    update(record);
                    state.advance_generation();
                }
            });
        }
    }

    /// Call only after native launch succeeds. Failed startup and inline results never hint.
    pub(crate) fn mark_started(&self) {
        self.update(|record| record.started = true);
    }

    /// The producer returned a live handle; automatic records already supply wake authority.
    pub(crate) fn mark_yielded(&self, automatic: bool) {
        self.update(|record| record.yielded_nonautomatic = !automatic);
    }

    /// Service cells also need retained dispatch authority; native processes never call this.
    pub(crate) fn mark_cell(&self) {
        self.update(|record| record.cell_authority = true);
    }

    /// Mark native close, retaining the hold until event/output processing and acceptance finish.
    pub(crate) fn mark_terminal(&self) {
        self.update(|record| record.terminal = true);
    }

    pub(crate) fn admit_completion<T>(
        &self,
        store: &Arc<crate::session::async_completion::AsyncCompletions>,
        admit: impl FnOnce() -> Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        let coordinator = self.0.coordinator.upgrade().ok_or("work owner is closed")?;
        let state = coordinator.lock_state();
        if state.closed || !state.producers.contains_key(&self.0.id) {
            return Err("work owner is closed");
        }
        if let Some(assignment) = &self.0.identity.assignment {
            coordinator
                .wake
                .admit_registered_completion(assignment, store, admit)
        } else {
            admit()
        }
    }
}

impl Drop for RegistrationHandle {
    fn drop(&mut self) {
        if let Some(coordinator) = self.coordinator.upgrade() {
            coordinator.release_producer(self.id);
        }
    }
}

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
    /// Running finite work. Open assignments and running producer registrations are additive
    /// units, including the root while it runs or waits on children.
    pub running_finite_work: usize,
    /// Undelivered child reports, queued agent wakes and terminal finite registrations.
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
    producers: HashMap<Uuid, ProducerRegistration>,
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
        let finite = self
            .producers
            .values()
            .filter(|record| record.identity.kind.is_finite());
        let running_producers = finite.clone().filter(|record| !record.terminal).count();
        let terminal_producers = finite.filter(|record| record.terminal).count();
        // Assignment and producer counts are additive units, not distinct sessions.
        let running_finite_work = tree.open_assignments + running_producers;
        let pending_notifications =
            tree.undelivered_reports + tree.queued_wakes + terminal_producers;
        WorkObservationSnapshot {
            revision: format!("{incarnation}:{}:{epoch}", self.generation),
            outstanding_work: tree.open_assignments
                + tree.undelivered_reports
                + running_producers
                + terminal_producers,
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
            producers: HashMap::new(),
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

    pub(crate) fn reserve_producer(
        self: &Arc<Self>,
        owner: ThreadId,
        turn_id: &str,
        call_id: &str,
        assignment: Option<&AgentAssignmentId>,
        kind: WorkKind,
        retained_cell: Option<&OwnedWorkRegistration>,
    ) -> Result<OwnedWorkRegistration, &'static str> {
        if turn_id.is_empty()
            || turn_id.len() > 128
            || call_id.len() > 128
            || assignment.is_some_and(|assignment| assignment.thread_id != owner)
        {
            return Err("work registration identifiers do not match their bounded owner");
        }
        self.update(|state| {
            if state.closed {
                return Err("agent work coordinator is closed");
            }
            let owned = state
                .producers
                .values()
                .filter(|record| record.identity.owner == owner);
            if owned.clone().count() >= MAX_OWNED_REGISTRATIONS
                || (kind == WorkKind::Service
                    && owned
                        .filter(|record| record.identity.kind == WorkKind::Service)
                        .count()
                        >= MAX_OWNED_SERVICES)
            {
                return Err("owned work registration capacity exhausted");
            }
            let retained = retained_cell.is_some_and(|cell| {
                cell.0.coordinator.ptr_eq(&Arc::downgrade(self))
                    && state.producers.get(&cell.id()).is_some_and(|record| {
                        record.cell_authority
                            && record.started
                            && !record.terminal
                            && record.identity.owner == owner
                            && record.identity.turn_id == turn_id
                            && record.identity.assignment.as_ref() == assignment
                    })
            });
            if retained_cell.is_some() && !retained {
                return Err("originating cell no longer authorizes this producer");
            }
            if let Some(assignment) = assignment {
                self.wake
                    .admit_finite_producer(assignment, turn_id, retained, kind.is_finite())?;
            }
            let id = Uuid::now_v7();
            let identity = WorkIdentity {
                owner,
                turn_id: turn_id.to_owned(),
                call_id: call_id.to_owned(),
                assignment: assignment.cloned(),
                kind,
            };
            state.producers.insert(
                id,
                ProducerRegistration {
                    identity: identity.clone(),
                    terminal: false,
                    started: false,
                    yielded_nonautomatic: false,
                    cell_authority: kind == WorkKind::Cell,
                },
            );
            state.advance_generation();
            Ok(OwnedWorkRegistration(Arc::new(RegistrationHandle {
                id,
                identity,
                coordinator: Arc::downgrade(self),
            })))
        })
    }

    fn release_producer(&self, id: Uuid) {
        let wake_assignment = self.update(|state| {
            let record = state.producers.remove(&id)?;
            state.advance_generation();
            let assignment = record.identity.assignment?;
            if record.identity.kind.is_finite() {
                self.wake
                    .release_finite_producer(
                        &assignment,
                        record.terminal && record.started && record.yielded_nonautomatic,
                    )
                    .then_some(assignment)
            } else {
                None
            }
        });
        // No lifecycle lock is held while requesting the existing scheduler.
        if let Some(assignment) = wake_assignment {
            self.wake.request_wake(assignment);
        }
    }

    pub(crate) fn has_live_registrations(&self, owner: ThreadId) -> bool {
        self.lock_state()
            .producers
            .values()
            .any(|record| record.identity.owner == owner)
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
