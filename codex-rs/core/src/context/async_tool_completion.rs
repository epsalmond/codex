use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;

/// A correlated terminal result appended after the original tool has yielded.
#[derive(Clone, Debug)]
pub(crate) struct AsyncToolCompletion {
    pub(crate) text: String,
}

impl ContextualUserFragment for AsyncToolCompletion {
    fn role(&self) -> &'static str {
        "user"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("tools.async_completion".to_owned())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<async_tool_completion>", "</async_tool_completion>")
    }

    fn body(&self) -> ContentItem {
        input_text(self.text.clone())
    }
}
