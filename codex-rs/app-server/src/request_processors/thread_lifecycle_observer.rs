//! Connection ownership and output delivery for observed root work.

use crate::error_code::internal_error;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use crate::thread_state::LifecycleObserverClaimError;
use crate::thread_state::ThreadStateManager;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::WorkObservation;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use std::future::Future;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) enum LifecycleObserverRegistration {
    Inactive,
    Active,
}

pub(super) async fn bind_exec_root_lifecycle_observer(
    manager: &ThreadStateManager,
    session_source: &SessionSource,
    thread_id: ThreadId,
    connection_id: ConnectionId,
    observation: Option<WorkObservation>,
) -> Result<LifecycleObserverRegistration, JSONRPCErrorError> {
    if !matches!(session_source, SessionSource::Exec) {
        return Ok(LifecycleObserverRegistration::Inactive);
    }
    let Some(observation) = observation else {
        return Ok(LifecycleObserverRegistration::Inactive);
    };
    bind_lifecycle_observer(manager, thread_id, connection_id, &observation).await?;
    Ok(LifecycleObserverRegistration::Active)
}

async fn bind_lifecycle_observer(
    manager: &ThreadStateManager,
    thread_id: ThreadId,
    connection_id: ConnectionId,
    observation: &WorkObservation,
) -> Result<(), JSONRPCErrorError> {
    manager
        .claim_lifecycle_observer(thread_id, connection_id)
        .await
        .map_err(|error| match error {
            LifecycleObserverClaimError::ConnectionNotSubscribed => internal_error(format!(
                "connection {connection_id} is not subscribed to lifecycle thread {thread_id}"
            )),
            LifecycleObserverClaimError::AlreadyClaimed { owner } => internal_error(format!(
                "lifecycle thread {thread_id} is already controlled by connection {owner}"
            )),
        })?;
    observation.subscribe();
    Ok(())
}

pub(super) async fn thread_scoped_outgoing(
    manager: &ThreadStateManager,
    outgoing: Arc<OutgoingMessageSender>,
    connection_ids: Vec<ConnectionId>,
    thread_id: ThreadId,
) -> (ThreadScopedOutgoingMessageSender, Option<ConnectionId>) {
    let owner = manager.lifecycle_observer_owner(thread_id).await;
    (
        ThreadScopedOutgoingMessageSender::new(outgoing, connection_ids, thread_id)
            .with_lifecycle_observer_owner(owner),
        owner,
    )
}

/// Runs event handling to completion. A listener cancellation that arrives meanwhile only
/// abandons a pending terminal-output ack, so handling never stops partway through an event.
/// Returns `false` when the listener should stop after this event.
pub(super) async fn complete_event_handling(
    cancel_rx: &mut tokio::sync::oneshot::Receiver<()>,
    terminal_ack_cancel: &CancellationToken,
    event_handling: impl Future<Output = ()>,
) -> bool {
    tokio::pin!(event_handling);
    tokio::select! {
        biased;
        _ = &mut event_handling => return true,
        _ = &mut *cancel_rx => {}
    }
    terminal_ack_cancel.cancel();
    event_handling.await;
    false
}

pub(super) async fn complete_event_handling_or_cancel(
    cancel_rx: &mut tokio::sync::oneshot::Receiver<()>,
    event_handling: impl Future<Output = ()>,
) -> bool {
    tokio::select! {
        biased;
        _ = event_handling => true,
        _ = &mut *cancel_rx => false,
    }
}

#[cfg(test)]
#[path = "thread_lifecycle_observer_tests.rs"]
mod tests;
