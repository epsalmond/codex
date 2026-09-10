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
        let fingerprint = fingerprint(items, mode)?;
        let tokens_before = items
            .iter()
            .map(|envelope| estimate_item_token_count(&envelope.item))
            .fold(/*init*/ 0i64, i64::saturating_add);
        let mut reduced = items.to_vec();
        let unavailable_reason = (ephemeral && mode == ShakeMode::Elide).then(|| {
            "Elide requires a persistent thread so removed text can be recovered.".to_string()
        });
        let result = if unavailable_reason.is_some() {
            super::ShakeResult::default()
        } else {
            match mode {
                ShakeMode::Elide => {
                    super::shake_elide_with_recovery(&mut reduced, &mut |content, _label| {
                        // Match the store's size limit and the real UUID's encoded length.
                        (content.len() as u64 <= MAX_ARTIFACT_BYTES)
                            .then(|| "artifact://00000000000000000000000000000000".to_string())
                    })
                }
                ShakeMode::Images => super::shake_images(&mut reduced),
                ShakeMode::Thinking => super::shake_thinking(&mut reduced),
            }
        };
        let tokens_after = reduced
            .iter()
            .map(|envelope| estimate_item_token_count(&envelope.item))
            .fold(/*init*/ 0i64, i64::saturating_add);
        Ok(ShakePreview {
            fingerprint,
            tokens_before,
            tokens_after,
            tool_outputs: result.tool_outputs_elided.try_into()?,
            text_blocks: result.blocks_elided.try_into()?,
            images: result.images_dropped.try_into()?,
            thinking_blocks: result.thinking_dropped.try_into()?,
            unavailable_reason,
        })
    }
}

pub(crate) fn fingerprint(
    items: &[ResponseItemEnvelope],
    mode: ShakeMode,
) -> serde_json::Result<String> {
    // This digest detects stale previews; it is not an authorization token.
    let mut digest = Sha1::new();
    digest.update(mode.as_str());
    for envelope in items {
        digest.update(serde_json::to_vec(&envelope.item)?);
    }
    Ok(format!("{:x}", digest.finalize()))
}
