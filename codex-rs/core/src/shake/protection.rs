//! Tool-identity protection: outputs shake must never elide.
//!
//! Ported from oh-my-pi's `compaction/tool-protection.ts`. omp matches a
//! `protectedTools` list *before* elision, so a skill read or an artifact
//! recovery read is never a candidate in the first place. The fork previously
//! had only the content-marker guard in [`super::recovery`], which stops
//! *re*-eliding text that already carries an `artifact://` / `[shaken …]`
//! marker but does nothing about the first elision of a skill read.
//!
//! Protection here is keyed on the *tool identity* of the call that produced an
//! output, not on the output's text, and applies to both the manual `/shake`
//! path and the automatic pre-sampling path (omp protects skill reads in its
//! default and aggressive presets alike). Tool outputs carry no name on the
//! wire — `ResponseInputItem::FunctionCallOutput` has no `name` field, and the
//! conversion into `ResponseItem::FunctionCallOutput` fills `name: None` — so
//! the name is recovered by pairing an output with its `call_id`'s
//! `FunctionCall` / `CustomToolCall`.

use std::collections::HashSet;

use codex_history::ResponseItemEnvelope;
use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::models::ResponseItem;

/// Tools whose outputs shake never elides, as `(namespace, name)` pairs.
/// `None` is the default (top-level) function namespace.
///
/// Every entry is a name registered by a real handler in this fork:
///
/// | Entry | Registered by |
/// | --- | --- |
/// | `read_artifact` | `tools::handlers::read_artifact_spec::READ_ARTIFACT_TOOL_NAME` |
/// | `skills.read` | `ext/skills/src/tools/read.rs` (`skills` namespace) |
/// | `skills.list` | `ext/skills/src/tools/list.rs` (`skills` namespace) |
///
/// `read_artifact` is the fork's artifact-recovery read — omp's
/// `isArtifactRecoveryToolResult`. Eliding a recovery read only mints another
/// artifact and can repeat indefinitely. The `skills` namespace is the fork's
/// equivalent of omp's literal `"skill"` tool name plus its
/// `isSkillReadToolResult` (`skill://` path) matcher: skill content is loaded
/// deliberately and is what the agent is working from.
pub(crate) const PROTECTED_TOOLS: &[(Option<&str>, &str)] = &[
    (None, "read_artifact"),
    (Some("skills"), "read"),
    (Some("skills"), "list"),
];

/// True when `namespace`/`name` identify a [`PROTECTED_TOOLS`] entry.
///
/// The default function namespace is spelled `None`, `Some("")` or
/// `Some("functions")` depending on which layer produced the item, so all three
/// are normalized to "no namespace" before comparing.
pub(crate) fn is_protected_tool(namespace: Option<&str>, name: &str) -> bool {
    let namespace =
        namespace.filter(|value| !value.is_empty() && *value != DEFAULT_FUNCTION_NAMESPACE);
    PROTECTED_TOOLS
        .iter()
        .any(|(protected_namespace, protected_name)| {
            *protected_namespace == namespace && *protected_name == name
        })
}

/// One flag per envelope: `true` when that envelope is a tool output belonging
/// to a protected tool. Non-output items are always `false`.
///
/// Computed in a read-only pass so the caller can keep its single mutable walk
/// over `items` (same shape as the protect-recent token accumulation).
pub(crate) fn protected_output_flags(items: &[ResponseItemEnvelope]) -> Vec<bool> {
    let mut protected_call_ids: HashSet<&str> = HashSet::new();
    for envelope in items {
        match &envelope.item {
            ResponseItem::FunctionCall {
                call_id,
                name,
                namespace,
                ..
            } => {
                if is_protected_tool(namespace.as_deref(), name) {
                    protected_call_ids.insert(call_id.as_str());
                }
            }
            ResponseItem::CustomToolCall {
                call_id,
                name,
                namespace,
                ..
            } => {
                if is_protected_tool(namespace.as_deref(), name) {
                    protected_call_ids.insert(call_id.as_str());
                }
            }
            _ => {}
        }
    }

    items
        .iter()
        .map(|envelope| match &envelope.item {
            // An output's own `name`/`namespace` is usually absent, but honor it
            // when a producer did set it (e.g. a replayed rollout).
            ResponseItem::FunctionCallOutput {
                call_id,
                name,
                namespace,
                ..
            } => {
                name.as_deref()
                    .is_some_and(|name| is_protected_tool(namespace.as_deref(), name))
                    || call_id
                        .as_deref()
                        .is_some_and(|call_id| protected_call_ids.contains(call_id))
            }
            ResponseItem::CustomToolCallOutput { call_id, name, .. } => {
                name.as_deref()
                    .is_some_and(|name| is_protected_tool(/*namespace*/ None, name))
                    || protected_call_ids.contains(call_id.as_str())
            }
            _ => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn default_namespace_spellings_are_equivalent() {
        for namespace in [None, Some(""), Some(DEFAULT_FUNCTION_NAMESPACE)] {
            assert!(
                is_protected_tool(namespace, "read_artifact"),
                "{namespace:?}"
            );
        }
        // A namespaced entry must not match in the default namespace, and vice
        // versa.
        assert!(!is_protected_tool(None, "read"));
        assert!(!is_protected_tool(Some("skills"), "read_artifact"));
        assert!(is_protected_tool(Some("skills"), "read"));
        assert!(is_protected_tool(Some("skills"), "list"));
        assert!(!is_protected_tool(Some("skills"), "write"));
        assert!(!is_protected_tool(None, "exec_command"));
    }

    #[test]
    fn output_flags_pair_with_the_originating_call() {
        let items = vec![
            ResponseItemEnvelope::new(ResponseItem::FunctionCall {
                id: None,
                name: "read".to_string(),
                namespace: Some("skills".to_string()),
                arguments: "{}".to_string(),
                encrypted_function_args: None,
                call_id: "skill-call".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }),
            ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("skill-call".to_string()),
                name: None,
                namespace: None,
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    "skill body".to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            }),
            ResponseItemEnvelope::new(ResponseItem::FunctionCall {
                id: None,
                name: "exec_command".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                encrypted_function_args: None,
                call_id: "shell-call".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }),
            ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("shell-call".to_string()),
                name: None,
                namespace: None,
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    "shell body".to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            }),
        ];
        assert_eq!(
            protected_output_flags(&items),
            vec![false, true, false, false]
        );
    }
}
