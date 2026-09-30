use super::BlockRegion;
use super::FENCE_MIN_TOKENS;
use super::ShakeResult;
use super::elide_tool_output;
use super::envelope_tokens;
use super::push_block_regions;
use super::recovery::is_artifact_recovery_output as is_recovery_placeholder;
use super::recovery_placeholder;
use super::tool_output_text;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

/// Shake a bounded history interval while preserving the first envelope whose
/// artifact save fails and every later envelope.
pub(crate) fn shake_elide_watermarked(
    items: &mut [ResponseItemEnvelope],
    start: usize,
    requested_end: usize,
    protect_tokens: usize,
    save: &mut dyn FnMut(&str, &str) -> Option<String>,
) -> (ShakeResult, usize) {
    let start = start.min(items.len());
    let mut accumulated_after = vec![0usize; items.len()];
    let mut accumulated_tokens = 0usize;
    for index in (0..items.len()).rev() {
        accumulated_after[index] = accumulated_tokens;
        accumulated_tokens = accumulated_tokens.saturating_add(envelope_tokens(&items[index]));
    }
    let recency_end = (0..items.len())
        .find(|index| accumulated_after[*index] < protect_tokens)
        .unwrap_or(items.len());
    let end = super::watermark::close_over_tool_calls(
        items,
        requested_end.min(recency_end).min(items.len()),
    )
    .max(start);
    let protected_output = super::protection::protected_output_flags(items);
    let mut result = ShakeResult::default();

    for index in start..end {
        if protected_output[index] {
            continue;
        }
        let mut staged = items[index].clone();
        let mut staged_result = ShakeResult::default();
        match &mut staged.item {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                let Some((original, tokens)) = tool_output_text(output) else {
                    continue;
                };
                if tokens < FENCE_MIN_TOKENS || is_recovery_placeholder(&original) {
                    continue;
                }
                let Some(placeholder) =
                    recovery_placeholder(save, &original, tokens, "tool output")
                else {
                    let boundary = super::watermark::close_over_tool_calls(items, index).max(start);
                    return (result, boundary);
                };
                let freed = elide_tool_output(output, &placeholder);
                staged_result.tool_outputs_elided += 1;
                staged_result.tokens_freed += freed as i64;
            }
            ResponseItem::Message { role, content, .. } => {
                let mut block_regions: Vec<BlockRegion> = Vec::new();
                for (content_index, part) in content.iter().enumerate() {
                    if let ContentItem::InputText { text } | ContentItem::OutputText { text } = part
                    {
                        push_block_regions(index, content_index, text, role, &mut block_regions);
                    }
                }
                block_regions.sort_by(|a, b| {
                    (b.message_index, b.content_index, b.start).cmp(&(
                        a.message_index,
                        a.content_index,
                        a.start,
                    ))
                });
                for region in &block_regions {
                    let Some(ContentItem::InputText { text } | ContentItem::OutputText { text }) =
                        content.get_mut(region.content_index)
                    else {
                        continue;
                    };
                    let original = &text[region.start..region.end];
                    let Some(placeholder) = recovery_placeholder(
                        save,
                        original,
                        region.tokens,
                        &format!("{} block", region.label),
                    ) else {
                        return (result, index);
                    };
                    text.replace_range(region.start..region.end, &placeholder);
                    staged_result.blocks_elided += 1;
                    staged_result.tokens_freed += region.tokens.saturating_sub(
                        codex_utils_output_truncation::approx_token_count(&placeholder),
                    ) as i64;
                }
            }
            _ => {}
        }
        if !staged_result.is_noop() {
            items[index] = staged;
            result.tool_outputs_elided += staged_result.tool_outputs_elided;
            result.blocks_elided += staged_result.blocks_elided;
            result.tokens_freed = result
                .tokens_freed
                .saturating_add(staged_result.tokens_freed);
        }
    }

    (result, end)
}

#[cfg(test)]
#[path = "elide_tests.rs"]
mod tests;
