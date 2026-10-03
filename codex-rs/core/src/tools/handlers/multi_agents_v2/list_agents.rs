use super::analytics::ToolCallAnalytics;
use super::*;
use crate::agent::types::AgentContextUsage;
use crate::session::multi_agents::ChildReportMode;
use crate::tools::handlers::multi_agents_spec::create_list_agents_tool;
use codex_tools::ToolSpec;

#[derive(Default)]
pub(crate) struct Handler {
    pub(crate) child_report_mode: ChildReportMode,
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("list_agents")
    }

    fn spec(&self) -> ToolSpec {
        create_list_agents_tool(self.child_report_mode)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let analytics = ToolCallAnalytics::new(&invocation, CollabAgentTool::ListAgents);
            let result = self.handle_call(invocation).await;
            analytics.finish(&result);
            result
        })
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: ListAgentsArgs = parse_arguments(&arguments)?;
        let agents = session
            .services
            .agent_control
            .list(
                session.thread_id,
                turn.parent_thread_id,
                &turn.session_source,
                args.path_prefix.as_deref(),
            )
            .await
            .map_err(collab_spawn_error)?;

        let mut listed = Vec::with_capacity(agents.len());
        for agent in agents {
            let mut context = session
                .services
                .local_agent_runtime
                .agent_context_usage(agent.thread_id)
                .await;
            // The sealed-item boundary is human-facing status metadata.
            if let Some(context) = &mut context {
                context.shake_watermark = None;
                context.prepared_request_tokens = None;
            }
            listed.push(ListedAgent {
                agent_name: agent
                    .metadata
                    .agent_path
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| agent.thread_id.to_string()),
                agent_status: agent.status,
                context,
            });
        }
        Ok(boxed_tool_output(ListAgentsResult { agents: listed }))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListAgentsArgs {
    path_prefix: Option<String>,
}

#[derive(Debug, Serialize)]
struct ListedAgent {
    agent_name: String,
    agent_status: AgentStatus,
    context: Option<AgentContextUsage>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListAgentsResult {
    agents: Vec<ListedAgent>,
}

impl ToolOutput for ListAgentsResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "list_agents")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "list_agents")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "list_agents")
    }
}
