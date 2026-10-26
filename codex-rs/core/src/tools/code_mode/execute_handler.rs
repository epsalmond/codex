use crate::agent::control::WorkKind;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_features::Feature;
use codex_prompts::ResolvedModelMessages;
use codex_tools::IndirectNamespacePrefixes;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use serde_json::Value;
use std::borrow::Cow;
use std::sync::Arc;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::cell_completion::CellCompletion;
use super::cell_completion::InitialCompletion;
use super::handle_runtime_response;
use super::is_exec_tool_name;
use super::observation::code_mode_hook_tool_name;
use super::observation::exec_observation_input;
use super::output::CodeModeToolOutput;
use super::prepare_code_mode_tool_definitions;
use super::telemetry::CodeModeToolCallGuard;
use super::telemetry::trace_id;

type CodeModeNestedTool = (Arc<ToolSpec>, Option<Arc<dyn CoreToolRuntime>>);

pub struct CodeModeExecuteHandler {
    spec: ToolSpec,
    nested_tool_specs: Vec<CodeModeNestedTool>,
}

impl CodeModeExecuteHandler {
    pub(crate) fn new(spec: ToolSpec, nested_tool_specs: Vec<CodeModeNestedTool>) -> Self {
        Self {
            spec,
            nested_tool_specs,
        }
    }

    #[expect(clippy::too_many_arguments)]
    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        step_context: Arc<crate::session::step_context::StepContext>,
        tracker: crate::tools::context::SharedTurnDiffTracker,
        call_id: String,
        originating_call: Option<crate::tools::context::ToolCallOrigin>,
        code: String,
        telemetry: &mut CodeModeToolCallGuard,
    ) -> Result<CodeModeToolOutput, FunctionCallError> {
        let args =
            codex_code_mode::parse_exec_source(&code).map_err(FunctionCallError::RespondToModel)?;
        let exec = ExecContext {
            session,
            turn: Arc::clone(&step_context.turn),
        };
        let code_mode_input_schema_max_bytes = step_context
            .turn
            .config
            .code_mode
            .tool_input_schema_max_bytes;
        let tool_description_first = step_context
            .turn
            .config
            .features
            .enabled(Feature::CodeModeToolDescriptionFirst);
        let mut enabled_tools = Vec::with_capacity(self.nested_tool_specs.len());
        for (spec, cached_runtime) in &self.nested_tool_specs {
            let definitions = prepare_code_mode_tool_definitions(
                cached_runtime.as_deref(),
                || Cow::Borrowed(spec.as_ref()),
                code_mode_input_schema_max_bytes,
                tool_description_first,
            );
            match definitions {
                Cow::Borrowed(definitions) => enabled_tools.extend_from_slice(definitions),
                Cow::Owned(definitions) => enabled_tools.extend(definitions),
            }
        }
        enabled_tools.sort_by(|left, right| left.name.cmp(&right.name));
        enabled_tools.dedup_by(|left, right| left.name == right.name);
        let model_messages = ResolvedModelMessages::from_model(&step_context.settings.model_info);
        IndirectNamespacePrefixes::new(
            model_messages.indirect_description_prefixes(),
            step_context.tool_router.mcp_namespaces(),
        )
        .map_err(|error| FunctionCallError::Fatal(error.to_string()))?
        .apply_code_mode(&mut enabled_tools);
        let runtime = &exec.session.services.local_agent_runtime;
        let membership = runtime
            .admit_start()
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        let residency = runtime
            .pin_owned_work_owner(&exec.session)
            .await
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        let kind = match args.work_kind {
            codex_code_mode::CodeModeWorkKind::Task => WorkKind::Cell,
            codex_code_mode::CodeModeWorkKind::Service => WorkKind::Service,
        };
        let registration = runtime
            .reserve_owned_work(&exec.session, &exec.turn, &call_id, kind, None)
            .map_err(|error| FunctionCallError::RespondToModel(error.to_owned()))?;
        registration.mark_cell();
        // Reserve before the cell runs, so a full store rejects without side effects. Eligibility
        // matches the native process producer: the feature gate, then the store's owner checks.
        let completion = if exec
            .turn
            .config
            .features
            .enabled(codex_features::Feature::AsyncProcessCompletion)
        {
            let reservation = exec
                .session
                .services
                .async_completions
                .reserve_cell_with_lifecycle(
                    exec.session.thread_id(),
                    &exec.turn,
                    &call_id,
                    &registration,
                )
                .map_err(|error| FunctionCallError::RespondToModel(error.to_owned()))?;
            Some(InitialCompletion(CellCompletion::new(
                reservation,
                Some(registration.clone()),
                &exec.session,
                Arc::clone(&exec.turn),
                exec.session
                    .services
                    .code_mode_service
                    .shutdown_token
                    .clone(),
                exec.turn.code_mode_automatic_completion(),
            )))
        } else {
            None
        };
        let started_at = std::time::Instant::now();
        let (started_cell, initial_guard) = exec
            .session
            .services
            .code_mode_service
            .execute(
                codex_code_mode::ExecuteRequest {
                    tool_call_id: call_id.clone(),
                    enabled_tools,
                    source: args.code.clone(),
                    yield_time_ms: args.yield_time_ms,
                    max_output_tokens: args.max_output_tokens,
                },
                Arc::clone(&step_context),
                tracker,
                completion.as_ref().map(|initial| Arc::clone(&initial.0)),
                registration.clone(),
                membership,
                residency,
                Arc::clone(&exec.session),
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let cell_id = started_cell.cell_id.clone();
        tracing::Span::current().record("cell.id", trace_id(cell_id.as_str()));
        telemetry.cell_id = Some(cell_id.to_string());
        exec.session
            .services
            .analytics_events_client
            .track_code_mode_tool_call(codex_analytics::CodeModeToolCallFact::CellStarted {
                thread_id: exec.session.thread_id.to_string(),
                turn_id: exec.turn.sub_id.clone(),
                call_id: call_id.clone(),
                cell_id: cell_id.to_string(),
            });
        exec.session
            .services
            .executed_tool_calls
            .start_cell(&cell_id, &call_id);
        let runtime_cell_id = cell_id.to_string();
        let code_cell_trace = exec
            .session
            .services
            .rollout_thread_trace
            .start_code_cell_trace(
                exec.turn.sub_id.as_str(),
                runtime_cell_id.as_str(),
                call_id.as_str(),
                args.code.as_str(),
            );
        exec.session
            .services
            .code_mode_service
            .mark_cell_ready_for_dispatch(&cell_id, originating_call);
        exec.session
            .services
            .code_mode_service
            .retain_dispatch_worker_for_cell(&exec.session, &cell_id);
        let response = started_cell
            .initial_response()
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        if let Some(code_mode_host_duration) = response.code_mode_host_duration() {
            telemetry.record_code_mode_host_duration(code_mode_host_duration);
        }
        // Record the raw runtime boundary. The model-visible custom-tool output
        // is produced by `handle_runtime_response` and later linked through
        // `CodeCell.output_item_ids` in the reduced trace.
        code_cell_trace.record_initial_response(&response);
        let automatic = completion
            .as_ref()
            .is_some_and(|completion| completion.0.initial(&response));
        if matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) && !automatic {
            registration.mark_yielded(false);
        }
        initial_guard.complete();
        // Yielded cells keep running, so terminal lifecycle is only emitted
        // here when the first response also ended the runtime.
        if matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
            telemetry.record_cell_timing(
                &cell_id,
                codex_extension_api::CodeModeCellTiming::ExecYielded,
            );
        } else {
            code_cell_trace.record_ended(&response);
            exec.session
                .services
                .code_mode_service
                .finish_cell_dispatch(&cell_id);
            exec.session
                .services
                .analytics_events_client
                .track_code_mode_tool_call(codex_analytics::CodeModeToolCallFact::CellClosed {
                    thread_id: exec.session.thread_id.to_string(),
                    turn_id: exec.turn.sub_id.clone(),
                    cell_id: cell_id.to_string(),
                });
        }
        exec.session.services.elicitations.wait_until_clear().await;
        let wall_time = response
            .code_mode_host_duration()
            .unwrap_or_else(|| started_at.elapsed());
        Ok(handle_runtime_response(
            &step_context.settings.model_info,
            response,
            args.max_output_tokens,
            wall_time,
            exec.turn.config.code_mode.experimental_show_cell_overhead,
            exec.turn.code_mode_automatic_completion(),
        )
        .with_work_kind(kind))
    }
}

/// Cancellation before the initial disposition transfers exact-cell cleanup with its startup
/// membership and residency pin; shutdown cannot pass that cleanup in flight.
pub(super) struct InitialCellGuard {
    session: Arc<crate::session::session::Session>,
    pending: Option<(
        codex_code_mode::CellId,
        crate::agent::control::AgentTreeMembership,
        Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    )>,
}

impl InitialCellGuard {
    pub(super) fn new(
        session: Arc<crate::session::session::Session>,
        cell_id: codex_code_mode::CellId,
        membership: crate::agent::control::AgentTreeMembership,
        residency: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Self {
        Self {
            session,
            pending: Some((cell_id, membership, residency)),
        }
    }

    pub(super) async fn cleanup(mut self) {
        if let Some((cell_id, membership, residency)) = self.pending.take() {
            let teardown =
                membership.into_teardown_guard("code_mode_initial", Some(self.session.thread_id()));
            cleanup_initial_cell(Arc::clone(&self.session), cell_id, teardown, residency).await;
        }
    }

    fn complete(mut self) {
        self.pending = None;
    }
}

impl Drop for InitialCellGuard {
    fn drop(&mut self) {
        let Some((cell_id, membership, residency)) = self.pending.take() else {
            return;
        };
        let session = Arc::clone(&self.session);
        // Construct before spawning: cancellation before the cleanup task's first poll still
        // records failure before releasing its tree membership.
        let teardown =
            membership.into_teardown_guard("code_mode_initial", Some(session.thread_id()));
        self.session
            .services
            .code_mode_service
            .startups
            .spawn(async move {
                cleanup_initial_cell(session, cell_id, teardown, residency).await;
            });
    }
}

async fn cleanup_initial_cell(
    session: Arc<crate::session::session::Session>,
    cell_id: codex_code_mode::CellId,
    teardown: crate::agent::control::AgentTreeTeardownGuard,
    residency: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
) {
    let _residency = residency;
    if let Err(error) = session
        .services
        .code_mode_service
        .terminate_for(cell_id.clone(), super::TerminateCause::Interrupt)
        .await
    {
        tracing::warn!(%cell_id, %error, "failed to terminate cancelled initial code-mode cell");
        teardown.record_shutdown_failure("terminate_cell", "code_mode_termination_failed");
    }
    teardown.complete();
}

impl ToolExecutor<ToolInvocation> for CodeModeExecuteHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(PUBLIC_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeExecuteHandler {
    // Default to interrupted if this future is dropped; telemetry::CodeModeToolCallGuard::finish
    // overwrites this handler's captured span on explicit success or failure.
    #[tracing::instrument(
        name = "code_mode.handler.execute",
        level = "info",
        skip_all,
        fields(
            conversation.id = %invocation.session.thread_id,
            turn_id = invocation.turn.sub_id.as_str(),
            call_id = trace_id(&invocation.call_id),
            cell.id = tracing::field::Empty,
            outcome = "interrupted",
        )
    )]
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let handler_span = tracing::Span::current();
        let originating_call = invocation.originating_call().await;
        let ToolInvocation {
            session,
            turn,
            step_context,
            tracker,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let mut telemetry = CodeModeToolCallGuard::new(
            &session,
            turn.sub_id.clone(),
            turn.turn_metadata_state.clone(),
            call_id.clone(),
            PUBLIC_TOOL_NAME,
            handler_span,
        );
        let result = match payload {
            ToolPayload::Custom { input } if is_exec_tool_name(&tool_name) => self
                .execute(
                    session,
                    step_context,
                    tracker,
                    call_id,
                    originating_call,
                    input,
                    &mut telemetry,
                )
                .await
                .map(boxed_tool_output),
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{PUBLIC_TOOL_NAME} expects raw JavaScript source text"
            ))),
        };
        telemetry.finish(
            result
                .as_ref()
                .is_ok_and(|output| output.success_for_logging()),
        );
        result
    }
}

impl CoreToolRuntime for CodeModeExecuteHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }

    // The cell is not gated by PreToolUse (its nested tool calls are), but
    // hooks observe that it started.
    fn observational_hook_tool_name(&self, invocation: &ToolInvocation) -> Option<HookToolName> {
        is_exec_tool_name(&invocation.tool_name).then(|| code_mode_hook_tool_name(invocation))
    }

    fn observational_hook_tool_input<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
    ) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            let ToolPayload::Custom { input } = &invocation.payload else {
                return None;
            };
            Some(exec_observation_input(input))
        })
    }
}

#[cfg(test)]
#[path = "execute_handler_tests.rs"]
mod tests;
