use crate::artifacts::recovery_read_budget;
use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::read_artifact_spec::READ_ARTIFACT_TOOL_NAME;
use crate::tools::handlers::read_artifact_spec::create_read_artifact_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ReadArtifactArgs {
    artifact: String,
    #[serde(default)]
    start_byte: Option<u64>,
}

pub(crate) struct ReadArtifactHandler;

impl ToolExecutor<ToolInvocation> for ReadArtifactHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_ARTIFACT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_read_artifact_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "read_artifact handler received unsupported payload".to_string(),
                ));
            };
            let args: ReadArtifactArgs = serde_json::from_str(arguments).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to parse read_artifact arguments: {err}"
                ))
            })?;
            // Bound this page by the context headroom *at the time of the
            // read*: an unbounded recovery can push input + requested output
            // past the context window and be rejected before generation
            // (oh-my-pi #11365).
            let remaining_tokens = crate::session::context_window::context_window_token_status(
                invocation.session.as_ref(),
                invocation.turn.as_ref(),
            )
            .await
            .base_window_tokens_remaining;
            let budget = recovery_read_budget(remaining_tokens)
                .map_err(FunctionCallError::RespondToModel)?;

            let store = invocation.session.artifact_store().await;
            let mut output = store
                .read(&args.artifact, args.start_byte, budget.max_bytes)
                .map_err(FunctionCallError::RespondToModel)?;
            if let Some(notice) = budget.truncation_notice() {
                output.push('\n');
                output.push_str(&notice);
            }
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                output,
                /*success*/ Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for ReadArtifactHandler {}
