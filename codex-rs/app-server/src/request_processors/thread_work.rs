use super::ThreadRequestProcessor;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionRequestId;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ThreadWorkShutdownIfQuiescentParams;
use codex_app_server_protocol::ThreadWorkShutdownIfQuiescentResponse;
use codex_app_server_protocol::ThreadWorkShutdownOutcome;
use codex_app_server_protocol::ThreadWorkSnapshot;
use codex_app_server_protocol::ThreadWorkSubscribeOutcome;
use codex_app_server_protocol::ThreadWorkSubscribeParams;
use codex_app_server_protocol::ThreadWorkSubscribeResponse;
use codex_core::GuardedShutdownOutcome;
use codex_core::WorkObservation;
use codex_core::WorkObservationSnapshot;
use codex_protocol::ThreadId;

impl ThreadRequestProcessor {
    pub(crate) async fn thread_work_subscribe(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadWorkSubscribeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_request(format!("invalid thread id: {error}")))?;
        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;
        let Some(observation) = thread.work_observation() else {
            return Ok(Some(
                ThreadWorkSubscribeResponse {
                    outcome: ThreadWorkSubscribeOutcome::Unavailable,
                    snapshot: None,
                }
                .into(),
            ));
        };
        if self
            .thread_state_manager
            .lifecycle_observer_owner(thread_id)
            .await
            != Some(request_id.connection_id)
        {
            return Err(invalid_request(
                "this connection does not own the thread's lifecycle observation",
            ));
        }
        let (snapshot, _updates) = observation.subscribe();
        Ok(Some(
            ThreadWorkSubscribeResponse {
                outcome: ThreadWorkSubscribeOutcome::Subscribed,
                snapshot: Some(snapshot_to_protocol(snapshot)?),
            }
            .into(),
        ))
    }

    pub(crate) async fn thread_work_shutdown_if_quiescent(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadWorkShutdownIfQuiescentParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, observation) = self
            .work_observation_for_owner(request_id, &params.thread_id)
            .await?;
        let (outcome, snapshot) = match observation.shutdown_if_quiescent(&params.revision) {
            GuardedShutdownOutcome::Closed(snapshot) => {
                (ThreadWorkShutdownOutcome::Closed, snapshot)
            }
            GuardedShutdownOutcome::AlreadyClosed(snapshot) => {
                (ThreadWorkShutdownOutcome::AlreadyClosed, snapshot)
            }
            GuardedShutdownOutcome::ObservationInactive(snapshot) => {
                (ThreadWorkShutdownOutcome::ObservationInactive, snapshot)
            }
            GuardedShutdownOutcome::StaleRevision(snapshot) => {
                (ThreadWorkShutdownOutcome::StaleRevision, snapshot)
            }
            GuardedShutdownOutcome::NotQuiescent(snapshot) => {
                (ThreadWorkShutdownOutcome::NotQuiescent, snapshot)
            }
        };
        Ok(Some(
            ThreadWorkShutdownIfQuiescentResponse {
                outcome,
                snapshot: snapshot_to_protocol(snapshot)?,
            }
            .into(),
        ))
    }

    async fn work_observation_for_owner(
        &self,
        request_id: &ConnectionRequestId,
        raw_thread_id: &str,
    ) -> Result<(ThreadId, WorkObservation), JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(raw_thread_id)
            .map_err(|error| invalid_request(format!("invalid thread id: {error}")))?;
        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;
        if self
            .thread_state_manager
            .lifecycle_observer_owner(thread_id)
            .await
            != Some(request_id.connection_id)
        {
            return Err(invalid_request(
                "this connection does not own the thread's lifecycle observation",
            ));
        }
        let observation = thread
            .work_observation()
            .ok_or_else(|| invalid_request("root work state observation is unavailable"))?;
        Ok((thread_id, observation))
    }
}

pub(super) fn snapshot_to_protocol(
    snapshot: WorkObservationSnapshot,
) -> Result<ThreadWorkSnapshot, JSONRPCErrorError> {
    let count = |value: usize| {
        u32::try_from(value)
            .map_err(|_| internal_error("root work snapshot count exceeds the v2 protocol bound"))
    };
    Ok(ThreadWorkSnapshot {
        revision: snapshot.revision,
        outstanding_work: count(snapshot.outstanding_work)?,
        running_finite_work: count(snapshot.running_finite_work)?,
        pending_notifications: count(snapshot.pending_notifications)?,
        active_root_turns: count(snapshot.active_root_turns)?,
        pending_terminal_outputs: count(snapshot.pending_terminal_outputs)?,
        output_forwarding_observed: snapshot.output_forwarding_observed,
        closed: snapshot.closed,
        quiescent: snapshot.quiescent,
    })
}
