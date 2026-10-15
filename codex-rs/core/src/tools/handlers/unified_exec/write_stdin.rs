use crate::function_tool::FunctionCallError;
use codex_features::Feature;

use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::tools::sandboxing::ToolError;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::WriteStdinInteractionEvent;
use crate::unified_exec::WriteStdinRequest;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::super::shell_spec::create_write_stdin_tool;
use super::post_unified_exec_tool_use_payload;

#[derive(Debug, Deserialize)]
struct WriteStdinArgs {
    // The model is trained on `session_id`.
    session_id: i32,
    #[serde(default)]
    chars: String,
    #[serde(default = "super::default_write_stdin_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Default)]
pub struct WriteStdinHandler {
    /// The session's results are delivered automatically, so the model-facing spec omits empty
    /// polling. Nested Code Mode calls use the default spec.
    automatic_completion: bool,
}

impl WriteStdinHandler {
    pub(crate) fn new(automatic_completion: bool) -> Self {
        Self {
            automatic_completion,
        }
    }
}

impl ToolExecutor<ToolInvocation> for WriteStdinHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("write_stdin")
    }

    fn spec(&self) -> ToolSpec {
        create_write_stdin_tool(/*automatic_completion*/ false)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl WriteStdinHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            call_id,
            source,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "write_stdin handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: WriteStdinArgs = parse_arguments(&arguments)?;
        // Only model polls are refused; a Code Mode cell polling its own process is the cheap
        // pattern, since the whole loop costs one model request.
        if args.chars.is_empty()
            && !matches!(source, ToolCallSource::CodeMode { .. })
            && turn.automatic_completion_active()
            && session
                .services
                .unified_exec_manager
                .has_automatic_completion(args.session_id)
                .await
        {
            return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                automatic_completion_poll_refusal(args.session_id),
                Some(true),
            )));
        }
        if turn
            .config
            .features
            .get()
            .enabled(Feature::StableEnvironmentTools)
        {
            resolve_tool_environment(
                &step_context,
                /*environment_id*/ None,
                "unified exec is unavailable in this session",
            )?;
        }
        let context =
            UnifiedExecContext::new(session.clone(), step_context, cancellation_token, call_id);
        let response = session
            .services
            .unified_exec_manager
            .write_stdin(
                &context,
                WriteStdinRequest {
                    process_id: args.session_id,
                    input: &args.chars,
                    yield_time_ms: args.yield_time_ms,
                    max_output_tokens: args.max_output_tokens,
                    truncation_policy: context
                        .step_context
                        .settings
                        .model_info
                        .truncation_policy
                        .into(),
                    interaction_event: Some(WriteStdinInteractionEvent {
                        session: &session,
                        turn: &turn,
                    }),
                },
            )
            .await
            .map_err(|err| {
                let message = match err {
                    UnifiedExecError::StdinApproval(ToolError::Rejected(reason)) => {
                        format!("write_stdin rejected: {reason}")
                    }
                    UnifiedExecError::StdinApproval(ToolError::Codex(err)) => {
                        format!("write_stdin approval failed: {err}")
                    }
                    err => format!("write_stdin failed: {err}"),
                };
                FunctionCallError::RespondToModel(message)
            })?;

        Ok(boxed_tool_output(response))
    }
}

fn automatic_completion_poll_refusal(session_id: i32) -> String {
    format!(
        "session {session_id} is running; its exit status and output tail will be delivered automatically."
    )
}

impl CoreToolRuntime for WriteStdinHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn model_visible_spec(&self) -> Option<ToolSpec> {
        self.automatic_completion
            .then(|| create_write_stdin_tool(/*automatic_completion*/ true))
    }

    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // `write_stdin` is transport for an existing exec session. Empty writes
        // are background polls, and non-empty writes continue a command that
        // already ran PreToolUse as Bash, so it is never gated here; hooks see
        // it as an observational event instead.
        None
    }

    fn observational_hook_tool_name(&self, _invocation: &ToolInvocation) -> Option<HookToolName> {
        Some(HookToolName::new("write_stdin"))
    }

    fn observational_hook_tool_input<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
    ) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return None;
            };
            let args: WriteStdinArgs = parse_arguments(arguments).ok()?;
            let exec_call_id = invocation
                .session
                .services
                .unified_exec_manager
                .originating_call_id_for_process(args.session_id)
                .await;
            Some(write_stdin_observation_input(&args, exec_call_id))
        })
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        // A `write_stdin` poll can observe final completion for the original
        // `exec_command`; emit that command's matching Bash PostToolUse.
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

/// Bounded hook view of a `write_stdin` call: identifiers, sizes, and flags,
/// never the stdin text. `chars: ""` is kept for empty polls so detectors that
/// key on the model-facing argument still recognise them.
fn write_stdin_observation_input(args: &WriteStdinArgs, exec_call_id: Option<String>) -> Value {
    let stdin_empty = args.chars.is_empty();
    let mut input = json!({
        "session_id": args.session_id,
        "stdin_empty": stdin_empty,
        "stdin_bytes": args.chars.len(),
        "yield_time_ms": args.yield_time_ms,
    });
    let Value::Object(fields) = &mut input else {
        unreachable!("json object literal");
    };
    if stdin_empty {
        fields.insert("chars".to_string(), Value::String(String::new()));
    }
    if let Some(max_output_tokens) = args.max_output_tokens {
        fields.insert("max_output_tokens".to_string(), json!(max_output_tokens));
    }
    if let Some(exec_call_id) = exec_call_id {
        fields.insert("exec_call_id".to_string(), Value::String(exec_call_id));
    }
    input
}

#[cfg(test)]
mod tests {
    use super::WriteStdinArgs;
    use super::write_stdin_observation_input;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn empty_poll_observation_keeps_empty_chars_and_origin() {
        let args = WriteStdinArgs {
            session_id: 1000,
            chars: String::new(),
            yield_time_ms: 60_000,
            max_output_tokens: None,
        };

        assert_eq!(
            write_stdin_observation_input(&args, Some("call-exec".to_string())),
            json!({
                "session_id": 1000,
                "chars": "",
                "stdin_empty": true,
                "stdin_bytes": 0,
                "yield_time_ms": 60_000,
                "exec_call_id": "call-exec",
            })
        );
    }

    #[test]
    fn input_observation_omits_stdin_text() {
        let args = WriteStdinArgs {
            session_id: 7,
            chars: "secret\n".to_string(),
            yield_time_ms: 250,
            max_output_tokens: Some(100),
        };

        assert_eq!(
            write_stdin_observation_input(&args, /*exec_call_id*/ None),
            json!({
                "session_id": 7,
                "stdin_empty": false,
                "stdin_bytes": 7,
                "yield_time_ms": 250,
                "max_output_tokens": 100,
            })
        );
    }
}
