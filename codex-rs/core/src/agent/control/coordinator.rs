//! Bounded, root-owned coordination state for delegated assignments and reports.

use crate::config::Config;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use uuid::Uuid;

mod assignments;
mod owned_work;
pub(crate) use assignments::AssignmentReservation;
mod reports;
pub(crate) use reports::ClaimedTerminalReport;
pub(crate) use reports::TerminalReportDeliveryGuard;
pub(crate) use reports::TerminalReportPublication;
mod wake_queue;
pub(crate) use wake_queue::WakeDispatchResult;
use wake_queue::WakeQueue;
mod work_summary;
pub(crate) use work_summary::TreeWorkSummary;

pub(crate) const MAX_OUTSTANDING_ASSIGNMENTS: usize = 1_024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AgentAssignmentId {
    pub(crate) thread_id: ThreadId,
    pub(crate) generation: Uuid,
}

pub(crate) struct AssignmentReloadAuthority {
    pub(crate) parent_thread_id: ThreadId,
    pub(crate) root_thread_id: ThreadId,
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

/// Shared cancellation and completion barrier for one assignment generation's operations.
pub(crate) struct GenerationOperation {
    cancellation: tokio_util::sync::CancellationToken,
    _completion: tokio::sync::watch::Sender<()>,
}

impl GenerationOperation {
    fn new() -> (Arc<Self>, tokio::sync::watch::Receiver<()>) {
        let (completion, receiver) = tokio::sync::watch::channel(());
        (
            Arc::new(Self {
                cancellation: tokio_util::sync::CancellationToken::new(),
                _completion: completion,
            }),
            receiver,
        )
    }

    pub(crate) async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
}

#[derive(Clone)]
struct SpawnCleanup {
    thread_id: ThreadId,
    operation: std::sync::Weak<GenerationOperation>,
    completion: tokio::sync::watch::Receiver<()>,
}

pub(crate) struct TurnEndClassification {
    pub(crate) phase: AssignmentPhase,
    pub(crate) newly_classified: bool,
    pub(crate) cancelled_descendants: Vec<(ThreadId, Option<tokio::sync::watch::Receiver<()>>)>,
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
    Claimed,
    Enqueued,
    Recorded,
}

struct Assignment {
    owned_completions: std::sync::Weak<crate::session::async_completion::AsyncCompletions>,
    parent: Option<AgentAssignmentId>,
    phase: AssignmentPhase,
    direct_children: HashSet<AgentAssignmentId>,
    active_turn_id: Option<String>,
    terminal_turn_id: Option<String>,
    terminal_disposition: Option<TurnEndDisposition>,
    last_classified_phase: Option<AssignmentPhase>,
    terminal_report_id: Option<ResponseItemId>,
    counts_toward_limit: bool,
    reload_config: Option<Config>,
    spawn_cleanups: Vec<SpawnCleanup>,
    in_flight: Option<(
        std::sync::Weak<crate::agent::control::GenerationOperation>,
        tokio::sync::watch::Receiver<()>,
    )>,
}

struct TerminalReport {
    child: AgentAssignmentId,
    parent: AgentAssignmentId,
    terminal_turn_id: String,
    communication: InterAgentCommunication,
    delivery: ReportDeliveryState,
    mailbox_inserted: bool,
}

#[derive(Default)]
pub(super) struct CoordinatorState {
    current_by_thread: HashMap<ThreadId, AgentAssignmentId>,
    assignments: HashMap<AgentAssignmentId, Assignment>,
    reports: HashMap<ResponseItemId, TerminalReport>,
    pending_by_parent: HashMap<AgentAssignmentId, VecDeque<ResponseItemId>>,
    wake_queue: WakeQueue,
    outstanding_assignments: usize,
}

/// The runtime shares this coordinator across every resident session in an agent tree.
#[derive(Default)]
pub(crate) struct AgentWakeCoordinator {
    state: Mutex<CoordinatorState>,
    wake_events: Notify,
    wake_event_epoch: AtomicU64,
}

impl AgentWakeCoordinator {
    fn lock_state(&self) -> CoordinatorGuard<'_> {
        CoordinatorGuard {
            coordinator: self,
            state: self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            summary_before_mutation: None,
        }
    }

    pub(super) fn wake_event_epoch(&self) -> u64 {
        self.wake_event_epoch.load(Ordering::Acquire)
    }

    /// Advances the event epoch under the state lock so a reader holding the lock sees an epoch
    /// that matches the state it reads.
    fn signal_wake_event_locked(&self, _state: &CoordinatorState) {
        self.wake_event_epoch.fetch_add(1, Ordering::AcqRel);
        self.wake_events.notify_waiters();
    }

    pub(super) fn signal_wake_event(&self) {
        let state = self.lock_state();
        self.signal_wake_event_locked(&state);
    }

    pub(super) async fn wait_for_wake_event_after(&self, observed_epoch: u64) {
        loop {
            let notified = self.wake_events.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.wake_event_epoch() != observed_epoch {
                return;
            }
            notified.await;
            if self.wake_event_epoch() != observed_epoch {
                return;
            }
        }
    }
}

/// Holds the coordinator lock. The first mutable access records the tree work summary, and
/// dropping the guard advances the event epoch, still under the lock, if that summary changed.
/// Every summary change is therefore visible to `wait_for_wake_event_after` waiters.
pub(super) struct CoordinatorGuard<'a> {
    coordinator: &'a AgentWakeCoordinator,
    state: MutexGuard<'a, CoordinatorState>,
    summary_before_mutation: Option<TreeWorkSummary>,
}

impl CoordinatorGuard<'_> {
    /// Advances the event epoch for a change that the work summary does not capture.
    pub(super) fn signal_wake_event(&self) {
        self.coordinator.signal_wake_event_locked(&self.state);
    }
}

impl Deref for CoordinatorGuard<'_> {
    type Target = CoordinatorState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for CoordinatorGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        if self.summary_before_mutation.is_none() {
            self.summary_before_mutation = Some(self.state.work_summary());
        }
        &mut self.state
    }
}

impl Drop for CoordinatorGuard<'_> {
    fn drop(&mut self) {
        if let Some(before) = self.summary_before_mutation.take()
            && before != self.state.work_summary()
        {
            self.coordinator.signal_wake_event_locked(&self.state);
        }
    }
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
