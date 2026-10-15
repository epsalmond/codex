//! One receiver lease shared by sampling and every live cell.
//!
//! A sampling step and each live cell hold a [`CodeModeDispatchWorker`]. The
//! receiver keeps serving nested `tools.*` calls and notifications while any
//! lease is alive, so a cell that outlives its turn can still finish. Each call
//! runs with the cell's original step, router and diff tracker. The receiver
//! stops when the last lease drops (the last live cell closed and no turn is
//! sampling) or when the Code Mode service shuts down.
use super::CodeModeDispatchBroker;
use super::DispatchInterruption;
use super::DispatchMessage;
use super::ExecutedToolCalls;
use super::PUBLIC_TOOL_NAME;
use super::remove_dispatch_gate;
use super::wait_until_cell_ready_for_dispatch;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::code_mode::submit_nested_tool;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::sync::Weak;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

impl CodeModeDispatchBroker {
    /// Returns a lease on the shared receiver, starting it if no lease is alive.
    pub(in crate::tools::code_mode) fn start_worker(
        &self,
        session: &Arc<Session>,
    ) -> CodeModeDispatchWorker {
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(lease) = worker.upgrade() {
            return CodeModeDispatchWorker { _lease: lease };
        }
        let lease = Arc::new(WorkerLease {
            stop: self.shutdown.child_token(),
        });
        *worker = Arc::downgrade(&lease);
        let stop = lease.stop.clone();
        // The receiver must not keep its owner alive.
        let host = Arc::new(CoreTurnHost {
            session: Arc::downgrade(session),
            parallel_execution: Arc::new(tokio::sync::RwLock::new(())),
        });
        let dispatch_rx = self.dispatch_rx.clone();
        let dispatch_gates = Arc::clone(&self.dispatch_gates);
        let receiver_permit = Arc::clone(&self.receiver_permit);
        let tasks = self.tasks.clone();
        self.tasks.spawn(async move {
            // A replacement lease cannot read until the retiring receiver has stopped.
            let _receiver = tokio::select! {
                biased;
                _ = stop.cancelled() => return,
                permit = receiver_permit.lock_owned() => permit,
            };
            loop {
                let message = tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    message = dispatch_rx.recv() => message.ok(),
                };
                let Some(message) = message else {
                    break;
                };
                match message {
                    DispatchMessage::Notify {
                        call_id,
                        cell_id,
                        text,
                        step_context,
                        output_token_limit,
                        cancellation_token,
                        response_tx,
                    } => {
                        let response = if wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                            &stop,
                        )
                        .await
                        {
                            match step_context.upgrade() {
                                Some(step_context) => tokio::select! {
                                    _ = stop.cancelled() => Err("code mode notification cancelled".to_owned()),
                                    response = host.notify(call_id, cell_id, text, output_token_limit, step_context) => response,
                                },
                                None => Err("code mode notification cancelled".to_owned()),
                            }
                        } else {
                            remove_dispatch_gate(&dispatch_gates, &cell_id);
                            Err("code mode notification cancelled".to_string())
                        };
                        let _ = response_tx.send(response);
                    }
                    DispatchMessage::InvokeTool {
                        invocation,
                        step_context,
                        tracker,
                        mut dispatch_trace,
                        cancellation_token,
                        response_tx,
                    } => {
                        let cell_id = invocation.cell_id.clone();
                        if !wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                            &stop,
                        )
                        .await
                        {
                            let outcome = if cancellation_token.is_cancelled() {
                                DispatchInterruption::Cancelled
                            } else {
                                DispatchInterruption::CellClosed
                            };
                            dispatch_trace.interruption = Some(outcome);
                            remove_dispatch_gate(&dispatch_gates, &cell_id);
                            continue;
                        }
                        let Some(step_context) = step_context
                            .upgrade()
                            .filter(|_| !cancellation_token.is_cancelled())
                        else {
                            let _ = response_tx
                                .send(Err("code mode nested tool call cancelled".to_string()));
                            continue;
                        };
                        let track_completeness =
                            ExecutedToolCalls::is_enabled(&step_context.turn.config.features);
                        let host = Arc::clone(&host);
                        let dispatch_gates = Arc::clone(&dispatch_gates);
                        let span = dispatch_trace.span.clone();
                        let stop = stop.clone();
                        tasks.spawn(async move {
                            let invocation = {
                                let dispatch_gate = track_completeness.then(|| {
                                    dispatch_gates
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                });
                                if dispatch_gate.as_ref().is_some_and(|gates| {
                                    cancellation_token.is_cancelled()
                                        || !gates.contains_key(&cell_id)
                                }) {
                                    let outcome = if cancellation_token.is_cancelled() {
                                        DispatchInterruption::Cancelled
                                    } else {
                                        DispatchInterruption::CellClosed
                                    };
                                    dispatch_trace.interruption = Some(outcome);
                                    return;
                                }
                                // Submission and cell closure share this gate.
                                span.in_scope(|| {
                                    dispatch_trace.interruption = None;
                                    host.submit_tool(
                                        invocation,
                                        step_context,
                                        tracker,
                                        dispatch_trace.call_id.clone(),
                                        cancellation_token.clone(),
                                    )
                                })
                                .instrument(span)
                            };
                            tokio::pin!(invocation);
                            let response = tokio::select! {
                                biased;
                                _ = stop.cancelled() => Err("code mode nested tool call cancelled".to_owned()),
                                _ = cancellation_token.cancelled() => invocation.await,
                                response = &mut invocation => response,
                            };
                            let _ = response_tx.send(response);
                        });
                    }
                }
            }
        });
        CodeModeDispatchWorker { _lease: lease }
    }

    /// Stops the receiver and every in-flight nested call, then waits for them.
    pub(in crate::tools::code_mode) async fn shutdown(&self) {
        self.shutdown.cancel();
        self.dispatch_tx.close();
        while let Ok(message) = self.dispatch_rx.try_recv() {
            match message {
                DispatchMessage::InvokeTool { response_tx, .. } => {
                    let _ = response_tx
                        .send(Err("code mode nested tool dispatcher stopped".to_owned()));
                }
                DispatchMessage::Notify { response_tx, .. } => {
                    let _ = response_tx
                        .send(Err("code mode notification dispatcher stopped".to_owned()));
                }
            }
        }
        self.tasks.close();
        self.tasks.wait().await;
    }

    #[cfg(test)]
    pub(in crate::tools::code_mode) fn has_live_worker(&self) -> bool {
        self.worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .strong_count()
            > 0
    }
}

/// Keeps the shared nested-call receiver running while held.
#[derive(Clone)]
pub(crate) struct CodeModeDispatchWorker {
    _lease: Arc<WorkerLease>,
}

pub(super) struct WorkerLease {
    stop: CancellationToken,
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct CoreTurnHost {
    session: Weak<Session>,
    parallel_execution: Arc<tokio::sync::RwLock<()>>,
}

impl CoreTurnHost {
    // Submit synchronously so the recorder sees the call before the cell's dispatch gate closes.
    fn submit_tool(
        &self,
        invocation: CodeModeNestedToolCall,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
        call_id: String,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<JsonValue, String>> + Send + 'static {
        let submitted = self
            .session
            .upgrade()
            .ok_or_else(|| "code mode owner stopped".to_owned())
            .and_then(|session| {
                let tool_runtime = ToolCallRuntime::for_retained_cell(
                    Arc::clone(&session),
                    Arc::clone(&step_context),
                    tracker,
                    Arc::clone(&self.parallel_execution),
                );
                submit_nested_tool(
                    session,
                    step_context,
                    tool_runtime,
                    invocation,
                    call_id,
                    cancellation_token,
                )
                .map_err(|error| error.to_string())
            });
        async move { submitted?.await.map_err(|error| error.to_string()) }
    }

    async fn notify(
        &self,
        call_id: String,
        cell_id: CellId,
        text: String,
        output_token_limit: usize,
        step_context: Arc<StepContext>,
    ) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| format!("code mode owner stopped before cell {cell_id} notified"))?;
        let mut item = ResponseItem::CustomToolCallOutput {
            id: None,
            call_id,
            name: Some(PUBLIC_TOOL_NAME.to_string()),
            output: FunctionCallOutputPayload::from_text(text),
            internal_chat_message_metadata_passthrough: None,
        };
        item.set_turn_id_if_missing(&step_context.turn.sub_id);
        let items = vec![ResponseItemEnvelope {
            item,
            metadata: Some(CodexHarnessMetadata {
                history_truncation_token_limit: Some(output_token_limit),
                ..Default::default()
            }),
        }];
        // An idle owner keeps the notification in history for its next turn.
        if let Err(items) = session.inject_if_running(items).await {
            session
                .record_annotated_conversation_items(
                    &step_context.turn,
                    &step_context.settings.model_info,
                    items,
                )
                .await;
        }
        Ok(())
    }
}
