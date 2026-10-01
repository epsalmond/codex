use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use super::ReportDeliveryState;
use super::TerminalReport;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use std::collections::HashSet;
use std::sync::Weak;
use uuid::Uuid;

pub(crate) struct ClaimedTerminalReport {
    coordinator: Weak<AgentWakeCoordinator>,
    pub(crate) id: ResponseItemId,
    pub(crate) sender_thread_id: codex_protocol::ThreadId,
    pub(crate) communication: InterAgentCommunication,
    enqueued: bool,
}

/// Keeps a coordinator mailbox claim retryable while a session handler awaits queue admission.
pub(crate) struct TerminalReportDeliveryGuard {
    coordinator: Weak<AgentWakeCoordinator>,
    id: ResponseItemId,
    committed: bool,
}

impl TerminalReportDeliveryGuard {
    pub(crate) fn acknowledge(&self, parent_thread_id: codex_protocol::ThreadId) -> bool {
        self.coordinator.upgrade().is_some_and(|coordinator| {
            coordinator.acknowledge_report_mailbox_delivery(&self.id, parent_thread_id)
        })
    }

    pub(crate) fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for TerminalReportDeliveryGuard {
    fn drop(&mut self) {
        if !self.committed
            && let Some(coordinator) = self.coordinator.upgrade()
        {
            coordinator.release_mailbox_claim_for_retry(&self.id);
        }
    }
}

impl ClaimedTerminalReport {
    pub(crate) fn mark_enqueued(mut self) -> bool {
        let marked = self
            .coordinator
            .upgrade()
            .is_some_and(|coordinator| coordinator.mark_report_enqueued(&self.id));
        self.enqueued = marked;
        marked
    }
}

impl Drop for ClaimedTerminalReport {
    fn drop(&mut self) {
        if !self.enqueued
            && let Some(coordinator) = self.coordinator.upgrade()
        {
            coordinator.release_mailbox_claim(&self.id);
        }
    }
}

pub(crate) enum TerminalReportPublication {
    Published(ResponseItemId),
    AlreadyPublished(ResponseItemId),
}

impl AgentWakeCoordinator {
    pub(crate) fn delivery_guard(
        self: &std::sync::Arc<Self>,
        id: ResponseItemId,
    ) -> TerminalReportDeliveryGuard {
        TerminalReportDeliveryGuard {
            coordinator: std::sync::Arc::downgrade(self),
            id,
            committed: false,
        }
    }

    /// Rechecks pending mailbox work when an assignment becomes eligible to receive a wake.
    pub(crate) fn request_wake_for_pending_mailbox_reports(
        &self,
        parent: &AgentAssignmentId,
    ) -> bool {
        let has_pending_mailbox_reports = {
            let state = self.lock_state();
            state
                .pending_by_parent
                .get(parent)
                .into_iter()
                .flatten()
                .any(|id| {
                    state.reports.get(id).is_some_and(|report| {
                        report.delivery == ReportDeliveryState::PendingMailbox
                    })
                })
        };
        has_pending_mailbox_reports && self.request_wake(parent.clone())
    }

    /// Publishes a terminal report once and returns only its stable ID; payload delivery must use a claim.
    pub(crate) fn publish_terminal_report(
        &self,
        child: &AgentAssignmentId,
        terminal_turn_id: &str,
        mut communication: InterAgentCommunication,
    ) -> Option<TerminalReportPublication> {
        let publication = {
            let mut state = self.lock_state();
            (|| {
                let child_assignment = state.assignments.get(child)?;
                if let Some(report_id) = &child_assignment.terminal_report_id {
                    return state
                        .reports
                        .get(report_id)
                        .filter(|report| report.terminal_turn_id == terminal_turn_id)
                        .map(|report| {
                            (
                                TerminalReportPublication::AlreadyPublished(report_id.clone()),
                                report.parent.clone(),
                                report.delivery == ReportDeliveryState::PendingMailbox,
                            )
                        });
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
                let child_assignment = state.assignments.get_mut(child)?;
                child_assignment.terminal_report_id = Some(report_id.clone());
                state.reports.insert(
                    report_id.clone(),
                    TerminalReport {
                        child: child.clone(),
                        parent: parent.clone(),
                        terminal_turn_id: terminal_turn_id.to_string(),
                        communication,
                        delivery: ReportDeliveryState::PendingMailbox,
                        mailbox_inserted: false,
                    },
                );
                state
                    .pending_by_parent
                    .entry(parent.clone())
                    .or_default()
                    .push_back(report_id.clone());
                Some((
                    TerminalReportPublication::Published(report_id),
                    parent,
                    true,
                ))
            })()
        }?;
        if publication.2 {
            self.request_wake(publication.1);
        }
        Some(publication.0)
    }

    /// Claims one report so a failed send can release only the affected mailbox item.
    pub(crate) fn claim_next_mailbox_report(
        self: &std::sync::Arc<Self>,
        parent: &AgentAssignmentId,
    ) -> Option<ClaimedTerminalReport> {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&parent.thread_id) != Some(parent)
            || !state
                .assignments
                .get(parent)
                .is_some_and(|assignment| assignment.phase.is_open())
        {
            return None;
        }
        let ids = state
            .pending_by_parent
            .get(parent)?
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for id in ids {
            let Some(report) = state.reports.get_mut(&id) else {
                continue;
            };
            if report.delivery != ReportDeliveryState::PendingMailbox {
                continue;
            }
            report.delivery = ReportDeliveryState::Claimed;
            return Some(ClaimedTerminalReport {
                coordinator: std::sync::Arc::downgrade(self),
                id,
                sender_thread_id: report.child.thread_id,
                communication: report.communication.clone(),
                enqueued: false,
            });
        }
        None
    }

    pub(crate) fn release_mailbox_claim(&self, report_id: &ResponseItemId) -> bool {
        let mut state = self.lock_state();
        let Some(report) = state.reports.get_mut(report_id) else {
            return false;
        };
        if !matches!(
            report.delivery,
            ReportDeliveryState::Claimed | ReportDeliveryState::Enqueued
        ) || report.mailbox_inserted
        {
            return false;
        }
        report.delivery = ReportDeliveryState::PendingMailbox;
        true
    }

    fn release_mailbox_claim_for_retry(&self, report_id: &ResponseItemId) -> bool {
        let parent = {
            let mut state = self.lock_state();
            let Some(report) = state.reports.get_mut(report_id) else {
                return false;
            };
            if !matches!(
                report.delivery,
                ReportDeliveryState::Claimed | ReportDeliveryState::Enqueued
            ) || report.mailbox_inserted
            {
                return false;
            }
            report.delivery = ReportDeliveryState::PendingMailbox;
            report.parent.clone()
        };
        self.request_wake(parent)
    }

    pub(crate) fn report_is_current_for_thread(
        &self,
        report_id: &ResponseItemId,
        parent_thread_id: codex_protocol::ThreadId,
    ) -> bool {
        let state = self.lock_state();
        let Some(report) = state.reports.get(report_id) else {
            return false;
        };
        report.parent.thread_id == parent_thread_id
            && matches!(
                report.delivery,
                ReportDeliveryState::Claimed | ReportDeliveryState::Enqueued
            )
            && state.current_by_thread.get(&parent_thread_id) == Some(&report.parent)
            && state
                .assignments
                .get(&report.parent)
                .is_some_and(|assignment| assignment.phase.is_open())
    }

    pub(crate) fn discard_report_if_stale(&self, report_id: &ResponseItemId) -> bool {
        let mut state = self.lock_state();
        let Some(report) = state.reports.get(report_id) else {
            return false;
        };
        let parent = report.parent.clone();
        if state.current_by_thread.get(&parent.thread_id) == Some(&parent)
            && state
                .assignments
                .get(&parent)
                .is_some_and(|assignment| assignment.phase.is_open())
        {
            return false;
        }
        let Some(report) = state.reports.remove(report_id) else {
            return false;
        };
        Self::remove_pending_report(&mut state, &parent, report_id);
        if let Some(parent_assignment) = state.assignments.get_mut(&parent) {
            parent_assignment.direct_children.remove(&report.child);
        }
        if state
            .assignments
            .get(&report.child)
            .is_some_and(|assignment| assignment.phase.is_terminal())
        {
            Self::release_assignment(&mut state, &report.child);
        }
        true
    }

    pub(crate) fn mark_report_enqueued(&self, report_id: &ResponseItemId) -> bool {
        let mut state = self.lock_state();
        let Some(report) = state.reports.get_mut(report_id) else {
            return false;
        };
        match report.delivery {
            ReportDeliveryState::Claimed => report.delivery = ReportDeliveryState::Enqueued,
            ReportDeliveryState::Enqueued => {}
            _ => return false,
        }
        true
    }

    pub(crate) fn acknowledge_report_mailbox_delivery(
        &self,
        report_id: &ResponseItemId,
        parent_thread_id: codex_protocol::ThreadId,
    ) -> bool {
        let mut state = self.lock_state();
        let Some(report) = state.reports.get(report_id) else {
            return false;
        };
        let parent = report.parent.clone();
        if parent.thread_id != parent_thread_id
            || report.mailbox_inserted
            || !matches!(
                report.delivery,
                ReportDeliveryState::Claimed | ReportDeliveryState::Enqueued
            )
            || state.current_by_thread.get(&parent_thread_id) != Some(&parent)
            || !state
                .assignments
                .get(&parent)
                .is_some_and(|assignment| assignment.phase.is_open())
        {
            return false;
        }
        let Some(report) = state.reports.get_mut(report_id) else {
            return false;
        };
        report.delivery = ReportDeliveryState::Enqueued;
        report.mailbox_inserted = true;
        true
    }

    pub(crate) fn has_pending_reports(&self, parent: &AgentAssignmentId) -> bool {
        self.lock_state()
            .pending_by_parent
            .get(parent)
            .is_some_and(|reports| !reports.is_empty())
    }

    pub(crate) fn has_recorded_reports(&self, parent: &AgentAssignmentId) -> bool {
        let state = self.lock_state();
        state.pending_by_parent.get(parent).is_some_and(|reports| {
            reports.iter().any(|report_id| {
                state
                    .reports
                    .get(report_id)
                    .is_some_and(|report| report.delivery == ReportDeliveryState::Recorded)
            })
        })
    }

    pub(crate) fn mark_report_recorded(&self, report_id: &ResponseItemId) -> bool {
        let mut state = self.lock_state();
        let Some(report) = state.reports.get_mut(report_id) else {
            return false;
        };
        if report.delivery != ReportDeliveryState::Enqueued {
            return false;
        }
        report.delivery = ReportDeliveryState::Recorded;
        true
    }

    /// Captures only recorded reports whose stable IDs appear in this prompt snapshot.
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

    /// Consumes reports only after their stable IDs appeared in an accepted model request.
    pub(crate) fn accept_reports(
        &self,
        parent: &AgentAssignmentId,
        report_ids: &[ResponseItemId],
    ) -> usize {
        let mut state = self.lock_state();
        if state.current_by_thread.get(&parent.thread_id) != Some(parent)
            || !state
                .assignments
                .get(parent)
                .is_some_and(|assignment| assignment.phase.is_open())
        {
            return 0;
        }
        let mut accepted = 0;
        for report_id in report_ids {
            let Some(report) = state.reports.get(report_id) else {
                continue;
            };
            if &report.parent != parent || report.delivery != ReportDeliveryState::Recorded {
                continue;
            }
            let Some(report) = state.reports.remove(report_id) else {
                continue;
            };
            Self::remove_pending_report(&mut state, &report.parent, report_id);
            if let Some(parent_assignment) = state.assignments.get_mut(parent) {
                parent_assignment.direct_children.remove(&report.child);
            }
            Self::release_assignment(&mut state, &report.child);
            accepted += 1;
        }
        accepted
    }
}
