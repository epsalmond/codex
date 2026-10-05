//! Restores interruption state without making durable turn history terminal.

use super::Session;
use crate::agent::agent_status_from_event;
use codex_history::RolloutItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_thread_store::ListTurnsParams;
use codex_thread_store::SortDirection;
use codex_thread_store::StoredTurnItemsView;
use codex_thread_store::StoredTurnStatus;
use tracing::warn;

impl Session {
    pub(super) async fn restore_resumed_agent_status(&self, items: &[RolloutItem]) {
        let last_status = items.iter().rev().find_map(|item| match item {
            RolloutItem::EventMsg(event) => agent_status_from_event(event),
            _ => None,
        });
        if matches!(last_status, Some(AgentStatus::Interrupted)) {
            self.agent_status.send_replace(AgentStatus::Interrupted);
            return;
        }
        let (is_v2_child, history_mode) = {
            let state = self.state.lock().await;
            (
                self.multi_agent_version() == Some(MultiAgentVersion::V2)
                    && matches!(
                        state.session_configuration.session_source,
                        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                    ),
                state.session_configuration.history_mode,
            )
        };
        if !is_v2_child {
            return;
        }
        let history_interrupted = matches!(last_status, Some(AgentStatus::Running));

        let interrupted = if history_mode == ThreadHistoryMode::Paginated {
            // Model context can start at a compaction after the opening turn boundary.
            // Read only the newest durable turn's metadata, rather than replaying history.
            match self
                .services
                .thread_store
                .list_turns(ListTurnsParams {
                    thread_id: self.thread_id,
                    include_archived: true,
                    cursor: None,
                    page_size: 1,
                    sort_direction: SortDirection::Desc,
                    items_view: StoredTurnItemsView::NotLoaded,
                })
                .await
            {
                Ok(page) => page.turns.first().map_or(history_interrupted, |turn| {
                    matches!(
                        turn.status,
                        StoredTurnStatus::InProgress | StoredTurnStatus::Interrupted
                    )
                }),
                Err(error) => {
                    warn!(thread_id = %self.thread_id, %error, "failed to restore resumed agent turn status");
                    history_interrupted
                }
            }
        } else {
            history_interrupted
        };
        if interrupted {
            // No task owns an orphaned turn in this runtime. A later explicit recovery
            // may still continue it under its original ID, so do not write TurnAborted.
            self.agent_status.send_replace(AgentStatus::Interrupted);
        }
    }
}
