//! Restores live child status without changing durable turn or assignment state.

use super::Session;
use crate::agent::agent_status_from_event;
use codex_history::RolloutItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_thread_store::ListTurnsParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::SortDirection;
use codex_thread_store::StoredTurnItemsView;
use codex_thread_store::StoredTurnStatus;
use tracing::warn;

impl Session {
    pub(super) async fn restore_resumed_agent_status(&self, items: &[RolloutItem]) {
        let last_status = last_history_status(items);
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
            // Preserve the earlier interruption restoration for other session sources.
            if matches!(last_status, Some(AgentStatus::Interrupted)) {
                self.agent_status.send_replace(AgentStatus::Interrupted);
            }
            return;
        }

        let restored = if history_mode == ThreadHistoryMode::Paginated {
            self.latest_durable_agent_status(items)
                .await
                .or(last_status)
        } else {
            last_status
        };
        if let Some(status) = restored {
            // No task owns an orphaned turn in this runtime. A later explicit recovery
            // may still continue it under its original ID, so do not write TurnAborted.
            // Assignment restoration can still override this with Waiting/Interrupted.
            let status = match status {
                AgentStatus::Running => AgentStatus::Interrupted,
                status => status,
            };
            self.agent_status.send_replace(status);
        }
    }

    async fn latest_durable_agent_status(&self, items: &[RolloutItem]) -> Option<AgentStatus> {
        // Model context can omit both boundaries of the latest durable turn.
        // Its metadata takes precedence over any older status in the loaded suffix.
        let turn = match self
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
            Ok(page) => page.turns.into_iter().next()?,
            Err(error) => {
                warn!(thread_id = %self.thread_id, %error, "failed to restore resumed agent turn status");
                return None;
            }
        };
        if matches!(
            turn.status,
            StoredTurnStatus::InProgress | StoredTurnStatus::Interrupted
        ) {
            return Some(AgentStatus::Interrupted);
        }
        if let Some(status) = terminal_status_for_turn(items, &turn.turn_id) {
            return Some(status);
        }
        // The projection has status and errors, but not terminal report text. Scan
        // for that exact event when compaction hid it; assistant summary text can
        // differ from a typed blocked/final report.
        let terminal = match self
            .services
            .thread_store
            .load_turn_terminal_event(
                LoadThreadHistoryParams {
                    thread_id: self.thread_id,
                    include_archived: true,
                },
                turn.turn_id.clone(),
            )
            .await
        {
            Ok(event) => event.as_ref().and_then(agent_status_from_event),
            Err(error) => {
                warn!(thread_id = %self.thread_id, %error, "failed to restore resumed agent result");
                None
            }
        };
        Some(terminal.unwrap_or_else(|| match turn.error {
            Some(error) => AgentStatus::Errored(error.message),
            None if turn.status == StoredTurnStatus::Failed => {
                AgentStatus::Errored("Persisted turn failed".to_string())
            }
            None => AgentStatus::Completed(None),
        }))
    }
}

fn last_history_status(items: &[RolloutItem]) -> Option<AgentStatus> {
    items.iter().rev().find_map(|item| match item {
        // Shutdown belongs to the old runtime, not the resumed child or its assignment.
        RolloutItem::EventMsg(EventMsg::ShutdownComplete) => None,
        RolloutItem::EventMsg(event) => agent_status_from_event(event),
        _ => None,
    })
}

fn terminal_status_for_turn(items: &[RolloutItem], turn_id: &str) -> Option<AgentStatus> {
    items.iter().rev().find_map(|item| match item {
        RolloutItem::EventMsg(event @ EventMsg::TurnComplete(complete))
            if complete.turn_id == turn_id =>
        {
            agent_status_from_event(event)
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context_with_session_source;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use std::sync::OnceLock;

    #[tokio::test]
    async fn resumed_child_without_turns_stays_pending_init() {
        let (mut session, _) = make_session_and_context_with_session_source(
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: ThreadId::new(),
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
        )
        .await;
        session.multi_agent_version = OnceLock::from(MultiAgentVersion::V2);
        session.restore_resumed_agent_status(&[]).await;
        assert_eq!(*session.agent_status.borrow(), AgentStatus::PendingInit);
    }

    #[tokio::test]
    async fn completed_model_turn_does_not_finish_a_waiting_assignment() {
        let parent_id = ThreadId::new();
        let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: parent_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        });
        let (mut session, turn) =
            make_session_and_context_with_session_source(source.clone()).await;
        session.multi_agent_version = OnceLock::from(MultiAgentVersion::V2);
        let runtime = &session.services.local_agent_runtime;
        runtime.enable_wake_mode();
        runtime
            .begin_wake_assignment_for_turn(parent_id, &SessionSource::Cli, "parent-turn", true)
            .expect("parent assignment");
        let child = runtime
            .begin_wake_assignment_for_turn(session.thread_id, &source, &turn.sub_id, true)
            .expect("child assignment")
            .expect("wake mode enabled");
        let leaf = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: session.thread_id,
            depth: 2,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        });
        runtime
            .begin_wake_assignment_for_turn(ThreadId::new(), &leaf, "leaf-turn", true)
            .expect("unfinished descendant assignment");
        assert_eq!(
            runtime
                .classify_wake_turn_end(
                    &child,
                    &turn.sub_id,
                    crate::agent::control::TurnEndDisposition::Succeeded
                )
                .await,
            Ok(crate::agent::control::AssignmentPhase::Waiting)
        );
        let complete = codex_protocol::protocol::TurnCompleteEvent {
            turn_id: turn.sub_id.clone(),
            last_agent_message: Some("model turn yielded".to_owned()),
            root_turn_id: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        };
        session
            .restore_resumed_agent_status(&[RolloutItem::EventMsg(EventMsg::TurnComplete(
                complete,
            ))])
            .await;
        assert_eq!(
            runtime.wake_assignment_status(session.thread_id),
            Some(AgentStatus::Waiting)
        );
        // Registration applies assignment status after restoring the persisted model turn.
        session.restore_wake_assignment_status();
        assert_eq!(*session.agent_status.borrow(), AgentStatus::Waiting);
    }
}
