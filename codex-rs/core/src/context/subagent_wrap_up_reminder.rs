use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

/// Tells a long-running subagent, once per turn, to finish and report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentWrapUpReminder {
    pub(crate) model_calls: u32,
}

impl ContextualUserFragment for SubagentWrapUpReminder {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.subagent_wrap_up_reminder".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<subagent_wrap_up>\n", "\n</subagent_wrap_up>")
    }

    fn body(&self) -> String {
        format!(
            "You have made {} model calls on this task. Finish the current step, then report(kind=\"final\") — or report(kind=\"blocked\") if you can't finish.",
            self.model_calls
        )
    }
}
