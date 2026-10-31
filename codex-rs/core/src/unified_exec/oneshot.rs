//! One-shot execution over the shared unified-exec lifecycle.
//!
//! The caller never receives a resumable process. Timeout and cancellation
//! terminate the exact process handle published by the normal startup path.

use std::sync::Arc;
use std::sync::OnceLock;

use tokio::time::Duration;
use tokio::time::Instant;
use tracing::Instrument;

use super::ExecCommandRequest;
use super::UnifiedExecContext;
use super::UnifiedExecError;
use super::UnifiedExecProcess;
use super::UnifiedExecProcessManager;
use super::trace_id;
use crate::tools::context::ExecCommandToolOutput;

pub(super) struct Completion<'a> {
    pub timeout: Duration,
    pub timed_out: bool,
    pub process: &'a OnceLock<Arc<UnifiedExecProcess>>,
}

impl UnifiedExecProcessManager {
    #[tracing::instrument(
        name = "unified_exec.exec_command",
        level = "info",
        skip_all,
        fields(
            conversation.id = %context.session.thread_id,
            turn_id = trace_id(&context.step_context.turn.sub_id),
            call_id = trace_id(&context.call_id),
            unified_exec_process_id = request.process_id,
            mode = "oneshot",
            outcome = tracing::field::Empty,
        )
    )]
    pub(crate) async fn exec_command_to_completion(
        request: ExecCommandRequest,
        context: &UnifiedExecContext,
        timeout: Duration,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        if request.work_kind == super::NativeWorkKind::Service {
            context
                .session
                .services
                .unified_exec_manager
                .release_process_id(request.process_id)
                .await;
            return Err(UnifiedExecError::process_failed(
                "one-shot exec_command cannot own a service".into(),
            ));
        }
        let mut copied_context = UnifiedExecContext::new(
            Arc::clone(&context.session),
            Arc::clone(&context.step_context),
            context.cancellation_token.child_token(),
            context.call_id.clone(),
        );
        copied_context.originating_cell_id = context.originating_cell_id.clone();
        copied_context.originating_cell_registration =
            context.originating_cell_registration.clone();
        let context = copied_context;
        let _cancel_on_drop = context.cancellation_token.clone().drop_guard();
        let task = async move {
            let manager = &context.session.services.unified_exec_manager;
            let process_id = request.process_id;
            if Instant::now().checked_add(timeout).is_none() {
                manager.release_process_id(process_id).await;
                return (
                    Err(UnifiedExecError::process_failed(
                        "timeout_ms is too large".into(),
                    )),
                    "failed",
                );
            }

            let (registration, residency) =
                match manager.admit_native_work(&request, &context).await {
                    Ok(admitted) => admitted,
                    Err(error) => return (Err(error), "failed"),
                };
            let process = OnceLock::new();
            let mut completion = Completion {
                timeout,
                timed_out: false,
                process: &process,
            };
            let result = {
                let mut execution = Box::pin(manager.exec_command_inner(
                    request,
                    &context,
                    Some(&mut completion),
                    /*reservation*/ None,
                    registration,
                    residency,
                ));
                tokio::select! {
                    biased;
                    _ = context.cancellation_token.cancelled() => {
                        if let Some(process) = process.get() {
                            if !process.has_exited()
                                && let Err(err) = process.terminate_confirmed().await
                            {
                                process.fail_and_terminate(err.to_string());
                            }
                            // An exited remote shell can still have open output pipes.
                            process.cancellation_token().cancel();
                            let _ = execution.await;
                        } else {
                            drop(execution);
                            manager.release_process_id(process_id).await;
                        }
                        return (
                            Err(UnifiedExecError::process_failed("command cancelled".into())),
                            "cancelled",
                        );
                    }
                    result = &mut execution => result,
                }
            };

            let outcome = if completion.timed_out {
                "timed_out"
            } else if result.is_ok() {
                "exited"
            } else {
                "failed"
            };
            let result = result.map(|mut output| {
                if completion.timed_out {
                    output.process_id = None;
                }
                output
            });
            (result, outcome)
        };
        // The owned task must retain the caller's idle approval policy before
        // it starts the separate cancellation-safe acquisition task.
        let nested_code_mode_call = crate::tools::in_code_mode_nested_call();
        let task = crate::tools::scope_code_mode_nested_call(nested_code_mode_call, task);
        let (result, outcome) = match tokio::spawn(task.in_current_span()).await {
            Ok(result) => result,
            Err(err) => (
                Err(UnifiedExecError::process_failed(err.to_string())),
                "failed",
            ),
        };
        tracing::Span::current().record("outcome", outcome);
        result
    }
}
