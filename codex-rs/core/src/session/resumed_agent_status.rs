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
use codex_thread_store::ThreadStoreError;
use tracing::warn;

impl Session {
    pub(super) async fn restore_resumed_agent_status(&self, items: &[RolloutItem]) {
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
            let last_status = items.iter().rev().find_map(|item| match item {
                RolloutItem::EventMsg(event) => agent_status_from_event(event),
                _ => None,
            });
            if matches!(last_status, Some(AgentStatus::Interrupted)) {
                self.agent_status.send_replace(AgentStatus::Interrupted);
            }
            return;
        }

        let last_status = last_history_status(items);
        // Visible turn boundaries are canonical and newer than a compaction-hidden
        // boundary. SQLite projections can lag durable rollout writes after a failure.
        let restored = if last_status.is_some() {
            last_status
        } else if history_mode == ThreadHistoryMode::Paginated {
            self.latest_durable_agent_status().await
        } else {
            None
        };
        if let Some(status) = restored {
            // No task owns an orphaned turn in this runtime. A later explicit recovery
            // may still continue it under its original ID, so do not write TurnAborted.
            // Assignment restoration can still override this with Waiting/Interrupted.
            self.agent_status.send_replace(status);
        }
    }

    async fn latest_durable_agent_status(&self) -> Option<AgentStatus> {
        // Model context can omit both boundaries of the latest durable turn.
        // Consult its metadata only when no boundary is visible in the loaded suffix.
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
            Ok(event) => event.as_ref().and_then(resumed_status_from_event),
            Err(ThreadStoreError::Unsupported { .. }) => None,
            Err(error) => {
                warn!(thread_id = %self.thread_id, %error, "failed to restore resumed agent result");
                None
            }
        };
        terminal.or_else(|| match turn.error {
            Some(error) => Some(AgentStatus::Errored(error.message)),
            None if turn.status == StoredTurnStatus::Failed => {
                Some(AgentStatus::Errored("Persisted turn failed".to_string()))
            }
            // Completed metadata does not say whether a result or blocked report
            // existed. Leave status unknown when the canonical event is unavailable.
            None => None,
        })
    }
}

fn last_history_status(items: &[RolloutItem]) -> Option<AgentStatus> {
    items.iter().rev().find_map(|item| match item {
        RolloutItem::EventMsg(event) => resumed_status_from_event(event),
        _ => None,
    })
}

fn resumed_status_from_event(event: &EventMsg) -> Option<AgentStatus> {
    match event {
        EventMsg::TurnStarted(_) | EventMsg::TurnAborted(_) => {
            // Even replacement/review aborts have no live owner after restart.
            Some(AgentStatus::Interrupted)
        }
        EventMsg::TurnComplete(complete) => Some(match &complete.error {
            Some(error) => AgentStatus::Errored(error.message.clone()),
            None => AgentStatus::Completed(complete.last_agent_message.clone()),
        }),
        // Standalone errors can be transient; only turn boundaries establish status.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context_with_session_source;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use std::sync::OnceLock;

    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_thread_store as store;
    use std::any::Any;
    use std::sync::Arc;

    #[derive(Clone, Copy)]
    enum TerminalLookup {
        Unsupported,
        Failed,
        Missing,
    }

    struct StatusStore {
        inner: Arc<dyn store::ThreadStore>,
        turn: store::StoredTurn,
        terminal: TerminalLookup,
    }

    macro_rules! delegate_status_store {
        ($(fn $name:ident($param:ident: $params:ty) -> $result:ty;)*) => {
            $(fn $name(&self, $param: $params) -> store::ThreadStoreFuture<'_, $result> {
                self.inner.$name($param)
            })*
        };
    }

    impl store::ThreadStore for StatusStore {
        fn as_any(&self) -> &dyn Any {
            self
        }
        delegate_status_store! {
            fn create_thread(params: store::CreateThreadParams) -> ();
            fn resume_thread(params: store::ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
            fn append_items(params: store::AppendThreadItemsParams) -> ();
            fn flush_thread(thread_id: ThreadId) -> ();
            fn shutdown_thread(thread_id: ThreadId) -> ();
            fn discard_thread(thread_id: ThreadId) -> ();
            fn load_history(params: store::LoadThreadHistoryParams) -> store::StoredThreadHistory;
            fn read_thread(params: store::ReadThreadParams) -> store::StoredThread;
            fn read_thread_by_rollout_path(params: store::ReadThreadByRolloutPathParams) -> store::StoredThread;
            fn list_threads(params: store::ListThreadsParams) -> store::ThreadPage;
            fn update_thread_metadata(params: store::UpdateThreadMetadataParams) -> Option<store::StoredThread>;
            fn archive_thread(params: store::ArchiveThreadParams) -> ();
            fn unarchive_thread(params: store::ArchiveThreadParams) -> store::StoredThread;
            fn delete_thread(params: store::DeleteThreadParams) -> ();
        }
        fn persist_thread(
            &self,
            thread_id: ThreadId,
            context: store::PersistContext,
        ) -> store::ThreadStoreFuture<'_, ()> {
            self.inner.persist_thread(thread_id, context)
        }
        fn list_turns(&self, _: ListTurnsParams) -> store::ThreadStoreFuture<'_, store::TurnPage> {
            Box::pin(async {
                Ok(store::TurnPage {
                    turns: vec![self.turn.clone()],
                    next_cursor: None,
                    backwards_cursor: None,
                })
            })
        }
        fn load_turn_terminal_event(
            &self,
            _: LoadThreadHistoryParams,
            _: String,
        ) -> store::ThreadStoreFuture<'_, Option<EventMsg>> {
            Box::pin(async {
                match self.terminal {
                    TerminalLookup::Unsupported => Err(store::ThreadStoreError::Unsupported {
                        operation: "load_turn_terminal_event",
                    }),
                    TerminalLookup::Failed => Err(store::ThreadStoreError::Internal {
                        message: "unreadable rollout".to_owned(),
                    }),
                    TerminalLookup::Missing => Ok(None),
                }
            })
        }
    }

    async fn paginated_child_with_index(
        status: StoredTurnStatus,
        error: Option<&str>,
        terminal: TerminalLookup,
    ) -> Session {
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
        session
            .state
            .lock()
            .await
            .session_configuration
            .history_mode = ThreadHistoryMode::Paginated;
        session.services.thread_store = Arc::new(StatusStore {
            inner: Arc::clone(&session.services.thread_store),
            turn: store::StoredTurn {
                turn_id: "indexed-turn".to_owned(),
                root_turn_id: None,
                items: Vec::new(),
                items_view: StoredTurnItemsView::NotLoaded,
                status,
                error: error.map(|message| store::StoredTurnError {
                    message: message.to_owned(),
                    codex_error_info: None,
                    additional_details: None,
                }),
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
            terminal,
        });
        session
    }

    fn completed(turn_id: &str, message: &str) -> RolloutItem {
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: turn_id.to_owned(),
            last_agent_message: Some(message.to_owned()),
            root_turn_id: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }))
    }

    #[tokio::test]
    async fn unavailable_terminal_event_preserves_unknown_or_durable_failure() {
        for terminal in [
            TerminalLookup::Unsupported,
            TerminalLookup::Failed,
            TerminalLookup::Missing,
        ] {
            for (status, error, expected) in [
                (StoredTurnStatus::Completed, None, AgentStatus::PendingInit),
                (
                    StoredTurnStatus::Failed,
                    None,
                    AgentStatus::Errored("Persisted turn failed".to_owned()),
                ),
                (
                    StoredTurnStatus::Failed,
                    Some("recorded failure"),
                    AgentStatus::Errored("recorded failure".to_owned()),
                ),
            ] {
                let session = paginated_child_with_index(status, error, terminal).await;
                session.restore_resumed_agent_status(&[]).await;
                assert_eq!(*session.agent_status.borrow(), expected);
            }
        }
    }

    #[tokio::test]
    async fn canonical_turn_boundaries_win_over_a_stale_projection() {
        for status in [StoredTurnStatus::Completed, StoredTurnStatus::InProgress] {
            let session =
                paginated_child_with_index(status, None, TerminalLookup::Unsupported).await;
            for items in [
                vec![
                    completed("indexed-turn", "older result"),
                    completed("newer-turn", "newer result"),
                ],
                vec![completed("newer-turn", "newer result")],
            ] {
                session.restore_resumed_agent_status(&items).await;
                assert_eq!(
                    *session.agent_status.borrow(),
                    AgentStatus::Completed(Some("newer result".to_owned()))
                );
            }
        }
    }

    #[tokio::test]
    async fn every_persisted_abort_restores_interrupted_work() {
        for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
            let session = paginated_child_with_index(
                StoredTurnStatus::Completed,
                None,
                TerminalLookup::Unsupported,
            )
            .await;
            session
                .state
                .lock()
                .await
                .session_configuration
                .history_mode = history_mode;
            for reason in [
                TurnAbortReason::Interrupted,
                TurnAbortReason::BudgetLimited,
                TurnAbortReason::Replaced,
                TurnAbortReason::ReviewEnded,
            ] {
                session
                    .restore_resumed_agent_status(&[
                        completed("indexed-turn", "older result"),
                        RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
                            turn_id: Some("aborted-turn".to_owned()),
                            root_turn_id: None,
                            reason,
                            error: None,
                            started_at: None,
                            completed_at: None,
                            duration_ms: None,
                        })),
                    ])
                    .await;
                assert_eq!(*session.agent_status.borrow(), AgentStatus::Interrupted);
            }
        }
    }

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
