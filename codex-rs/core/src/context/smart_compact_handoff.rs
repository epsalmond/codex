use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use std::path::Path;
use std::path::PathBuf;

const CONTEXT_START_MARKER: &str = "<codex_smart_compact_handoff>";
const CONTEXT_END_MARKER: &str = "</codex_smart_compact_handoff>";

/// Durable context produced by the optional Luna pass after an explicit
/// smart-compact shake.
///
/// The body is historical derived data. It is inserted as a user-role context
/// fragment so the normal context parser can distinguish it from a real user
/// request and from authorization messages when history is resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SmartCompactHandoff {
    body: String,
    artifact_path: PathBuf,
}

impl SmartCompactHandoff {
    pub(crate) fn new(body: impl Into<String>, artifact_path: PathBuf) -> Self {
        Self {
            body: body.into(),
            artifact_path,
        }
    }

    pub(crate) fn artifact_path(&self) -> &Path {
        &self.artifact_path
    }
}

impl ContextualUserFragment for SmartCompactHandoff {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("compaction.smart_handoff".to_string())
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
