//! Terminal-output delivery tied to the root thread's controlling lifecycle observer.

use super::ConnectionId;
use super::OutgoingEnvelope;
use super::OutgoingMessageSender;
use super::ServerNotification;
use super::ThreadScopedOutgoingMessageSender;
use super::timestamped_server_notification;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::warn;

impl ThreadScopedOutgoingMessageSender {
    pub(crate) fn with_lifecycle_observer_owner(mut self, owner: Option<ConnectionId>) -> Self {
        self.lifecycle_observer_owner = owner;
        self
    }

    /// Lets the listener abandon the owner's write ack without cancelling event handling.
    pub(crate) fn with_terminal_ack_cancel(mut self, cancel: CancellationToken) -> Self {
        self.terminal_ack_cancel = Some(cancel);
        self
    }

    /// Send a terminal turn notification and wait for the controlling observer's write ack.
    /// `None` means the thread has no active lifecycle observer and preserves normal fanout.
    /// Only the ack wait is cancellable; a cancelled wait reports the output as undelivered.
    pub(crate) async fn send_terminal_server_notification(
        &self,
        notification: ServerNotification,
    ) -> Option<bool> {
        let Some(owner) = self.lifecycle_observer_owner else {
            self.send_server_notification(notification).await;
            return None;
        };
        self.outgoing
            .analytics_events_client
            .track_notification(&notification);
        let owner_ack = self
            .outgoing
            .send_server_notification_to_connections_for_owner_ack(
                self.connection_ids.as_slice(),
                owner,
                notification,
            )
            .await;
        let delivered = match (owner_ack, &self.terminal_ack_cancel) {
            (None, _) => false,
            (Some(owner_ack), None) => owner_ack.await.is_ok(),
            (Some(owner_ack), Some(cancel)) => tokio::select! {
                acknowledged = owner_ack => acknowledged.is_ok(),
                _ = cancel.cancelled() => false,
            },
        };
        if !delivered {
            warn!(
                thread_id = %self.thread_id,
                owner_connection_id = ?owner,
                "terminal turn notification was not acknowledged by its lifecycle observer"
            );
        }
        Some(delivered)
    }
}

impl OutgoingMessageSender {
    /// Returns the owner's write-ack receiver, or `None` if the notification could not be queued
    /// for the owner.
    async fn send_server_notification_to_connections_for_owner_ack(
        &self,
        connection_ids: &[ConnectionId],
        owner: ConnectionId,
        notification: ServerNotification,
    ) -> Option<oneshot::Receiver<()>> {
        tracing::trace!(
            targeted_connections = connection_ids.len(),
            "app-server terminal event: {notification}"
        );
        let outgoing_message = timestamped_server_notification(notification);
        let mut owner_write_complete_rx = None;
        for connection_id in connection_ids {
            let write_complete_tx = if *connection_id == owner {
                let (write_complete_tx, write_complete_rx) = tokio::sync::oneshot::channel();
                owner_write_complete_rx = Some(write_complete_rx);
                Some(write_complete_tx)
            } else {
                None
            };
            if let Err(error) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id: *connection_id,
                    message: outgoing_message.clone(),
                    write_complete_tx,
                })
                .await
            {
                warn!("failed to send terminal notification to client: {error:?}");
                return None;
            }
        }
        owner_write_complete_rx
    }
}
