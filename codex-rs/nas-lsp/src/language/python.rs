//! Existing Pyrefly profile; see `.codex/design/nas-lsp-broker.md`.

pub(super) fn language_id(extension: &str) -> Option<&'static str> {
    matches!(extension, "py" | "pyi").then_some("python")
}
