//! Turn-scoped terminal failures requested by hosts after a turn has started.

use super::Session;
use super::TaskCancellation;
use crate::agent::control::TurnEndDisposition;
use codex_extension_api::ThreadIdleCause;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnCompleteEvent;
use std::sync::Arc;

impl Session {
    pub(crate) async fn fail_turn_if_active(self: &Arc<Self>, turn_id: &str, error: ErrorEvent) {
        if !error.affects_turn_status() {
            return;
        }
        let active_turn = {
            let mut active = self.active_turn.lock().await;
            if active
                .as_ref()
                .and_then(|turn| turn.task.as_ref())
                .is_some_and(|task| task.turn_context.sub_id == turn_id)
            {
                active.take()
            } else {
                None
            }
        };
        let Some(mut active_turn) = active_turn else {
            return;
        };
        let Some(mut task) = active_turn.task.take() else {
            return;
        };
        let context = Arc::clone(&task.turn_context);
        let _finalization = context.agent_assignment.get().and_then(|assignment| {
            self.services
                .local_agent_runtime
                .retain_assignment_finalization(assignment)
        });
        // Seal and capture the subtree before aborting work can detach descendants.
        let failure_work = context.agent_assignment.get().and_then(|assignment| {
            match self.services.local_agent_runtime.prepare_wake_turn_end(
                assignment,
                &context.sub_id,
                TurnEndDisposition::Errored,
                || self.input_queue.pause_wakeups(),
            ) {
                Ok(classification) => Some((assignment.clone(), classification)),
                Err(error) => {
                    tracing::warn!(%error, "failed to classify host turn failure");
                    None
                }
            }
        });
        if failure_work
            .as_ref()
            .is_some_and(|(_, classification)| classification.newly_classified)
        {
            self.emit_agent_wakeups_updated().await;
        }
        self.cancel_running_task(&mut task, &TaskCancellation::Failed)
            .await;
        self.input_queue
            .take_pending_input_for_turn_state(&active_turn.turn_state)
            .await;
        self.send_event(&context, EventMsg::Error(error.clone()))
            .await;
        let started_at = context.turn_timing_state.started_at_unix_secs().await;
        let (completed_at, duration_ms, profile) = context
            .turn_timing_state
            .complete_profile_and_duration_ms()
            .await;
        self.services.analytics_events_client.track_turn_profile(
            codex_analytics::TurnProfileFact {
                turn_id: context.sub_id.clone(),
                profile,
            },
        );
        self.emit_turn_stop_lifecycle(context.extension_data.as_ref())
            .await;
        let event = EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: context.sub_id.clone(),
            last_agent_message: None,
            error: Some(error),
            started_at,
            completed_at,
            duration_ms,
            time_to_first_token_ms: context.turn_timing_state.time_to_first_token_ms().await,
        });
        if let Some((assignment, classification)) = failure_work {
            self.services
                .local_agent_runtime
                .finish_wake_turn_end(&assignment, classification)
                .await;
        }
        self.send_event(&context, event).await;
        self.services
            .local_agent_runtime
            .notify_active_turn_cleared();
        self.emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Failed)
            .await;
        if let Err(error) = self.flush_rollout().await {
            tracing::warn!(%error, "failed to flush terminal turn failure");
        }
    }
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod tests;
