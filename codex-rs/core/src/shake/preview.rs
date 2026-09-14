//! Preview the real transformation on a copy, without writing recovery artifacts.

use codex_history::ResponseItemEnvelope;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::shake::ShakePreview;
use sha1::Digest;
use sha1::Sha1;

use crate::CodexThread;
use crate::artifacts::MAX_ARTIFACT_BYTES;
use crate::context_manager::estimate_item_token_count;

impl CodexThread {
    /// Measure a shake without changing history, usage accounting, or artifact storage.
    pub async fn preview_shake(&self, mode: ShakeMode) -> anyhow::Result<ShakePreview> {
        anyhow::ensure!(
            self.session.active_turn.lock().await.is_none(),
            "Cannot preview shake while a turn is in progress."
        );
        let history = self.session.clone_history().await;
        let ephemeral = self.session.get_config().await.ephemeral;
        let items = history.annotated_items();
        let artifact_store = self.session.artifact_store().await;
        // `/shake`'s preview measures what a manual shake would do, so it uses
        // the manual protect-tail size.
        let estimate = estimate_shake(
            items,
            mode,
            super::MANUAL_PROTECT_TOKENS,
            /*persistent_thread*/ !ephemeral,
            &artifact_store,
        );
        let smart_context = if mode == ShakeMode::SmartCompact {
            Some(format!(
                "{}|{}",
                self.session.effective_model_slug().await,
                self.session.provider().await.name,
            ))
        } else {
            None
        };
        let fingerprint = fingerprint(items, mode, smart_context.as_deref())?;
        Ok(ShakePreview {
            fingerprint,
            tokens_before: estimate.tokens_before,
            tokens_after: estimate.tokens_after,
            tool_outputs: estimate.result.tool_outputs_elided.try_into()?,
            text_blocks: estimate.result.blocks_elided.try_into()?,
            images: estimate.result.images_dropped.try_into()?,
            thinking_blocks: estimate.result.thinking_dropped.try_into()?,
            unavailable_reason: estimate.unavailable_reason,
        })
    }
}

/// Read-only measurement of a shake, shared by `/shake`'s confirmation preview
/// and by the auto-shake decision in `session::turn`.
pub(crate) struct ShakeEstimate {
    pub(crate) result: super::ShakeResult,
    pub(crate) tokens_before: i64,
    pub(crate) tokens_after: i64,
    pub(crate) unavailable_reason: Option<String>,
}

/// A fixed dummy id of the same encoded length as a real `Uuid::simple()`
/// artifact id, used so a preview's placeholder size matches what a confirmed
/// shake would actually insert without writing anything.
const DUMMY_ARTIFACT_ID: &str = "00000000000000000000000000000000";

/// Run the real transformation on a copy of `items`, writing no artifacts.
///
/// The save closure returns a path built from `artifact_store` (whose root
/// reflects the real `$CODEX_HOME`/thread-id) with a dummy id of the same
/// encoded length as a real UUID and the region's real label, so the
/// measured placeholder size matches what a confirmed shake would actually
/// insert — including the file path.
pub(crate) fn estimate_shake(
    items: &[ResponseItemEnvelope],
    mode: ShakeMode,
    protect_tokens: usize,
    persistent_thread: bool,
    artifact_store: &crate::artifacts::ArtifactStore,
) -> ShakeEstimate {
    let tokens_before = sum_item_tokens(items);
    let mut reduced = items.to_vec();
    let unavailable_reason = (!persistent_thread
        && matches!(mode, ShakeMode::Elide | ShakeMode::SmartCompact))
    .then(|| "Elide requires a persistent thread so removed text can be recovered.".to_string());
    let result = if unavailable_reason.is_some() {
        super::ShakeResult::default()
    } else {
        match mode {
            ShakeMode::Elide | ShakeMode::SmartCompact => super::shake_elide_with_recovery(
                &mut reduced,
                protect_tokens,
                &mut |content, label| {
                    // Match the store's size limit and the real UUID's encoded length.
                    (content.len() as u64 <= MAX_ARTIFACT_BYTES).then(|| {
                        artifact_store
                            .destination_path(DUMMY_ARTIFACT_ID, label)
                            .display()
                            .to_string()
                    })
                },
            ),
            ShakeMode::Images => super::shake_images(&mut reduced),
            ShakeMode::Thinking => super::shake_thinking(&mut reduced),
        }
    };
    ShakeEstimate {
        result,
        tokens_before,
        tokens_after: sum_item_tokens(&reduced),
        unavailable_reason,
    }
}

fn sum_item_tokens(items: &[ResponseItemEnvelope]) -> i64 {
    items
        .iter()
        .map(|envelope| estimate_item_token_count(&envelope.item))
        .fold(/*init*/ 0i64, i64::saturating_add)
}

pub(crate) fn fingerprint(
    items: &[ResponseItemEnvelope],
    mode: ShakeMode,
    smart_context: Option<&str>,
) -> serde_json::Result<String> {
    // This digest detects stale previews; it is not an authorization token.
    let mut digest = Sha1::new();
    digest.update(mode.as_str());
    if let Some(smart_context) = smart_context {
        digest.update("smartCompact-context");
        digest.update(smart_context);
    }
    for envelope in items {
        digest.update(serde_json::to_vec(&envelope.item)?);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;

    fn text_item(role: &str, text: &str) -> ResponseItemEnvelope {
        ResponseItemEnvelope::new(ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        })
    }

    /// History whose only elidable-sized tool output belongs to `name` in
    /// `namespace`, plus enough plain-text tail to clear the protect window.
    fn history(namespace: Option<&str>, name: &str) -> Vec<ResponseItemEnvelope> {
        let big = "protected tool output line with padding words\n".repeat(/*n*/ 1_200);
        let mut items = vec![
            ResponseItemEnvelope::new(ResponseItem::FunctionCall {
                id: None,
                name: name.to_string(),
                namespace: namespace.map(str::to_string),
                arguments: "{}".to_string(),
                encrypted_function_args: None,
                call_id: "preview-call".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }),
            ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("preview-call".to_string()),
                name: None,
                namespace: None,
                output: FunctionCallOutputPayload::from_text(big),
                internal_chat_message_metadata_passthrough: None,
            }),
        ];
        for index in 0..32 {
            items.push(text_item(
                "assistant",
                &format!("tail {index}: {}", "some padding words ".repeat(/*n*/ 30)),
            ));
        }
        items
    }

    /// The preview must not count a protected tool's output, so `/shake`'s
    /// confirmation prompt never promises savings a shake cannot deliver.
    #[test]
    fn preview_does_not_count_protected_tool_outputs() {
        let store = crate::artifacts::ArtifactStore::for_thread(
            std::path::Path::new("/codex-home"),
            "preview-thread",
        );
        let estimate = estimate_shake(
            &history(Some("skills"), "read"),
            ShakeMode::Elide,
            super::super::MANUAL_PROTECT_TOKENS,
            /*persistent_thread*/ true,
            &store,
        );
        assert_eq!(estimate.result.tool_outputs_elided, 0);
        assert_eq!(estimate.tokens_after, estimate.tokens_before);

        // Same history with an unprotected tool name is counted.
        let estimate = estimate_shake(
            &history(/*namespace*/ None, "exec_command"),
            ShakeMode::Elide,
            super::super::MANUAL_PROTECT_TOKENS,
            /*persistent_thread*/ true,
            &store,
        );
        assert_eq!(estimate.result.tool_outputs_elided, 1);
        assert!(estimate.tokens_after < estimate.tokens_before);
    }

    #[test]
    fn smart_compact_fingerprint_binds_model_provider_context() {
        let items = history(/*namespace*/ None, "exec_command");
        let openai_astra = fingerprint(&items, ShakeMode::SmartCompact, Some("gpt-6-astra|OpenAI"))
            .expect("fingerprint should serialize");
        let other_provider = fingerprint(
            &items,
            ShakeMode::SmartCompact,
            Some("gpt-6-astra|Other provider"),
        )
        .expect("fingerprint should serialize");
        assert_ne!(openai_astra, other_provider);
        let mechanical =
            fingerprint(&items, ShakeMode::Elide, None).expect("fingerprint should serialize");
        assert_ne!(openai_astra, mechanical);
    }
}
