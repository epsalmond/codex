use codex_protocol::models::ContentItemKind;

use super::ContextualUserFragment;

/// Queue-only warning sent to a legacy orchestrator when a child keeps running after reduction fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentContextReductionWarning {
    pub(crate) agent_reference: String,
    pub(crate) message: String,
}

impl SubagentContextReductionWarning {
    pub(crate) fn new(agent_reference: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            agent_reference: agent_reference.into(),
            message: message.into(),
        }
    }
}

impl ContextualUserFragment for SubagentContextReductionWarning {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.context_reduction_warning".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<subagent_context_reduction_warning>",
            "</subagent_context_reduction_warning>",
        )
    }

    fn body(&self) -> String {
        format!(
            "\n{}\n",
            serde_json::json!({
                "agent": self.agent_reference,
                "message": self.message,
            })
        )
    }
}
