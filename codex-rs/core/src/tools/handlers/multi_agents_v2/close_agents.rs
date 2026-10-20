//! `close_agents`: lets a V2 agent close its descendants, loaded or unloaded.
//!
//! A fork-owned top-level tool: the server rejects new tools in the reserved `collaboration`
//! namespace. Closing marks the spawn edges closed and shuts down anything still running in
//! the closed subtrees, through the same path the app-server uses to archive a subagent.

use super::analytics::ToolCallAnalytics;
use super::list_agents::UNLOADED_AGENT_STATUS;
use super::*;
use crate::agent::types::ListedAgent;
use crate::tools::handlers::multi_agents_spec::CLOSE_AGENTS_IDLE_TARGET;
use crate::tools::handlers::multi_agents_spec::CLOSE_AGENTS_TOOL_NAME;
use crate::tools::handlers::multi_agents_spec::create_close_agents_tool;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_tools::ToolSpec;
use std::collections::HashSet;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(CLOSE_AGENTS_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_close_agents_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let analytics = ToolCallAnalytics::new(&invocation, CollabAgentTool::CloseAgent);
            let result = handle_close_agents(invocation).await;
            analytics.finish(&result);
            result.map(boxed_tool_output)
        })
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseAgentsArgs {
    targets: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ClosedAgent {
    agent_name: String,
    /// Status before closing; `unloaded` for an idle agent whose runtime was unloaded.
    previous_status: JsonValue,
}

#[derive(Debug, Serialize)]
struct CloseFailure {
    target: String,
    error: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct CloseAgentsResult {
    closed: Vec<ClosedAgent>,
    failed: Vec<CloseFailure>,
}

async fn handle_close_agents(
    invocation: ToolInvocation,
) -> Result<CloseAgentsResult, FunctionCallError> {
    let arguments = function_arguments(invocation.payload.clone())?;
    let args: CloseAgentsArgs = parse_arguments(&arguments)?;
    let ToolInvocation { session, turn, .. } = invocation;
    if args.targets.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "{CLOSE_AGENTS_TOOL_NAME} needs at least one target, or [\"{CLOSE_AGENTS_IDLE_TARGET}\"]."
        )));
    }
    let caller_path = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let agent_control = &session.services.agent_control;
    let descendants = agent_control
        .list(
            session.thread_id,
            turn.parent_thread_id,
            &turn.session_source,
            Some(caller_path.as_str()),
        )
        .await
        .map_err(collab_spawn_error)?
        .into_iter()
        .filter(|agent| {
            agent
                .metadata
                .agent_path
                .as_ref()
                .is_some_and(|path| caller_path.is_ancestor_of(path))
        })
        .collect::<Vec<_>>();

    let mut failed = Vec::new();
    let selected = if args.targets.len() == 1 && args.targets[0] == CLOSE_AGENTS_IDLE_TARGET {
        idle_subtree_roots(&descendants)
    } else {
        let mut selected = Vec::new();
        for target in args.targets {
            let thread_id = match resolve_agent_target(&session, &turn, &target).await {
                Ok(thread_id) => thread_id,
                Err(err) => {
                    failed.push(CloseFailure {
                        target,
                        error: err.to_string(),
                    });
                    continue;
                }
            };
            match descendants
                .iter()
                .find(|agent| agent.thread_id == thread_id)
            {
                Some(agent) => selected.push(agent),
                None => failed.push(CloseFailure {
                    target,
                    error: format!(
                        "not an open descendant of `{caller_path}`; agents can only close agents they spawned and those agents' descendants"
                    ),
                }),
            }
        }
        topmost(selected)
    };

    let mut closed = Vec::with_capacity(selected.len());
    for agent in selected {
        let agent_name = agent_name(agent);
        match agent_control.close_v2_agent(agent.thread_id).await {
            Ok(()) => closed.push(ClosedAgent {
                agent_name,
                previous_status: match &agent.live_status {
                    Some(status) => serde_json::to_value(status).unwrap_or(JsonValue::Null),
                    None => JsonValue::String(UNLOADED_AGENT_STATUS.to_string()),
                },
            }),
            Err(err) => failed.push(CloseFailure {
                target: agent_name,
                error: collab_v2_agent_error(agent.thread_id, err).to_string(),
            }),
        }
    }
    Ok(CloseAgentsResult { closed, failed })
}

fn agent_name(agent: &ListedAgent) -> String {
    agent
        .metadata
        .agent_path
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| agent.thread_id.to_string())
}

/// Idle agents whose open subtree is idle too, without their idle descendants: closing a
/// subtree root closes the rest, and an agent with busy descendants is left alone.
fn idle_subtree_roots(descendants: &[ListedAgent]) -> Vec<&ListedAgent> {
    let busy_paths = descendants
        .iter()
        .filter(|agent| !agent.is_idle())
        .filter_map(|agent| agent.metadata.agent_path.as_ref())
        .collect::<Vec<_>>();
    let idle = descendants
        .iter()
        .filter(|agent| agent.is_idle())
        .filter(|agent| {
            agent
                .metadata
                .agent_path
                .as_ref()
                .is_some_and(|path| !busy_paths.iter().any(|busy| path.is_ancestor_of(busy)))
        })
        .collect::<Vec<_>>();
    topmost(idle)
}

/// Drops agents whose ancestor is also selected, and duplicates.
fn topmost(selected: Vec<&ListedAgent>) -> Vec<&ListedAgent> {
    let paths = selected
        .iter()
        .filter_map(|agent| agent.metadata.agent_path.clone())
        .collect::<Vec<_>>();
    let mut seen = HashSet::<ThreadId>::new();
    selected
        .into_iter()
        .filter(|agent| {
            agent
                .metadata
                .agent_path
                .as_ref()
                .is_none_or(|path| !paths.iter().any(|other| other.is_ancestor_of(path)))
        })
        .filter(|agent| seen.insert(agent.thread_id))
        .collect()
}

impl ToolOutput for CloseAgentsResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, CLOSE_AGENTS_TOOL_NAME)
    }

    fn success_for_logging(&self) -> bool {
        self.failed.is_empty()
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), CLOSE_AGENTS_TOOL_NAME)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, CLOSE_AGENTS_TOOL_NAME)
    }
}

#[cfg(test)]
#[path = "close_agents_tests.rs"]
mod tests;
