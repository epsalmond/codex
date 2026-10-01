//! Bounded, root-owned coordination state for delegated assignments and reports.

use crate::config::Config;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::MutexGuard;
use tokio::sync::Notify;
use uuid::Uuid;

mod assignments;
pub(crate) use assignments::AssignmentReservation;
mod reports;
pub(crate) use reports::TerminalReportPublication;
mod wake_queue;
pub(crate) use wake_queue::WakeDispatchResult;
use wake_queue::WakeQueue;

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
    Enqueued,
    Recorded,
}

struct Assignment {
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
}

struct TerminalReport {
    child: AgentAssignmentId,
    parent: AgentAssignmentId,
    terminal_turn_id: String,
    communication: InterAgentCommunication,
    delivery: ReportDeliveryState,
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
}

impl AgentWakeCoordinator {
    fn lock_state(&self) -> MutexGuard<'_, CoordinatorState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn signal_wake_event(&self) {
        self.wake_events.notify_one();
    }
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
