//! Bind observed roots before a newly installed listener can drain terminal events.

use super::thread_lifecycle::EnsureConversationListenerResult;
use super::thread_lifecycle::ListenerTaskContext;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionId;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::WorkObservation;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;

#[expect(
    clippy::await_holding_invalid_type,
    reason = "root observer binding must be serialized against pending unloads"
)]
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
    let conversation = listener_task_context
        .thread_manager
        .get_thread(conversation_id)
        .await
        .map_err(|_| invalid_request(format!("thread not found: {conversation_id}")))?;
    if conversation.has_shutdown_status() {
        return Err(super::thread_lifecycle::thread_closing_for_resume(
            conversation_id,
        ));
    }
    let lifecycle_observer_active = {
        let pending = listener_task_context.pending_thread_unloads.lock().await;
        if pending.contains_key(&conversation_id) {
            return Err(invalid_request(format!(
                "thread {conversation_id} is closing; retry after the thread is closed"
            )));
        }
        // Claim the output owner before listener startup: queued terminal events must see it.
        // The normal listener transaction below validates the runtime and installs its drain.
        let attached = listener_task_context
            .thread_state_manager
            .try_add_connection_to_thread(conversation_id, connection_id)
            .await;
        if !attached && matches!(session_source, SessionSource::Exec) && observation.is_some() {
            return Err(internal_error(format!(
                "failed to attach lifecycle observer to root thread {conversation_id}"
            )));
        }
        if attached {
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
        }
    };
    let result = super::thread_lifecycle::ensure_conversation_listener(
        listener_task_context.clone(),
        conversation_id,
        connection_id,
        raw_events_enabled,
    )
    .await;
    if result.is_err() {
        let _ = listener_task_context
            .thread_state_manager
            .unsubscribe_connection_from_thread(conversation_id, connection_id)
            .await;
    }
    Ok((result, lifecycle_observer_active))
}
