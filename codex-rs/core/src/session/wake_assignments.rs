//! Binds turns to coordinator assignment generations and classifies their terminal outcomes.

use super::TurnInput;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::agent::control::AssignmentPhase;
use crate::agent::control::TurnEndDisposition;
use codex_protocol::AgentPath;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TurnAbortReason;
use tracing::warn;

/// Held by a start from the moment it reserves the active-turn slot until its task is
/// installed. Dropping it any other way gives back the assignment its turn id still holds, whether
/// the start bound it or inherited it from a reservation it replaced, so no exit leaves an
/// assignment running under a turn that never runs.
#[must_use]
pub(crate) struct UnstartedTurnAssignment<'a> {
    session: &'a Session,
    turn_id: Option<String>,
    agent_path: Option<AgentPath>,
}

impl UnstartedTurnAssignment<'_> {
    /// Records the agent path the turn runs as, so a child generation it gives back while its
    /// parent waits can report its interruption to that parent.
    pub(crate) fn record_turn_context(&mut self, turn_context: &TurnContext) {
        self.agent_path = turn_context.session_source.get_agent_path();
    }

    /// The task is installed; its turn now ends the assignment through normal finalization.
    pub(crate) fn started(mut self) {
        self.turn_id = None;
    }
}

impl Drop for UnstartedTurnAssignment<'_> {
    /// Holds no lock: the release and any report it publishes take only the coordinator lock, so
    /// this keeps the active_turn -> coordinator -> completions store order wherever it drops.
    fn drop(&mut self) {
        if let Some(turn_id) = self.turn_id.take() {
            self.session
                .services
                .local_agent_runtime
                .release_unstarted_wake_turn(
                    self.session.thread_id,
                    &turn_id,
                    self.agent_path.take(),
                );
        }
    }
}

impl Session {
    pub(crate) fn guard_unstarted_turn_assignment(
        &self,
        turn_id: &str,
    ) -> UnstartedTurnAssignment<'_> {
        UnstartedTurnAssignment {
            session: self,
            turn_id: Some(turn_id.to_owned()),
            agent_path: None,
        }
    }

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
        let assignment = self
            .services
            .local_agent_runtime
            .begin_wake_assignment_for_turn(
                self.thread_id,
                &turn_context.session_source,
                &turn_context.sub_id,
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

    pub(crate) async fn classify_wake_turn_end(
        &self,
        turn_context: &TurnContext,
        event: &EventMsg,
    ) {
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
        let runtime = &self.services.local_agent_runtime;
        let phase = if disposition == TurnEndDisposition::Errored {
            match runtime.prepare_wake_turn_end(
                assignment,
                &turn_context.sub_id,
                disposition,
                || self.input_queue.pause_wakeups(),
            ) {
                Ok(classification) => {
                    if classification.newly_classified {
                        self.emit_agent_wakeups_updated().await;
                    }
                    Ok(runtime
                        .finish_wake_turn_end(assignment, classification)
                        .await)
                }
                Err(error) => Err(error),
            }
        } else {
            runtime
                .classify_wake_turn_end(assignment, &turn_context.sub_id, disposition)
                .await
        };
        match phase {
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
