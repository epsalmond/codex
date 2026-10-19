use codex_protocol::AgentPath;

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterAgentCompletionMessage {
    message_type: &'static str,
    task_name: AgentPath,
    sender: AgentPath,
    payload: String,
}

impl InterAgentCompletionMessage {
    pub(crate) fn new(task_name: AgentPath, sender: AgentPath, payload: impl Into<String>) -> Self {
        Self {
            message_type: "FINAL_ANSWER",
            task_name,
            sender,
            payload: payload.into(),
        }
    }

    /// A subagent's `report_to_parent(blocked)`: it stopped and waits for instructions.
    pub(crate) fn blocked(
        task_name: AgentPath,
        sender: AgentPath,
        payload: impl Into<String>,
    ) -> Self {
        Self {
            message_type: "BLOCKED",
            ..Self::new(task_name, sender, payload)
        }
    }
}

impl ContextualUserFragment for InterAgentCompletionMessage {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.inter_agent_completion_message".to_string())
    }

    fn role(&self) -> &'static str {
        "assistant"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        format!(
            "Message Type: {}\nTask name: {}\nSender: {}\nPayload:\n{}",
            self.message_type, self.task_name, self.sender, self.payload,
        )
    }
}
