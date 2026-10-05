//! Reserves a nested-wake assignment before creating its child thread.

use super::LocalAgentControl;
use super::coordinator::AgentAssignmentId;
use super::coordinator::AssignmentReservation;
use codex_features::AgentPolling;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

pub(super) struct WakeAssignmentReservation {
    reservation: AssignmentReservation,
    thread_id: ThreadId,
}

impl WakeAssignmentReservation {
    pub(super) fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    pub(super) fn operation(&self) -> std::sync::Arc<crate::agent::control::GenerationOperation> {
        self.reservation.operation()
    }

    pub(super) fn commit(self, actual_thread_id: ThreadId) -> CodexResult<AgentAssignmentId> {
        if actual_thread_id != self.thread_id {
            return Err(CodexErr::InvalidRequest(
                "spawned thread ID did not match its wake assignment reservation".to_string(),
            ));
        }
        self.reservation.commit().map_err(|error| {
            CodexErr::InvalidRequest(format!("delegation was cancelled before start: {error}"))
        })
    }
}

impl LocalAgentControl {
    pub(super) fn reserve_wake_assignment(
        &self,
        multi_agent_version: MultiAgentVersion,
        agent_polling: AgentPolling,
        notification_source: Option<&SessionSource>,
        parent_turn_id: Option<&str>,
    ) -> CodexResult<Option<WakeAssignmentReservation>> {
        if multi_agent_version != MultiAgentVersion::V2
            || agent_polling == AgentPolling::Enabled
            || !self.runtime.wake_mode_enabled()
        {
            return Ok(None);
        }
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = notification_source
        else {
            return Ok(None);
        };
        let parent_turn_id = parent_turn_id.ok_or_else(|| {
            CodexErr::InvalidRequest(
                "wake-mode delegation requires an active parent turn".to_string(),
            )
        })?;
        let parent_assignment = self
            .runtime
            .wake_coordinator
            .active_assignment_for_turn(*parent_thread_id, parent_turn_id)
            .ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "wake-mode delegation parent assignment is no longer active".to_string(),
                )
            })?;
        let thread_id = self.runtime.generate_thread_id();
        let reservation = self
            .runtime
            .wake_coordinator
            .reserve_child_assignment(parent_assignment, thread_id)
            .map_err(|error| {
                CodexErr::InvalidRequest(format!("delegation was rejected: {error}"))
            })?;
        Ok(Some(WakeAssignmentReservation {
            reservation,
            thread_id,
        }))
    }
}

#[cfg(test)]
#[path = "wake_spawn_tests.rs"]
mod tests;
