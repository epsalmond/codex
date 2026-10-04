use super::thread_lifecycle::EnsureConversationListenerResult;
use super::thread_lifecycle::ListenerTaskContext;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionId;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::WorkObservation;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;

pub(super) async fn ensure_conversation_listener(
    listener_task_context: ListenerTaskContext,
    conversation_id: ThreadId,
    connection_id: ConnectionId,
    raw_events_enabled: bool,
) -> Result<EnsureConversationListenerResult, JSONRPCErrorError> {
    ensure_conversation_listener_inner(
        listener_task_context,
        conversation_id,
        connection_id,
        raw_events_enabled,
        /*lifecycle*/ None,
    )
    .await
    .map(|(result, _)| result)
    .and_then(|result| result)
}

pub(super) async fn ensure_root_thread_listener(
    listener_task_context: ListenerTaskContext,
    conversation_id: ThreadId,
    connection_id: ConnectionId,
    raw_events_enabled: bool,
    session_source: &SessionSource,
    observation: Option<WorkObservation>,
) -> Result<
    (
        Result<EnsureConversationListenerResult, JSONRPCErrorError>,
        bool,
    ),
    JSONRPCErrorError,
> {
    ensure_conversation_listener_inner(
        listener_task_context,
        conversation_id,
        connection_id,
        raw_events_enabled,
        Some((session_source, observation)),
    )
    .await
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "listener subscription must be serialized against pending unloads"
)]
async fn ensure_conversation_listener_inner(
    listener_task_context: ListenerTaskContext,
    conversation_id: ThreadId,
    connection_id: ConnectionId,
    raw_events_enabled: bool,
    lifecycle: Option<(&SessionSource, Option<WorkObservation>)>,
) -> Result<
    (
        Result<EnsureConversationListenerResult, JSONRPCErrorError>,
        bool,
    ),
    JSONRPCErrorError,
> {
    let conversation = match listener_task_context
        .thread_manager
        .get_thread(conversation_id)
        .await
    {
        Ok(conversation) => conversation,
        Err(_) => {
            return Err(invalid_request(format!(
                "thread not found: {conversation_id}"
            )));
        }
    };
    let thread_state = {
        let pending_thread_unloads = listener_task_context.pending_thread_unloads.lock().await;
        if pending_thread_unloads.contains(&conversation_id) {
            return Err(invalid_request(format!(
                "thread {conversation_id} is closing; retry after the thread is closed"
            )));
        }
        let Some(thread_state) = listener_task_context
            .thread_state_manager
            .try_ensure_connection_subscribed(conversation_id, connection_id, raw_events_enabled)
            .await
        else {
            if lifecycle
                .as_ref()
                .is_some_and(|(session_source, observation)| {
                    matches!(session_source, SessionSource::Exec) && observation.is_some()
                })
            {
                return Err(internal_error(format!(
                    "failed to attach lifecycle observer to root thread {conversation_id}"
                )));
            }
            return Ok((
                Ok(EnsureConversationListenerResult::ConnectionClosed),
                false,
            ));
        };
        thread_state
    };
    let lifecycle_observer_active = if let Some((session_source, observation)) = lifecycle {
        matches!(
            super::thread_lifecycle_observer::bind_exec_root_lifecycle_observer(
                &listener_task_context.thread_state_manager,
                session_source,
                conversation_id,
                connection_id,
                observation,
            )
            .await?,
            super::thread_lifecycle_observer::LifecycleObserverRegistration::Active
        )
    } else {
        false
    };
    if let Err(error) = super::thread_lifecycle::ensure_listener_task_running(
        listener_task_context.clone(),
        conversation_id,
        conversation,
        thread_state,
    )
    .await
    {
        let _ = listener_task_context
            .thread_state_manager
            .unsubscribe_connection_from_thread(conversation_id, connection_id)
            .await;
        return Ok((Err(error), lifecycle_observer_active));
    }
    Ok((
        Ok(EnsureConversationListenerResult::Attached),
        lifecycle_observer_active,
    ))
}
