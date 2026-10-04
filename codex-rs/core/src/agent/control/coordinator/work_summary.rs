//! Lock-consistent counts of delegated work that can still produce root input.

use super::AgentWakeCoordinator;
use super::CoordinatorState;
use super::ReportDeliveryState;

/// Agent-tree work that keeps a root drain open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TreeWorkSummary {
    /// Reserved, running, or waiting assignments, including the root while it waits on children.
    pub(crate) open_assignments: usize,
    /// Terminal reports that have not yet been recorded in the parent's history.
    pub(crate) undelivered_reports: usize,
    /// Wake requests that are queued or being delivered.
    pub(crate) queued_wakes: usize,
}

impl CoordinatorState {
    pub(super) fn work_summary(&self) -> TreeWorkSummary {
        TreeWorkSummary {
            open_assignments: self
                .assignments
                .values()
                .filter(|assignment| assignment.phase.is_open())
                .count(),
            undelivered_reports: self
                .reports
                .values()
                .filter(|report| report.delivery != ReportDeliveryState::Recorded)
                .count(),
            queued_wakes: self.wake_queue.queued_count(),
        }
    }
}

impl AgentWakeCoordinator {
    /// Returns the summary and the event epoch read under one lock. Every summary change advances
    /// the epoch before the lock is released, so waiting for a newer epoch cannot miss one.
    pub(crate) fn work_summary(&self) -> (TreeWorkSummary, u64) {
        self.with_work_summary(|summary, epoch| (summary, epoch))
    }

    /// Runs `f` with the summary and epoch while still holding the lock, so no tree change can
    /// land before `f` returns. `f` must not call back into this coordinator.
    pub(crate) fn with_work_summary<R>(&self, f: impl FnOnce(TreeWorkSummary, u64) -> R) -> R {
        let state = self.lock_state();
        f(state.work_summary(), self.wake_event_epoch())
    }
}
