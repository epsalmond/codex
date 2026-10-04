use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::thread_state::ThreadListenerCommand;
use crate::thread_state::ThreadStateManager;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadWorkUpdatedNotification;
use codex_core::CodexThread;
use codex_core::WorkObservationSnapshot;
use codex_protocol::ThreadId;
use tokio::sync::oneshot;
use tokio::sync::watch;

/// Carries the pinned owner's latest-state watcher for root work snapshots.
pub(super) struct ThreadWorkUpdates {
    thread_id: ThreadId,
    owner_connection_id: ConnectionId,
    receiver: watch::Receiver<WorkObservationSnapshot>,
}

impl ThreadWorkUpdates {
    pub(super) async fn for_thread(
        thread_state_manager: &ThreadStateManager,
        conversation: &CodexThread,
        thread_id: ThreadId,
    ) -> Result<Option<Self>, JSONRPCErrorError> {
        let Some(owner_connection_id) = thread_state_manager
            .lifecycle_observer_owner(thread_id)
            .await
        else {
            return Ok(None);
        };
        let Some(observation) = conversation.work_observation() else {
            return Ok(None);
        };
        let (_, mut receiver) = observation.subscribe();
        receiver.mark_changed();
        Ok(Some(Self {
            thread_id,
            owner_connection_id,
            receiver,
        }))
    }

    pub(super) fn into_listener_command(self) -> ThreadListenerCommand {
        ThreadListenerCommand::SetThreadWorkUpdates {
            owner_connection_id: self.owner_connection_id,
            thread_id: self.thread_id,
            receiver: self.receiver,
        }
    }

    pub(super) fn from_listener_parts(
        owner_connection_id: ConnectionId,
        thread_id: ThreadId,
        mut receiver: watch::Receiver<WorkObservationSnapshot>,
    ) -> Self {
        receiver.mark_changed();
        Self {
            thread_id,
            owner_connection_id,
            receiver,
        }
    }

    pub(super) async fn next_notification(
        &mut self,
    ) -> Option<Result<ServerNotification, JSONRPCErrorError>> {
        if self.receiver.changed().await.is_err() {
            return None;
        }
        let snapshot = self.receiver.borrow_and_update().clone();
        Some(
            super::thread_work::snapshot_to_protocol(snapshot).map(|snapshot| {
                ServerNotification::ThreadWorkUpdated(ThreadWorkUpdatedNotification {
                    thread_id: self.thread_id.to_string(),
                    snapshot,
                })
            }),
        )
    }

    pub(super) async fn deliver_or_cancel(
        &self,
        outgoing: &OutgoingMessageSender,
        cancel_rx: &mut oneshot::Receiver<()>,
        notification: ServerNotification,
    ) -> bool {
        let recipients = [self.owner_connection_id];
        let delivery = outgoing.send_server_notification_to_connections(&recipients, notification);
        if super::thread_lifecycle_observer::complete_event_handling_or_cancel(cancel_rx, delivery)
            .await
        {
            return true;
        }
        tracing::warn!(
            thread_id = %self.thread_id,
            owner_connection_id = ?self.owner_connection_id,
            "root work state notification cancelled while listener was shutting down"
        );
        false
    }
}

#[cfg(test)]
#[path = "thread_work_updates_tests.rs"]
mod tests;
