//! Binds turns to coordinator assignment generations and classifies their terminal outcomes.

use super::TurnInput;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::agent::control::AssignmentPhase;
use crate::agent::control::TurnEndDisposition;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TurnAbortReason;
use tracing::warn;

impl Session {
    pub(crate) fn bind_wake_assignment(
        &self,
        turn_context: &TurnContext,
        allow_new_generation: bool,
    ) -> CodexResult<()> {
        if turn_context.multi_agent_version != MultiAgentVersion::V2
            || turn_context.config.multi_agent_v2.agent_polling
                == codex_features::AgentPolling::Enabled
            || !self.services.local_agent_runtime.wake_mode_enabled()
        {
            return Ok(());
        }
        let parent_turn_id = turn_context.turn_metadata_state.parent_turn_id();
        let assignment = self
            .services
            .local_agent_runtime
            .begin_wake_assignment_for_turn(
                self.thread_id,
                &turn_context.session_source,
                &turn_context.sub_id,
                parent_turn_id.as_deref(),
                allow_new_generation,
            )
            .map_err(|message| CodexErr::InvalidRequest(message.to_string()))?;
        let Some(assignment) = assignment else {
            return Ok(());
        };
        match turn_context.agent_assignment.set(assignment.clone()) {
            Ok(()) => Ok(()),
            Err(existing) if existing == assignment => Ok(()),
            Err(_) => Err(CodexErr::InvalidRequest(
                "turn context already belongs to a different agent assignment".to_string(),
            )),
        }
    }

    pub(crate) fn bind_wake_assignment_for_input(
        &self,
        turn_context: &TurnContext,
        input: &[TurnInput],
    ) -> CodexResult<()> {
        let has_coordinator_report = input
            .iter()
            .any(|input_item| Self::coordinator_report_id(input_item).is_some());
        if let Err(error) = self.bind_wake_assignment(turn_context, !has_coordinator_report) {
            for report_id in input.iter().filter_map(Self::coordinator_report_id) {
                self.services
                    .local_agent_runtime
                    .release_terminal_report_claim(report_id);
                self.services
                    .local_agent_runtime
                    .discard_stale_terminal_report(report_id);
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn classify_wake_turn_end(&self, turn_context: &TurnContext, event: &EventMsg) {
        let Some(assignment) = turn_context.agent_assignment.get() else {
            return;
        };
        let disposition = match event {
            EventMsg::TurnComplete(event) if event.error.is_some() => TurnEndDisposition::Errored,
            EventMsg::TurnComplete(_) => TurnEndDisposition::Succeeded,
            EventMsg::TurnAborted(event) => match event.reason {
                TurnAbortReason::ReviewEnded => TurnEndDisposition::Cancelled,
                TurnAbortReason::Interrupted
                | TurnAbortReason::Replaced
                | TurnAbortReason::BudgetLimited => TurnEndDisposition::Interrupted,
            },
            _ => unreachable!("turn finalization only constructs terminal events"),
        };
        match self.services.local_agent_runtime.classify_wake_turn_end(
            assignment,
            &turn_context.sub_id,
            disposition,
        ) {
            Ok(AssignmentPhase::Waiting) => {
                let _ = turn_context.agent_assignment_waiting.set(true);
            }
            Ok(
                AssignmentPhase::Completed
                | AssignmentPhase::Errored
                | AssignmentPhase::Interrupted
                | AssignmentPhase::Cancelled,
            ) => {}
            Ok(AssignmentPhase::Reserved | AssignmentPhase::Running) => {
                unreachable!("terminal classification must leave a terminal phase")
            }
            Err(error) => warn!(
                "failed to classify terminal turn {} for wake assignment: {error}",
                turn_context.sub_id
            ),
        }
    }
}
