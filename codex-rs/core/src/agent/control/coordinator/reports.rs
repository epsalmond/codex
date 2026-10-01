use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use super::ReportDeliveryState;
use super::TerminalReport;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use std::collections::HashSet;
use uuid::Uuid;

pub(crate) enum TerminalReportPublication {
    Published(InterAgentCommunication),
    AlreadyPublished,
}

impl AgentWakeCoordinator {
    /// Publishes a terminal report once and gives the persisted AgentMessage a stable ID.
    pub(crate) fn publish_terminal_report(
        &self,
        child: &AgentAssignmentId,
        terminal_turn_id: &str,
        mut communication: InterAgentCommunication,
    ) -> Option<TerminalReportPublication> {
        let mut state = self.lock_state();
        let child_assignment = state.assignments.get(child)?;
        if let Some(report_id) = &child_assignment.terminal_report_id {
            return state
                .reports
                .get(report_id)
                .filter(|report| report.terminal_turn_id == terminal_turn_id)
                .map(|_| TerminalReportPublication::AlreadyPublished);
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
        state.reports.insert(
            report_id.clone(),
            TerminalReport {
                child: child.clone(),
                parent: parent.clone(),
                terminal_turn_id: terminal_turn_id.to_string(),
                communication: communication.clone(),
                delivery: ReportDeliveryState::PendingMailbox,
            },
        );
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
        Some(TerminalReportPublication::Published(communication))
    }

    /// Claims each mailbox report once; callers release the claim if enqueue fails.
    pub(crate) fn claim_pending_mailbox_reports(
        &self,
        parent: &AgentAssignmentId,
    ) -> Vec<InterAgentCommunication> {
        let mut state = self.lock_state();
        let ids = state
            .pending_by_parent
            .get(parent)
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        let mut reports = Vec::new();
        for id in ids {
            if let Some(report) = state.reports.get_mut(&id)
                && report.delivery == ReportDeliveryState::PendingMailbox
            {
                report.delivery = ReportDeliveryState::Enqueued;
                reports.push(report.communication.clone());
            }
        }
        reports
    }

    pub(crate) fn release_mailbox_claim(&self, report_id: &ResponseItemId) -> bool {
        let mut state = self.lock_state();
        let Some(report) = state.reports.get_mut(report_id) else {
            return false;
        };
        if report.delivery != ReportDeliveryState::Enqueued {
            return false;
        }
        report.delivery = ReportDeliveryState::PendingMailbox;
        true
    }

    pub(crate) fn mark_report_recorded(&self, report_id: &ResponseItemId) -> bool {
        let mut state = self.lock_state();
        let Some(report) = state.reports.get_mut(report_id) else {
            return false;
        };
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
            let report = state
                .reports
                .remove(report_id)
                .expect("report checked above");
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
