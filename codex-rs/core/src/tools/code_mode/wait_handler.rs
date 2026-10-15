use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use serde_json::Value;
use serde_json::json;

use super::DEFAULT_WAIT_YIELD_TIME_MS;
use super::ExecContext;
use super::WAIT_TOOL_NAME;
use super::handle_runtime_response;
use super::observation::code_mode_hook_tool_name;
use super::telemetry::CodeModeToolCallGuard;
use super::telemetry::trace_id;
use super::wait_spec::create_wait_tool;

pub struct CodeModeWaitHandler {
    spec: ToolSpec,
}

#[derive(Debug, Deserialize)]
struct ExecWaitArgs {
    cell_id: String,
    #[serde(default = "default_wait_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

fn default_wait_yield_time_ms() -> u64 {
    DEFAULT_WAIT_YIELD_TIME_MS
}

fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}

impl ToolExecutor<ToolInvocation> for CodeModeWaitHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WAIT_TOOL_NAME)
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

impl CodeModeWaitHandler {
    pub(crate) fn new(
        description_override: Option<&str>,
        parameters_override: Option<&str>,
    ) -> Self {
        Self {
            spec: create_wait_tool(description_override, parameters_override),
        }
    }

    // Default to interrupted if this future is dropped; telemetry::CodeModeToolCallGuard::finish
    // overwrites this handler's captured span on explicit success or failure, including early errors.
    #[tracing::instrument(
        name = "code_mode.handler.wait",
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
        let ToolInvocation {
            session,
            turn,
            step_context,
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
            WAIT_TOOL_NAME,
            handler_span,
        );
        let result = match payload {
            ToolPayload::Function { arguments }
                if tool_name.is_default_namespace()
                    && tool_name.name.as_str() == WAIT_TOOL_NAME =>
            {
                let args: ExecWaitArgs = parse_arguments(&arguments).inspect_err(|_error| {
                    telemetry.finish(/*success*/ false);
                })?;
                let exec = ExecContext { session, turn };
                let started_at = std::time::Instant::now();
                telemetry.cell_id = Some(args.cell_id.clone());
                let cell_id = codex_code_mode::CellId::new(args.cell_id);
                let wait_response = if args.terminate {
                    exec.session
                        .services
                        .code_mode_service
                        .terminate(cell_id)
                        .await
                } else {
                    exec.session
                        .services
                        .code_mode_service
                        .wait(
                            codex_code_mode::WaitRequest {
                                cell_id,
                                yield_time_ms: args.yield_time_ms,
                            },
                            step_context.preempt.clone(),
                        )
                        .await
                }
                .map_err(|error| {
                    telemetry.finish(/*success*/ false);
                    FunctionCallError::RespondToModel(error)
                })?;
                if let codex_code_mode::WaitOutcome::LiveCell(response) = &wait_response {
                    let runtime_cell_id = match response {
                        codex_code_mode::RuntimeResponse::Yielded { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Terminated { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Result { cell_id, .. } => cell_id,
                    };
                    tracing::Span::current().record("cell.id", trace_id(runtime_cell_id.as_str()));
                    telemetry.cell_id = Some(runtime_cell_id.to_string());
                    exec.session
                        .services
                        .executed_tool_calls
                        .register_cell(runtime_cell_id, &call_id);
                    if !matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
                        exec.session
                            .services
                            .rollout_thread_trace
                            .code_cell_trace_context(
                                exec.turn.sub_id.as_str(),
                                runtime_cell_id.as_str(),
                            )
                            .record_ended(response);
                        exec.session
                            .services
                            .code_mode_service
                            .finish_cell_dispatch(runtime_cell_id);
                        exec.session
                            .services
                            .analytics_events_client
                            .track_code_mode_tool_call(
                                codex_analytics::CodeModeToolCallFact::CellClosed {
                                    thread_id: exec.session.thread_id.to_string(),
                                    turn_id: exec.turn.sub_id.clone(),
                                    cell_id: runtime_cell_id.to_string(),
                                },
                            );
                    }
                }
                if let Some(code_mode_host_duration) = wait_response.code_mode_host_duration() {
                    telemetry.record_code_mode_host_duration(code_mode_host_duration);
                }
                exec.session.services.elicitations.wait_until_clear().await;
                let wall_time = wait_response
                    .code_mode_host_duration()
                    .unwrap_or_else(|| started_at.elapsed());
                Ok(boxed_tool_output(handle_runtime_response(
                    &step_context.settings.model_info,
                    wait_response.into(),
                    args.max_tokens,
                    wall_time,
                    exec.turn.config.code_mode.experimental_show_cell_overhead,
                )))
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WAIT_TOOL_NAME} expects JSON arguments"
            ))),
        };
        telemetry.finish(
            result
                .as_ref()
                .is_ok_and(codex_tools::ToolOutput::success_for_logging),
        );
        result
    }
}

/// Bounded hook view of a Code Mode `wait` call; it mirrors the model's
/// arguments, all of which are identifiers, durations, or flags.
fn wait_observation_input(args: &ExecWaitArgs) -> Value {
    let mut input = json!({
        "cell_id": args.cell_id,
        "yield_time_ms": args.yield_time_ms,
        "terminate": args.terminate,
    });
    if let (Some(max_tokens), Value::Object(fields)) = (args.max_tokens, &mut input) {
        fields.insert("max_tokens".to_string(), json!(max_tokens));
    }
    input
}

impl CoreToolRuntime for CodeModeWaitHandler {
    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // Code-mode `wait` is runtime control for an existing code cell, not a
        // standalone user action. Tool calls made from code mode still flow
        // through normal dispatch, but hooks should not block or rewrite the
        // wait loop itself; they observe it as an observational event.
        None
    }

    fn observational_hook_tool_name(&self, invocation: &ToolInvocation) -> Option<HookToolName> {
        Some(code_mode_hook_tool_name(invocation))
    }

    fn observational_hook_tool_input<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
    ) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return None;
            };
            let args: ExecWaitArgs = parse_arguments(arguments).ok()?;
            Some(wait_observation_input(&args))
        })
    }

    fn post_tool_use_payload(
        &self,
        _invocation: &ToolInvocation,
        _result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        // The wait result feeds code-mode control flow, so do not let
        // PostToolUse replace it with model-facing hook feedback.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::ExecWaitArgs;
    use super::parse_arguments;
    use super::wait_observation_input;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn wait_observation_input_uses_defaults_and_bounded_fields() {
        let args: ExecWaitArgs =
            parse_arguments(r#"{"cell_id":"3"}"#).expect("parse wait arguments");

        assert_eq!(
            wait_observation_input(&args),
            json!({
                "cell_id": "3",
                "yield_time_ms": super::DEFAULT_WAIT_YIELD_TIME_MS,
                "terminate": false,
            })
        );
    }

    #[test]
    fn wait_observation_input_keeps_terminate_and_token_limit() {
        let args: ExecWaitArgs = parse_arguments(
            r#"{"cell_id":"9","terminate":true,"max_tokens":50,"yield_time_ms":5}"#,
        )
        .expect("parse wait arguments");

        assert_eq!(
            wait_observation_input(&args),
            json!({
                "cell_id": "9",
                "yield_time_ms": 5,
                "terminate": true,
                "max_tokens": 50,
            })
        );
    }
}
