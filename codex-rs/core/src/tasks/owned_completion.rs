//! Automatic completion wakes carry no user input or new assignment authority.

use crate::agent::control::AgentAssignmentId;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

pub(super) enum PendingWorkCause {
    Mailbox,
    OwnedCompletion(Option<AgentAssignmentId>),
}

impl Session {
    pub(super) fn bind_owned_completion(
        &self,
        turn: &TurnContext,
        assignment: Option<&AgentAssignmentId>,
    ) -> CodexResult<()> {
        let Some(assignment) = assignment else {
            return self.bind_wake_assignment(turn, /*allow_new_generation*/ false);
        };
        self.services
            .local_agent_runtime
            .continue_owned_completion(assignment, &turn.sub_id)
            .map_err(|error| CodexErr::InvalidRequest(error.to_owned()))?;
        turn.agent_assignment.set(assignment.clone()).map_err(|_| {
            CodexErr::InvalidRequest("completion turn already has an assignment".to_owned())
        })
    }

    pub(super) async fn ready_owned_completion(&self) -> Option<PendingWorkCause> {
        let runtime = &self.services.local_agent_runtime;
        let assignment = runtime.current_wake_assignment(self.thread_id());
        if assignment.is_none() && self.session_source().await.is_non_root_agent() {
            return None;
        }
        if assignment
            .as_ref()
            .is_some_and(|id| !runtime.is_current_waiting_assignment(id))
        {
            return None;
        }
        self.services
            .async_completions
            .has_ready(self.thread_id(), assignment.as_ref())
            .then_some(PendingWorkCause::OwnedCompletion(assignment))
    }
}
