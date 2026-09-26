use super::*;
use crate::agent::agent_resolver::resolve_agent_target;
use crate::agent::api::AgentTarget;
use crate::agent::context_policy::ContextReductionPolicyArgs;
use crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE;
use crate::tools::handlers::multi_agents_spec::create_set_agent_context_policy_tool;
use crate::tools::handlers::multi_agents_spec::create_set_agent_context_policy_tool_v1;
use codex_tools::ToolSpec;

#[derive(Default)]
pub(crate) struct Handler {
    v1: bool,
}

impl Handler {
    pub(crate) fn v1() -> Self {
        Self { v1: true }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        if self.v1 {
            ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "set_agent_context_policy")
        } else {
            ToolName::plain("set_agent_context_policy")
        }
    }

    fn spec(&self) -> ToolSpec {
        if self.v1 {
            create_set_agent_context_policy_tool_v1()
        } else {
            create_set_agent_context_policy_tool()
        }
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let result = self.handle_call(invocation).await;
            result.map(boxed_tool_output)
        })
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<SetAgentContextPolicyResult, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: SetAgentContextPolicyArgs = parse_arguments(&arguments)?;
        let patch = args
            .context_policy
            .map(ContextReductionPolicyArgs::into_overrides)
            .unwrap_or_default();
        let has_policy_field = patch.enabled.is_some()
            || patch.threshold_tokens.is_some()
            || patch.check_after_tools.is_some()
            || patch.shake.is_some()
            || patch.on_failure.is_some();
        if (args.reset && has_policy_field) || (!args.reset && !has_policy_field) {
            return Err(FunctionCallError::RespondToModel(
                "provide context_policy fields for an update, or set reset=true without fields"
                    .to_string(),
            ));
        }
        let target = resolve_agent_target(&session, &turn, &args.target).await?;
        let (desired_revision, applied_revision, persisted) = session
            .services
            .agent_control
            .set_agent_context_policy(
                session.thread_id,
                AgentTarget::Id(target),
                patch,
                args.inherit_to_children.unwrap_or(true),
                args.reset,
            )
            .await
            .map_err(|error| collab_v2_agent_error(target, error))?;
        Ok(SetAgentContextPolicyResult {
            desired_revision,
            applied_revision,
            persisted,
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
struct SetAgentContextPolicyArgs {
    target: String,
    context_policy: Option<ContextReductionPolicyArgs>,
    inherit_to_children: Option<bool>,
    #[serde(default)]
    reset: bool,
}

#[derive(Debug, Serialize)]
struct SetAgentContextPolicyResult {
    desired_revision: u64,
    applied_revision: Option<u64>,
    persisted: bool,
}

impl ToolOutput for SetAgentContextPolicyResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "set_agent_context_policy")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(
            call_id,
            payload,
            self,
            Some(true),
            "set_agent_context_policy",
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "set_agent_context_policy")
    }
}
