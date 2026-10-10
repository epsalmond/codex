//! Starts a turn for reports that are already in history but missed the prior request snapshot.

use super::RegularTask;
use super::TaskStartOutcome;
use super::TurnReservation;
use crate::agent::control::AgentAssignmentId;
use crate::session::session::Session;
use crate::session::turn_context::NewTurnContextOptions;
use crate::state::ActiveTurn;
use std::sync::Arc;
use tracing::warn;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordedReportWakeResult {
    Started,
    Busy,
    NoLongerNeeded,
}

impl Session {
    pub(crate) async fn start_turn_for_recorded_reports(
        self: &Arc<Self>,
        assignment: &AgentAssignmentId,
    ) -> RecordedReportWakeResult {
        let runtime = &self.services.local_agent_runtime;
        if !runtime.is_current_waiting_assignment(assignment)
            || !runtime.has_recorded_wake_reports(assignment)
            || self.input_queue.wakeups_paused()
        {
            return RecordedReportWakeResult::NoLongerNeeded;
        }

        let config = self.get_config().await;
        let source = self.session_source().await;
        let version = self
            .multi_agent_version()
            .unwrap_or_else(|| config.multi_agent_version_from_features());
        if self
            .services
            .agent_control
            .check_turn_admission(version, &source)
            .is_err()
        {
            return RecordedReportWakeResult::Busy;
        }

        let turn_id = uuid::Uuid::new_v4().to_string();
        let turn_state = {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.is_some() {
                return RecordedReportWakeResult::Busy;
            }
            let active_turn = active_turn.insert(ActiveTurn {
                wake_turn_id: Some(turn_id.clone()),
                ..ActiveTurn::default()
            });
            Arc::clone(&active_turn.turn_state)
        };

        self.services
            .models_manager
            .refresh_after_auth_change(self.get_config().await.http_client_factory())
            .await;
        if self
            .active_turn
            .lock()
            .await
            .as_ref()
            .is_none_or(|turn| !Arc::ptr_eq(&turn.turn_state, &turn_state))
        {
            return RecordedReportWakeResult::Busy;
        }

        let mut turn_context = self
            .new_turn_with_default_settings(turn_id, NewTurnContextOptions::default())
            .await;
        // Bind before draining input. A concurrent interruption or close can invalidate the
        // waiting assignment while turn context is being prepared; in that case its queued mail
        // must remain available for an explicit followup or shutdown recovery.
        if let Err(error) =
            self.bind_wake_assignment(&turn_context, /*allow_new_generation*/ false)
        {
            self.clear_reserved_idle_turn(&turn_state).await;
            warn!("failed to bind recorded-report wake turn: {error}");
            return RecordedReportWakeResult::NoLongerNeeded;
        }

        let (input, start_options) = self.input_queue.get_pending_input(&self.active_turn).await;
        let mail_start_options = start_options.clone();
        #[expect(
            clippy::expect_used,
            reason = "the new turn context is not shared until start_task"
        )]
        let turn_context_mut = Arc::get_mut(&mut turn_context)
            .expect("turn context stays uniquely owned until its task starts");
        turn_context_mut.final_output_json_schema = start_options.final_output_json_schema;
        turn_context_mut.cyber_access_program = start_options.cyber_access_program;
        if let Some(trigger) = start_options.turn_trigger {
            turn_context.turn_metadata_state.set_turn_trigger(trigger);
        }
        if let Some(root_turn_id) = start_options.root_turn_id {
            turn_context
                .turn_metadata_state
                .set_root_turn_id(root_turn_id);
        }
        self.input_queue
            .extend_pending_input_for_turn_state(turn_state.as_ref(), input)
            .await;
        let reservation = TurnReservation {
            turn_state,
            mail_start_options,
        };
        match self
            .start_task(turn_context, Vec::new(), RegularTask::new(), reservation)
            .await
        {
            TaskStartOutcome::Started => RecordedReportWakeResult::Started,
            // The reports stay in history for whichever turn holds the slot next.
            TaskStartOutcome::Rejected(_) | TaskStartOutcome::Aborted => {
                RecordedReportWakeResult::NoLongerNeeded
            }
        }
    }
}
