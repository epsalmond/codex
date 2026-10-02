//! Tracks coordinator report mail through history recording and model acceptance.

use super::TurnInput;
use super::session::Session;
use super::turn_context::TurnContext;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;

impl Session {
    pub(crate) async fn return_coordinator_reports_to_mailbox(&self, input: &[TurnInput]) {
        let mut reports = Vec::new();
        for input_item in input {
            let Some(report_id) = coordinator_report_id(input_item) else {
                continue;
            };
            if self
                .services
                .local_agent_runtime
                .terminal_report_is_current(report_id, self.thread_id)
            {
                if let Some(report) = deferred_coordinator_report(input_item) {
                    reports.push(report);
                }
            } else {
                self.services
                    .local_agent_runtime
                    .release_terminal_report_claim(report_id);
                self.services
                    .local_agent_runtime
                    .discard_stale_terminal_report(report_id);
            }
        }
        if !reports.is_empty() {
            self.input_queue.return_to_mailbox(&mut reports).await;
        }
    }

    pub(crate) fn coordinator_report_id(input_item: &TurnInput) -> Option<&ResponseItemId> {
        coordinator_report_id(input_item)
    }

    pub(crate) fn deferred_coordinator_report(input_item: &TurnInput) -> Option<TurnInput> {
        deferred_coordinator_report(input_item)
    }

    pub(crate) fn coordinator_report_is_current(&self, report_id: &ResponseItemId) -> bool {
        self.services
            .local_agent_runtime
            .terminal_report_is_current(report_id, self.thread_id)
    }

    pub(crate) fn mark_coordinator_report_recorded(&self, report_id: &ResponseItemId) {
        self.services
            .local_agent_runtime
            .mark_wake_report_recorded(report_id);
    }

    pub(crate) fn coordinator_report_candidates(
        &self,
        turn_context: &TurnContext,
        prompt_input: &[ResponseItem],
    ) -> Vec<ResponseItemId> {
        turn_context
            .agent_assignment
            .get()
            .map_or_else(Vec::new, |assignment| {
                self.services
                    .local_agent_runtime
                    .wake_report_ids_in_prompt(assignment, prompt_input)
            })
    }

    pub(crate) fn accept_coordinator_report_candidates(
        &self,
        turn_context: &TurnContext,
        report_candidates: &[ResponseItemId],
    ) {
        if let Some(assignment) = turn_context.agent_assignment.get() {
            self.services
                .local_agent_runtime
                .accept_wake_reports(assignment, report_candidates);
        }
    }
}

fn coordinator_report_id(input_item: &TurnInput) -> Option<&ResponseItemId> {
    match input_item {
        TurnInput::InterAgentCommunication(communication) => communication
            .id
            .as_ref()
            .filter(|id| id.as_str().starts_with("amsg_")),
        _ => None,
    }
}

fn deferred_coordinator_report(input_item: &TurnInput) -> Option<TurnInput> {
    let TurnInput::InterAgentCommunication(communication) = input_item else {
        return None;
    };
    let communication: InterAgentCommunication = communication.clone();
    if !communication
        .id
        .as_ref()
        .is_some_and(|id| id.as_str().starts_with("amsg_"))
    {
        return None;
    }
    Some(TurnInput::InterAgentCommunication(communication))
}

#[cfg(test)]
#[path = "wake_reports_tests.rs"]
mod tests;
