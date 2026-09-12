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
        // `/shake`'s preview measures what a manual shake would do, so it uses
        // the manual protect-tail size.
        let estimate = estimate_shake(
            items,
            mode,
            super::MANUAL_PROTECT_TOKENS,
            /*persistent_thread*/ !ephemeral,
        );
        let fingerprint = fingerprint(items, mode)?;
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

/// Run the real transformation on a copy of `items`, writing no artifacts.
///
/// The save closure returns a fixed URI of the same encoded length as a real
/// artifact id so the measured placeholder size matches what a confirmed shake
/// would actually insert.
pub(crate) fn estimate_shake(
    items: &[ResponseItemEnvelope],
    mode: ShakeMode,
    protect_tokens: usize,
    persistent_thread: bool,
) -> ShakeEstimate {
    let tokens_before = sum_item_tokens(items);
    let mut reduced = items.to_vec();
    let unavailable_reason = (!persistent_thread && mode == ShakeMode::Elide).then(|| {
        "Elide requires a persistent thread so removed text can be recovered.".to_string()
    });
    let result = if unavailable_reason.is_some() {
        super::ShakeResult::default()
    } else {
        match mode {
            ShakeMode::Elide => {
                super::shake_elide_with_recovery(&mut reduced, protect_tokens, &mut |content, _label| {
                    // Match the store's size limit and the real UUID's encoded length.
                    (content.len() as u64 <= MAX_ARTIFACT_BYTES)
                        .then(|| "artifact://00000000000000000000000000000000".to_string())
                })
            }
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
) -> serde_json::Result<String> {
    // This digest detects stale previews; it is not an authorization token.
    let mut digest = Sha1::new();
    digest.update(mode.as_str());
    for envelope in items {
        digest.update(serde_json::to_vec(&envelope.item)?);
    }
    Ok(format!("{:x}", digest.finalize()))
}
