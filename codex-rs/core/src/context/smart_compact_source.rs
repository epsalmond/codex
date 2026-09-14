use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

const CONTEXT_START_MARKER: &str = "<codex_smart_compact_source>";
const CONTEXT_END_MARKER: &str = "</codex_smart_compact_source>";

/// Bounded source data sent only to the optional Luna summarizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SmartCompactSourceFragment {
    body: String,
}

impl SmartCompactSourceFragment {
    pub(crate) fn new(body: impl Into<String>) -> Self {
        Self { body: body.into() }
    }
}

impl ContextualUserFragment for SmartCompactSourceFragment {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("compaction.smart_source".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (CONTEXT_START_MARKER, CONTEXT_END_MARKER)
    }

    fn body(&self) -> String {
        self.body.clone()
    }
}
