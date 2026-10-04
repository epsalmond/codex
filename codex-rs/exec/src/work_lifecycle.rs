//! Exec's event-driven drain of one app-server-owned execution scope.

use crate::RequestIdSequencer;
use crate::event_processor::EventProcessor;
use crate::event_processor::print_final_output_before_cleanup;
use crate::request_shutdown;
use crate::send_request_with_response;
use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadWorkShutdownIfQuiescentParams;
use codex_app_server_protocol::ThreadWorkShutdownIfQuiescentResponse;
use codex_app_server_protocol::ThreadWorkShutdownOutcome;
use codex_app_server_protocol::ThreadWorkSnapshot;
use codex_app_server_protocol::ThreadWorkSubscribeParams;
use codex_app_server_protocol::ThreadWorkSubscribeResponse;
use codex_app_server_protocol::ThreadWorkUpdatedNotification;
use tracing::warn;

pub(crate) struct WorkLifecycle {
    thread_id: String,
    snapshot: ThreadWorkSnapshot,
    /// Exec's latest root turn has not completed. Exec subscribes before it starts the first turn,
    /// and core admits that turn asynchronously (`review/start` only queues it), so snapshots
    /// published before then can be quiescent without covering the turn.
    root_turn_open: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseOutcome {
    Closed,
    KeepWaiting,
}

/// How a drained run left the event loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DrainExit {
    pub(crate) error: bool,
    pub(crate) final_output_attempted: bool,
}

impl WorkLifecycle {
    pub(crate) async fn subscribe(
        client: &InProcessAppServerClient,
        request_ids: &mut RequestIdSequencer,
        thread_id: &str,
    ) -> Result<Option<Self>, String> {
        let response: ThreadWorkSubscribeResponse = send_request_with_response(
            client,
            ClientRequest::ThreadWorkSubscribe {
                request_id: request_ids.next(),
                params: ThreadWorkSubscribeParams {
                    thread_id: thread_id.to_string(),
                },
            },
            "thread/subscribeWorkState",
        )
        .await?;
        let snapshot = match (response.outcome, response.snapshot) {
            (codex_app_server_protocol::ThreadWorkSubscribeOutcome::Subscribed, Some(snapshot)) => {
                snapshot
            }
            (codex_app_server_protocol::ThreadWorkSubscribeOutcome::Unavailable, None) => {
                return Ok(None);
            }
            _ => {
                return Err(
                    "thread/subscribeWorkState: inconsistent capability response".to_string(),
                );
            }
        };
        Ok(Some(Self {
            thread_id: thread_id.to_string(),
            snapshot,
            root_turn_open: true,
        }))
    }

    pub(crate) fn root_turn_started(&mut self) {
        self.root_turn_open = true;
    }

    pub(crate) fn root_turn_completed(&mut self) {
        self.root_turn_open = false;
    }

    pub(crate) fn root_turn_open(&self) -> bool {
        self.root_turn_open
    }

    pub(crate) fn observe(&mut self, notification: &ThreadWorkUpdatedNotification) -> bool {
        if notification.thread_id != self.thread_id {
            return false;
        }
        self.snapshot.clone_from(&notification.snapshot);
        true
    }

    /// Close only after the latest root turn completed and the tree is quiescent.
    pub(crate) fn ready_to_close(&self) -> bool {
        !self.root_turn_open && self.snapshot.quiescent
    }

    /// Closes the scope once it is ready, then writes the final output before tearing down.
    /// Returns `None` while exec must keep waiting.
    pub(crate) async fn finish_if_drained(
        &mut self,
        client: &InProcessAppServerClient,
        request_ids: &mut RequestIdSequencer,
        event_processor: &mut dyn EventProcessor,
    ) -> Option<DrainExit> {
        if !self.ready_to_close() {
            return None;
        }
        match self.close_if_quiescent(client, request_ids).await {
            Ok(CloseOutcome::KeepWaiting) => None,
            Ok(CloseOutcome::Closed) => {
                let (output_result, unsubscribe_result) = print_final_output_before_cleanup(
                    event_processor,
                    request_shutdown(client, request_ids, &self.thread_id),
                )
                .await;
                let mut error = false;
                if let Err(err) = output_result {
                    // The default exec log filter hides `warn!`, so report this directly.
                    #[allow(clippy::print_stderr)]
                    {
                        eprintln!("Failed to write final output: {err}");
                    }
                    error = true;
                }
                if let Err(err) = unsubscribe_result {
                    warn!("thread/unsubscribe failed after work drain: {err}");
                    error = true;
                }
                Some(DrainExit {
                    error,
                    final_output_attempted: true,
                })
            }
            Err(error) => {
                warn!("failed to close the drained work scope: {error}");
                Some(DrainExit {
                    error: true,
                    final_output_attempted: false,
                })
            }
        }
    }

    async fn close_if_quiescent(
        &mut self,
        client: &InProcessAppServerClient,
        request_ids: &mut RequestIdSequencer,
    ) -> Result<CloseOutcome, String> {
        if !self.ready_to_close() {
            return Ok(CloseOutcome::KeepWaiting);
        }
        let response: ThreadWorkShutdownIfQuiescentResponse = send_request_with_response(
            client,
            ClientRequest::ThreadWorkShutdownIfQuiescent {
                request_id: request_ids.next(),
                params: ThreadWorkShutdownIfQuiescentParams {
                    thread_id: self.thread_id.clone(),
                    revision: self.snapshot.revision.clone(),
                },
            },
            "thread/shutdownIfQuiescent",
        )
        .await?;
        self.snapshot.clone_from(&response.snapshot);
        match response.outcome {
            ThreadWorkShutdownOutcome::Closed | ThreadWorkShutdownOutcome::AlreadyClosed => {
                Ok(CloseOutcome::Closed)
            }
            ThreadWorkShutdownOutcome::StaleRevision | ThreadWorkShutdownOutcome::NotQuiescent => {
                Ok(CloseOutcome::KeepWaiting)
            }
            ThreadWorkShutdownOutcome::ObservationInactive => Err(
                "thread/shutdownIfQuiescent: observation became inactive during drain".to_string(),
            ),
        }
    }
}

/// What a Ctrl-C does while exec drains the root's work scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrainInterrupt {
    /// Interrupt the active root turn and keep draining.
    InterruptTurn,
    /// Stop draining and tear the session down.
    StopDraining,
}

/// The first Ctrl-C during a root turn interrupts it, while a Ctrl-C between turns, or any later
/// Ctrl-C, stops the drain instead of waiting for children to report.
#[derive(Debug, Default)]
pub(crate) struct DrainInterrupts {
    interrupt_sent: bool,
}

impl DrainInterrupts {
    pub(crate) fn on_ctrl_c(&mut self, root_turn_open: bool) -> DrainInterrupt {
        if root_turn_open && !self.interrupt_sent {
            self.interrupt_sent = true;
            DrainInterrupt::InterruptTurn
        } else {
            DrainInterrupt::StopDraining
        }
    }
}

#[cfg(test)]
#[path = "work_lifecycle_tests.rs"]
mod tests;
