use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use crate::agent::control::OwnedWorkRegistration;
use crate::tools::context::ToolCallOrigin;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ToolInvocationFuture;
use codex_protocol::ThreadId;
use codex_utils_output_truncation::with_serialization_allowance;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::PUBLIC_TOOL_NAME;
use super::cell_completion::CellCompletion;
use super::telemetry::DispatchInterruption;
use super::telemetry::NestedToolDispatchTrace;
use super::telemetry::trace_id;
use crate::session::step_context::StepContext;
use crate::tools::ExecutedToolCalls;
use crate::tools::call_trace;
use crate::tools::context::SharedTurnDiffTracker;

#[path = "dispatch_worker.rs"]
mod worker;
pub(crate) use worker::CodeModeDispatchWorker;
use worker::WorkerLease;

pub(super) struct CodeModeDispatchBroker {
    thread_id: ThreadId,
    dispatch_tx: async_channel::Sender<DispatchMessage>,
    dispatch_rx: async_channel::Receiver<DispatchMessage>,
    dispatch_gates: Arc<Mutex<HashMap<CellId, CellDispatchGate>>>,
    executed_tool_calls: ExecutedToolCalls,
    // One receiver serves sampling and every live cell; it stops when the last lease drops.
    worker: Mutex<Weak<WorkerLease>>,
    receiver_permit: Arc<tokio::sync::Mutex<()>>,
    tasks: TaskTracker,
    shutdown: CancellationToken,
    // Live cells with an automatic-completion observer; one outlives close while it collects.
    completions: Mutex<HashMap<CellId, Arc<CellCompletion>>>,
}

/// Retains the step advertised to one execution, including callbacks after it yields.
///
/// Nested calls keep running with this exact step, router and diff tracker even after
/// the turn that started the cell has ended. The step's sandbox and approval policy
/// are therefore frozen at `exec` time: later `/approvals` or permission changes
/// apply to new cells, not to a cell that is already running.
pub(super) struct CodeModeCellDelegate {
    pub(super) broker: Arc<CodeModeDispatchBroker>,
    pub(super) step_context: Arc<StepContext>,
    pub(super) tracker: SharedTurnDiffTracker,
    pub(super) completion: Option<Arc<CellCompletion>>,
    pub(super) work_registration: Mutex<Option<OwnedWorkRegistration>>,
}

struct CellDispatchGate {
    delegate: Weak<CodeModeCellDelegate>,
    ready: watch::Sender<bool>,
    // Callbacks may create the gate before exec attaches its origin. None means
    // no origin is attached; Some retains the original item and window across
    // waits, even if the best-effort item lookup found no ID.
    originating_call: Option<ToolCallOrigin>,
    // A live cell keeps the shared receiver alive until it closes.
    worker: Option<CodeModeDispatchWorker>,
}

impl CodeModeDispatchBroker {
    pub(super) fn new(thread_id: ThreadId, executed_tool_calls: ExecutedToolCalls) -> Self {
        let (dispatch_tx, dispatch_rx) = async_channel::unbounded();
        Self {
            thread_id,
            dispatch_tx,
            dispatch_rx,
            dispatch_gates: Arc::new(Mutex::new(HashMap::new())),
            executed_tool_calls,
            worker: Mutex::new(Weak::new()),
            receiver_permit: Arc::new(tokio::sync::Mutex::new(())),
            tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            completions: Mutex::new(HashMap::new()),
        }
    }

    /// Bind the existing live gate before nested calls can be released for dispatch.
    pub(super) fn attach_registration(
        &self,
        cell_id: &CellId,
        delegate: &Arc<CodeModeCellDelegate>,
    ) {
        // Close takes these locks in the same order. A callback that closed during execute
        // must not be resurrected when the provider returns its ID.
        let registration = delegate
            .work_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registration.is_none() {
            return;
        }
        let mut gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gate = gates
            .entry(cell_id.clone())
            .or_insert_with(|| CellDispatchGate {
                delegate: Weak::new(),
                ready: watch::channel(false).0,
                originating_call: None,
                worker: None,
            });
        gate.delegate = Arc::downgrade(delegate);
    }

    pub(super) fn work_registration(&self, cell_id: &CellId) -> Option<OwnedWorkRegistration> {
        let delegate = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id)?
            .delegate
            .upgrade()?;
        delegate
            .work_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Makes the cell's completion observable to `wait` and `terminate` until the cell closes.
    pub(super) fn attach_completion(&self, cell_id: &CellId, completion: Arc<CellCompletion>) {
        let mut completions = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !completion.is_closed() {
            completions.insert(cell_id.clone(), completion);
        }
    }

    pub(super) fn completion(&self, cell_id: &CellId) -> Option<Arc<CellCompletion>> {
        self.completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id)
            .cloned()
    }

    pub(super) fn mark_cell_ready_for_dispatch(
        &self,
        cell_id: &CellId,
        originating_call: Option<ToolCallOrigin>,
    ) {
        let ready = {
            let mut dispatch_gates = self
                .dispatch_gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // execute attached the live delegate before returning its ID. Native close may
            // already have removed that gate; an initial response cannot reopen it.
            let Some(gate) = dispatch_gates.get_mut(cell_id) else {
                return;
            };
            gate.originating_call = originating_call;
            gate.ready.clone()
        };
        ready.send_replace(true);
    }

    /// Keeps the dispatch worker alive for a live cell until the cell closes.
    ///
    /// A cell that already closed keeps no lease.
    pub(super) fn retain_worker_for_cell(&self, cell_id: &CellId, worker: CodeModeDispatchWorker) {
        if let Some(gate) = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(cell_id)
        {
            gate.worker = Some(worker);
        }
    }

    pub(super) fn cell_originating_call(&self, cell_id: &CellId) -> Option<ToolCallOrigin> {
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id)
            .and_then(|gate| gate.originating_call.clone())
    }

    pub(super) fn close_cell(&self, cell_id: &CellId) {
        let mut dispatch_gates = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Dropping the gate releases the cell's worker lease.
        dispatch_gates.remove(cell_id);
        drop(dispatch_gates);
        self.executed_tool_calls.finish_cell_recording(cell_id);
    }

    pub(super) fn forget_closed_completion(&self, cell_id: &CellId) {
        let mut completions = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if completions
            .get(cell_id)
            .is_some_and(|completion| completion.is_closed() && !completion.collecting())
        {
            completions.remove(cell_id);
        }
    }

    pub(super) fn active_cell_ids(&self) -> Vec<CellId> {
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}

impl Drop for CodeModeDispatchBroker {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.tasks.close();
    }
}

fn dispatch_gate(
    dispatch_gates: &Mutex<HashMap<CellId, CellDispatchGate>>,
    cell_id: &CellId,
) -> watch::Sender<bool> {
    let mut dispatch_gates = match dispatch_gates.lock() {
        Ok(dispatch_gates) => dispatch_gates,
        Err(poisoned) => poisoned.into_inner(),
    };
    dispatch_gates
        .entry(cell_id.clone())
        .or_insert_with(|| CellDispatchGate {
            delegate: Weak::new(),
            ready: watch::channel(false).0,
            originating_call: None,
            worker: None,
        })
        .ready
        .clone()
}

fn remove_dispatch_gate(
    dispatch_gates: &Mutex<HashMap<CellId, CellDispatchGate>>,
    cell_id: &CellId,
) {
    let mut dispatch_gates = match dispatch_gates.lock() {
        Ok(dispatch_gates) => dispatch_gates,
        Err(poisoned) => poisoned.into_inner(),
    };
    dispatch_gates.remove(cell_id);
}

async fn wait_until_cell_ready_for_dispatch(
    dispatch_gates: &Mutex<HashMap<CellId, CellDispatchGate>>,
    cell_id: &CellId,
    cancellation_token: &CancellationToken,
    stop: &CancellationToken,
) -> bool {
    if cancellation_token.is_cancelled() {
        return false;
    }
    let mut ready_rx = dispatch_gate(dispatch_gates, cell_id).subscribe();
    loop {
        if *ready_rx.borrow_and_update() {
            return true;
        }
        tokio::select! {
            changed = ready_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            _ = cancellation_token.cancelled() => return false,
            _ = stop.cancelled() => return false,
        }
    }
}

impl CodeModeSessionDelegate for CodeModeCellDelegate {
    #[tracing::instrument(
        name = "code_mode.broker.invoke_tool",
        level = "info",
        skip_all,
        fields(
            conversation.id = %self.broker.thread_id,
            cell.id = %invocation.cell_id,
            runtime_tool_call_id = invocation.runtime_tool_call_id.as_str(),
            tool_name = invocation.tool_name.name.as_str(),
            tool_namespace = invocation.tool_name.namespace.as_deref(),
        )
    )]
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            let call_id = format!("{PUBLIC_TOOL_NAME}-{}", uuid::Uuid::new_v4());
            call_trace::received(
                self.broker.thread_id,
                &invocation.tool_name,
                &call_id,
                call_trace::Receipt::CodeModeBroker {
                    cell_id: trace_id(invocation.cell_id.as_str()),
                    runtime_tool_call_id: trace_id(&invocation.runtime_tool_call_id),
                },
            );
            let mut dispatch_trace =
                Box::new(NestedToolDispatchTrace::new(self.broker.thread_id, call_id));
            if cancellation_token.is_cancelled() {
                dispatch_trace.interruption = Some(DispatchInterruption::Cancelled);
                return Err("code mode nested tool call cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            // Only the worker can tell whether dispatch beats cancellation once the call is queued.
            self.broker
                .dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    step_context: Arc::downgrade(&self.step_context),
                    tracker: Arc::clone(&self.tracker),
                    dispatch_trace,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .map_err(|_| "code mode nested tool dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode notification cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.broker
                .dispatch_tx
                .send(DispatchMessage::Notify {
                    call_id,
                    cell_id,
                    text,
                    step_context: Arc::downgrade(&self.step_context),
                    output_token_limit: with_serialization_allowance(
                        self.step_context
                            .settings
                            .model_info
                            .truncation_policy
                            .into(),
                    )
                    .token_budget(),
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .map_err(|_| "code mode notification dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn cell_terminal_ready(&self, ready: codex_code_mode::CellTerminalReady) {
        if let Some(completion) = &self.completion {
            completion.ready(ready);
        }
    }

    fn cell_closed(&self, cell_id: &CellId) {
        let registration = self
            .work_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(registration) = &registration {
            registration.mark_terminal();
        }
        drop(registration);
        self.broker.close_cell(cell_id);
        // Deliverability follows delegate, dispatch-worker and event-recording cleanup.
        if let Some(completion) = &self.completion {
            completion.closed(cell_id);
        }
        self.broker.forget_closed_completion(cell_id);
    }
}

enum DispatchMessage {
    InvokeTool {
        invocation: CodeModeNestedToolCall,
        // The delegate owns the step while the callback is live; stale queued work must not.
        step_context: Weak<StepContext>,
        tracker: SharedTurnDiffTracker,
        dispatch_trace: Box<NestedToolDispatchTrace>,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<JsonValue, String>>,
    },
    Notify {
        call_id: String,
        cell_id: CellId,
        text: String,
        step_context: Weak<StepContext>,
        output_token_limit: usize,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
}

#[cfg(test)]
#[path = "delegate_tests.rs"]
mod tests;
