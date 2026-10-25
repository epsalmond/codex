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
        let ready = assignment.as_ref().map_or_else(
            || {
                self.services
                    .async_completions
                    .has_ready(self.thread_id(), None)
            },
            |assignment| runtime.has_ready_owned_completions(assignment),
        );
        ready.then_some(PendingWorkCause::OwnedCompletion(assignment))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::control::AssignmentPhase;
    use crate::agent::control::TurnEndDisposition;
    use crate::agent::control::WorkKind;
    use crate::session::tests::make_session_and_context;
    use crate::tasks::PendingWorkStartResult;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;

    #[tokio::test]
    async fn finite_hint_without_payload_passes_selector_and_bind_and_survives_paused_unstarted_turn()
     {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let runtime = &session.services.local_agent_runtime;
        runtime.enable_wake_mode();
        let assignment = runtime
            .begin_wake_assignment_for_turn(
                session.thread_id(),
                &turn.session_source,
                &turn.sub_id,
                true,
            )
            .unwrap()
            .unwrap();
        turn.agent_assignment.set(assignment.clone()).unwrap();
        let work = runtime
            .reserve_owned_work(&session, &turn, "tty", WorkKind::Task, None)
            .unwrap();
        work.mark_started();
        work.mark_yielded(false);
        assert_eq!(
            runtime
                .classify_wake_turn_end(&assignment, &turn.sub_id, TurnEndDisposition::Succeeded)
                .await,
            Ok(AssignmentPhase::Waiting)
        );
        work.mark_terminal();
        drop(work);
        assert!(!session.services.async_completions.has_owned_work());
        assert!(
            matches!(session.ready_owned_completion().await, Some(PendingWorkCause::OwnedCompletion(Some(ref ready))) if ready == &assignment)
        );
        session.input_queue.pause_wakeups();
        assert_eq!(
            session.maybe_start_turn_for_pending_work().await,
            PendingWorkStartResult::Stale
        );
        assert!(runtime.has_ready_owned_completions(&assignment));
        session.input_queue.resume_wakeups();
        let followup = session.new_default_turn().await;
        let rollback = session.guard_unstarted_turn_assignment(&followup.sub_id);
        session
            .bind_owned_completion(&followup, Some(&assignment))
            .unwrap();
        assert!(!runtime.has_ready_owned_completions(&assignment));
        drop(rollback);
        assert!(runtime.has_ready_owned_completions(&assignment));
        let next = session.new_default_turn().await;
        session
            .bind_owned_completion(&next, Some(&assignment))
            .unwrap();
        runtime.acknowledge_finite_terminal_hint(&assignment, &next.sub_id);
        assert_eq!(
            runtime
                .classify_wake_turn_end(&assignment, &next.sub_id, TurnEndDisposition::Succeeded)
                .await,
            Ok(AssignmentPhase::Completed)
        );
        assert!(session.ready_owned_completion().await.is_none());
    }
}
