//! Esc pause for a root thread in wake mode.
//!
//! In wake mode, a direct child's final answer starts the root's next turn. Interrupting the root
//! pauses those automatic wakeups: results stay queued until the next user message, which clears
//! the pause and starts one turn carrying both the user's text and the queued results.

use super::multi_agents::ChildReportMode;
use super::session::Session;
use codex_protocol::protocol::AgentWakeupsUpdatedEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use uuid::Uuid;

impl Session {
    /// Returns this thread's child report mode, or `None` outside MultiAgentV2.
    pub(crate) async fn child_report_mode(&self) -> Option<ChildReportMode> {
        if self.multi_agent_version() != Some(MultiAgentVersion::V2) {
            return None;
        }
        let config = self.get_config().await;
        let session_source = self
            .state
            .lock()
            .await
            .session_configuration
            .session_source
            .clone();
        Some(ChildReportMode::for_thread(
            &config.multi_agent_v2,
            &session_source,
        ))
    }

    /// Pauses automatic wakeups when this is a root thread in wake mode.
    pub(super) async fn pause_wakeups_for_interrupt(&self) {
        if self.child_report_mode().await == Some(ChildReportMode::WakeOnReport) {
            self.input_queue.pause_wakeups();
            self.emit_agent_wakeups_updated().await;
        }
    }

    /// Lets the root decide whether a child's report wakes it.
    ///
    /// Children mark every completion sent to the root with `trigger_turn`. No other agent mail
    /// triggers the root: `followup_task` cannot target it and `send_message` is queue-only. A
    /// MultiAgentV2 root that keeps `wait_agent` reads the report on its next turn instead, and so
    /// does a root whose version is unresolved. Only a new thread before its first turn is
    /// unresolved, and it has no children yet; a resumed root resolves it from its history.
    pub(super) async fn apply_child_report_mode(
        &self,
        communication: &mut InterAgentCommunication,
    ) {
        let reads_on_next_turn = match self.multi_agent_version() {
            None => true,
            Some(MultiAgentVersion::V2) => {
                self.child_report_mode().await != Some(ChildReportMode::WakeOnReport)
            }
            Some(MultiAgentVersion::V1 | MultiAgentVersion::Disabled) => false,
        };
        if reads_on_next_turn
            && communication.trigger_turn
            && communication.recipient.is_root()
            && !communication.author.is_root()
        {
            communication.trigger_turn = false;
        }
    }

    /// Clears a pause before a user turn starts. `start_task` then moves the held mail into that
    /// turn's pending input, which the turn records with the user's input and a replacement
    /// returns to the mailbox.
    pub(super) async fn resume_paused_wakeups(&self) {
        if self.input_queue.resume_wakeups() {
            self.emit_agent_wakeups_updated().await;
        }
    }

    pub(crate) async fn emit_agent_wakeups_updated(&self) {
        let paused = self.input_queue.wakeups_paused();
        let queued_results = if paused {
            self.input_queue.trigger_turn_mailbox_count().await
        } else {
            0
        };
        self.send_event_raw_without_materializing_rollout(Event {
            id: Uuid::now_v7().to_string(),
            msg: EventMsg::AgentWakeupsUpdated(AgentWakeupsUpdatedEvent {
                paused,
                queued_results: u32::try_from(queued_results).unwrap_or(u32::MAX),
            }),
        })
        .await;
    }
}

#[cfg(test)]
#[path = "wakeups_tests.rs"]
mod tests;
