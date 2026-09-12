//! Context-reducing surgical compaction ("shake").
//!
//! Ported from oh-my-pi's `compaction/shake.ts`: drop heavy content out of the
//! live context mechanically. Tool-call outputs and large fenced/XML blocks in
//! message text are replaced with short placeholders, and image / reasoning
//! blocks can be stripped outright. This module is the pure layer — region
//! detection and in-place mutation only. History persistence, event emission,
//! and token-usage repair are orchestrated by the caller
//! (`session::handlers::shake`).
//!
//! Manual `/shake` mirrors oh-my-pi's aggressive preset: no savings threshold,
//! and a small protected recent tail so the active working context is not
//! stripped. Automatic shake protects a much larger tail
//! (`AUTO_PROTECT_TOKENS`) since it runs unattended.
//!
//! NOTE ON OFFSETS: block detection works in *byte* offsets by iterating lines
//! through `str::split_inclusive`; region application converts the recorded
//! byte offsets back to byte indices for in-place `String` splicing. Region
//! coordinates are therefore always valid `str` byte boundaries (lines never
//! split a code point), so no char-index translation is ever required.

use codex_history::ResponseItemEnvelope;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::approx_tokens_from_byte_count;

pub(crate) mod auto;
pub(crate) mod preview;
mod recovery;
use self::recovery::is_artifact_recovery_output;
use self::recovery::recovery_placeholder;

pub(crate) use codex_protocol::protocol::ShakeMode;

/// Manual `/shake` is aggressive: no savings threshold and drops eligible
/// regions across history. Still keeps a small recent tail (~4k tokens) so it
/// cannot strip the tool outputs the agent is currently working from. Matches
/// oh-my-pi's aggressive/manual preset `protectTokens`.
pub(crate) const MANUAL_PROTECT_TOKENS: usize = 4_000;

/// Automatic shake protects a much larger recent tail than manual: it runs
/// unattended, so it must not risk stripping content the agent is actively
/// relying on this turn. Matches oh-my-pi's default automatic preset
/// `protectTokens` (`DEFAULT_SHAKE_CONFIG.protectTokens`).
pub(crate) const AUTO_PROTECT_TOKENS: usize = 16_000;

/// Minimum token size for a fenced/XML block or tool output to be eligible for
/// elision.
const FENCE_MIN_TOKENS: usize = 400;

/// A byte range `[start, end)` inside a text payload. Always lands on `str`
/// code-point boundaries because the scanner advances line-by-line and lines
/// are terminated by `'\n'` (a single-byte terminator), which cannot appear
/// inside a multi-byte UTF-8 sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextRange {
    start: usize,
    end: usize,
}

/// Detect a lowercase opening XML tag line like `<details>` or `<foo bar>`.
/// Conservative by design: lowercase tag names only, and ignored inside fenced
/// blocks (handled by the caller). Returns the tag name.
fn parse_opening_xml_tag(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix('<')?;
    if rest.starts_with('/') || rest.starts_with('!') || rest.starts_with('?') {
        return None;
    }
    let name_end = rest
        .find(|c: char| !(c.is_ascii_lowercase() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    if name_end == 0 {
        return None;
    }
    // A valid opening tag terminates with `>` somewhere on the line.
    if !rest.contains('>') {
        return None;
    }
    Some(&rest[..name_end])
}

/// Locate fenced code blocks and top-level XML element spans inside `text`.
/// Returns byte ranges covering the full block (opening and closing fence/tag
/// lines included, trailing newline excluded).
///
/// Conservative: unterminated fences/tags yield no range, and XML detection is
/// suppressed inside fences. Mirrors oh-my-pi's `scanTextForBlockRanges`.
fn scan_text_for_block_ranges(text: &str) -> Vec<TextRange> {
    let mut ranges: Vec<TextRange> = Vec::new();
    let mut in_fence = false;
    let mut fence_start = 0usize;
    let mut tag_stack: Vec<String> = Vec::new();
    let mut xml_start = 0usize;

    // `split_inclusive` keeps the trailing '\n', so line byte offsets are exact.
    let mut byte_offset = 0usize;
    for raw_line in text.split_inclusive('\n') {
        let line_start = byte_offset;
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line_end = line_start + line.len();
        byte_offset += raw_line.len();

        let trimmed = line.trim_start();
        let is_fence_line = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if is_fence_line {
            if !in_fence {
                in_fence = true;
                fence_start = line_start;
            } else {
                in_fence = false;
                ranges.push(TextRange {
                    start: fence_start,
                    end: line_end,
                });
            }
            continue;
        }

        if !in_fence {
            let unindented = line.len() == trimmed.len(); // opening tags must start a line
            if unindented
                && !trimmed.starts_with("</")
                && let Some(name) = parse_opening_xml_tag(&trimmed.to_ascii_lowercase())
            {
                if tag_stack.is_empty() {
                    xml_start = line_start;
                }
                tag_stack.push(name.to_string());
                continue;
            }
            let closing = trimmed.to_ascii_lowercase();
            if let Some(rest) = closing.strip_prefix("</")
                && let Some(gt) = rest.find('>')
            {
                let name = &rest[..gt];
                if tag_stack.last().map(String::as_str) == Some(name) {
                    tag_stack.pop();
                    if tag_stack.is_empty() {
                        ranges.push(TextRange {
                            start: xml_start,
                            end: line_end,
                        });
                    }
                }
            }
        }
    }

    // Sort ascending by start and drop any range overlapping an already-kept
    // range. Fence/XML spans are properly nested (XML suppressed inside fences),
    // so overlap means containment — keeping the earlier-starting range keeps
    // the outermost span.
    ranges.sort_by_key(|range| range.start);
    let mut kept: Vec<TextRange> = Vec::with_capacity(ranges.len());
    let mut last_end = 0usize;
    let mut have_last = false;
    for range in ranges {
        if have_last && range.start < last_end {
            continue;
        }
        last_end = range.end;
        have_last = true;
        kept.push(range);
    }
    kept
}

/// Estimated model-visible size in bytes of a response item (heuristic feeding
/// the `approx_tokens_from_byte_count` estimate for the protect-recent window).
fn item_model_visible_bytes(item: &ResponseItem) -> usize {
    let mut bytes = 0usize;
    match item {
        ResponseItem::Message { role, content, .. } => {
            bytes += role.len();
            for part in content {
                bytes += match part {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        text.len()
                    }
                    ContentItem::InputImage { image_url, .. } => image_url.len(),
                    ContentItem::InputAudio { audio_url } => audio_url.len(),
                };
            }
        }
        ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            ..
        } => {
            bytes += author.len() + recipient.len();
            for part in content {
                bytes += match part {
                    AgentMessageInputContent::InputText { text } => text.len(),
                    AgentMessageInputContent::EncryptedContent { encrypted_content } => {
                        encrypted_content.len()
                    }
                };
            }
        }
        ResponseItem::FunctionCall {
            name, arguments, ..
        } => {
            bytes += name.len() + arguments.len();
        }
        ResponseItem::CustomToolCall { name, input, .. } => {
            bytes += name.len() + input.len();
        }
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            bytes += payload_text_bytes(output);
        }
        ResponseItem::Reasoning {
            summary,
            content,
            encrypted_content,
            ..
        } => {
            for s in summary {
                let codex_protocol::models::ReasoningItemReasoningSummary::SummaryText { text } = s;
                bytes += text.len();
            }
            if let Some(content) = content {
                for c in content {
                    bytes += match c {
                        ReasoningItemContent::ReasoningText { text }
                        | ReasoningItemContent::Text { text } => text.len(),
                    };
                }
            }
            if let Some(encrypted) = encrypted_content {
                bytes += encrypted.len();
            }
        }
        _ => {}
    }
    bytes
}

fn payload_text_bytes(output: &FunctionCallOutputPayload) -> usize {
    match &output.body {
        FunctionCallOutputBody::Text(t) => t.len(),
        FunctionCallOutputBody::ContentItems(items) => items
            .iter()
            .map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => text.len(),
                FunctionCallOutputContentItem::InputImage { image_url, .. } => image_url.len(),
                FunctionCallOutputContentItem::InputAudio { audio_url } => audio_url.len(),
                FunctionCallOutputContentItem::EncryptedContent { encrypted_content } => {
                    encrypted_content.len()
                }
            })
            .sum(),
    }
}

/// The estimated token contribution of an envelope, used for the protect-recent
/// window.
fn envelope_tokens(envelope: &ResponseItemEnvelope) -> usize {
    approx_tokens_from_byte_count(item_model_visible_bytes(&envelope.item)) as usize
}

/// The text of a tool-call output plus its estimated token count, or `None`
/// when there is no text to elide.
fn tool_output_text(output: &FunctionCallOutputPayload) -> Option<(String, usize)> {
    match &output.body {
        FunctionCallOutputBody::Text(t) => {
            if t.is_empty() {
                None
            } else {
                Some((t.clone(), approx_token_count(t)))
            }
        }
        FunctionCallOutputBody::ContentItems(items) => {
            let fragments: Vec<&str> = items
                .iter()
                .filter_map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } if !text.is_empty() => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect();
            if fragments.is_empty() {
                None
            } else {
                let joined = fragments.join("\n");
                let tokens = approx_token_count(&joined);
                Some((joined, tokens))
            }
        }
    }
}

/// Replace the text of a tool output in place with `placeholder`, preserving
/// every non-text block. Returns an estimate of the reclaimed tokens.
fn elide_tool_output(output: &mut FunctionCallOutputPayload, placeholder: &str) -> usize {
    let original = tool_output_text(output).map_or(0, |(_, tokens)| tokens);
    match &mut output.body {
        FunctionCallOutputBody::Text(t) => {
            *t = placeholder.to_string();
        }
        FunctionCallOutputBody::ContentItems(items) => {
            let mut wrote_placeholder = false;
            let mut kept: Vec<FunctionCallOutputContentItem> = Vec::with_capacity(items.len());
            for item in items.drain(..) {
                match item {
                    FunctionCallOutputContentItem::InputText { .. } => {
                        if !wrote_placeholder {
                            wrote_placeholder = true;
                            kept.push(FunctionCallOutputContentItem::InputText {
                                text: placeholder.to_string(),
                            });
                        }
                    }
                    other => kept.push(other),
                }
            }
            *items = kept;
        }
    }
    original.saturating_sub(approx_token_count(placeholder))
}

/// Strip image blocks from a single item; returns the number of images removed.
fn strip_item_images(item: &mut ResponseItem) -> usize {
    match item {
        ResponseItem::Message { content, .. } => {
            let removed = content
                .iter()
                .filter(|c| matches!(c, ContentItem::InputImage { .. }))
                .count();
            content.retain(|c| !matches!(c, ContentItem::InputImage { .. }));
            removed
        }
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            let Some(items) = output.content_items_mut() else {
                return 0;
            };
            let removed = items
                .iter()
                .filter(|c| matches!(c, FunctionCallOutputContentItem::InputImage { .. }))
                .count();
            items.retain(|c| !matches!(c, FunctionCallOutputContentItem::InputImage { .. }));
            // Retain a text marker so the model still sees an output payload.
            if items.is_empty() && removed > 0 {
                items.push(FunctionCallOutputContentItem::InputText {
                    text: "[image removed]".to_string(),
                });
            }
            removed
        }
        _ => 0,
    }
}

/// A located large fenced/XML block inside a message content text part.
/// Coordinates are byte offsets into the content text.
#[derive(Debug, Clone)]
struct BlockRegion {
    message_index: usize,
    content_index: usize,
    start: usize,
    end: usize,
    tokens: usize,
    label: String,
}

fn push_block_regions(
    message_index: usize,
    content_index: usize,
    text: &str,
    label: &str,
    out: &mut Vec<BlockRegion>,
) {
    for range in scan_text_for_block_ranges(text) {
        let slice = &text[range.start..range.end];
        if slice.is_empty() || is_artifact_recovery_output(slice) {
            continue;
        }
        let tokens = approx_token_count(slice);
        if tokens < FENCE_MIN_TOKENS {
            continue;
        }
        out.push(BlockRegion {
            message_index,
            content_index,
            start: range.start,
            end: range.end,
            tokens,
            label: label.to_string(),
        });
    }
}

/// Outcome of a shake run.
#[derive(Debug, Default)]
pub(crate) struct ShakeResult {
    /// Whole tool-call outputs elided.
    pub tool_outputs_elided: usize,
    /// Large fenced/XML blocks elided.
    pub blocks_elided: usize,
    /// Image blocks removed (images mode).
    pub images_dropped: usize,
    /// Reasoning items dropped (thinking mode).
    pub thinking_dropped: usize,
    /// Estimated context tokens reclaimed.
    pub tokens_freed: i64,
}

impl ShakeResult {
    pub(crate) fn is_noop(&self) -> bool {
        self.tool_outputs_elided == 0
            && self.blocks_elided == 0
            && self.images_dropped == 0
            && self.thinking_dropped == 0
    }

    /// One-line operator summary (ported from oh-my-pi's `formatShakeSummary`).
    pub fn summary_line(&self, mode: ShakeMode) -> String {
        match mode {
            ShakeMode::Images => {
                if self.images_dropped == 0 {
                    "No images found in this conversation.".to_string()
                } else {
                    format!(
                        "Dropped {} image{} from this conversation.",
                        self.images_dropped,
                        if self.images_dropped == 1 { "" } else { "s" }
                    )
                }
            }
            ShakeMode::Thinking => {
                if self.thinking_dropped == 0 {
                    "No thinking blocks found in this conversation.".to_string()
                } else {
                    format!(
                        "Dropped {} thinking block{} from this conversation.",
                        self.thinking_dropped,
                        if self.thinking_dropped == 1 { "" } else { "s" }
                    )
                }
            }
            ShakeMode::Elide => {
                let mut parts: Vec<String> = Vec::new();
                if self.tool_outputs_elided > 0 {
                    parts.push(format!(
                        "{} tool output{}",
                        self.tool_outputs_elided,
                        if self.tool_outputs_elided == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ));
                }
                if self.blocks_elided > 0 {
                    parts.push(format!(
                        "{} block{}",
                        self.blocks_elided,
                        if self.blocks_elided == 1 { "" } else { "s" }
                    ));
                }
                if parts.is_empty() {
                    "Nothing to shake.".to_string()
                } else {
                    format!(
                        "Shook {} (~{} tokens freed).",
                        parts.join(" + "),
                        self.tokens_freed
                    )
                }
            }
        }
    }
}

/// Strip every image block out of the history envelopes.
pub(crate) fn shake_images(items: &mut [ResponseItemEnvelope]) -> ShakeResult {
    let mut result = ShakeResult::default();
    for envelope in items.iter_mut() {
        result.images_dropped += strip_item_images(&mut envelope.item);
    }
    result
}

/// Drop every reasoning item from the history envelopes.
pub(crate) fn shake_thinking(items: &mut Vec<ResponseItemEnvelope>) -> ShakeResult {
    let before = items.len();
    items.retain(|envelope| !matches!(envelope.item, ResponseItem::Reasoning { .. }));
    ShakeResult {
        thinking_dropped: before - items.len(),
        ..Default::default()
    }
}

/// Elide history while saving each original region before replacing it. A
/// failed save leaves that region untouched, so its placeholder never promises
/// recovery that is not durable.
///
/// `protect_tokens` is the size of the trailing window (measured from the most
/// recent envelope backwards) that is never eligible for elision. Callers pass
/// [`MANUAL_PROTECT_TOKENS`] for operator-driven `/shake` and
/// [`AUTO_PROTECT_TOKENS`] for the automatic pre-sampling trigger.
pub(crate) fn shake_elide_with_recovery(
    items: &mut [ResponseItemEnvelope],
    protect_tokens: usize,
    save: &mut dyn FnMut(&str, &str) -> Option<String>,
) -> ShakeResult {
    let mut result = ShakeResult::default();
    if items.is_empty() {
        return result;
    }

    // Tokens of all envelopes strictly more recent than index i.
    let mut accumulated_after = vec![0usize; items.len()];
    let mut acc = 0usize;
    for i in (0..items.len()).rev() {
        accumulated_after[i] = acc;
        acc += envelope_tokens(&items[i]);
    }

    let mut block_regions: Vec<BlockRegion> = Vec::new();

    for (i, envelope) in items.iter_mut().enumerate() {
        if accumulated_after[i] < protect_tokens {
            continue;
        }
        match &mut envelope.item {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                let Some((original, tokens)) = tool_output_text(output) else {
                    continue;
                };
                if tokens < FENCE_MIN_TOKENS || is_artifact_recovery_output(&original) {
                    continue;
                }
                let Some(placeholder) =
                    recovery_placeholder(save, &original, tokens, "tool output")
                else {
                    continue;
                };
                let freed = elide_tool_output(output, &placeholder);
                result.tool_outputs_elided += 1;
                result.tokens_freed += freed as i64;
            }
            ResponseItem::Message { role, content, .. } => {
                for (content_index, part) in content.iter().enumerate() {
                    if let ContentItem::InputText { text } | ContentItem::OutputText { text } = part
                    {
                        push_block_regions(i, content_index, text, role, &mut block_regions);
                    }
                }
            }
            _ => {}
        }
    }

    // Apply block regions inside-out (highest byte-offset start first) so
    // splicing one region never shifts the byte offsets of another in the same
    // text block.
    block_regions.sort_by(|a, b| {
        (b.message_index, b.content_index, b.start).cmp(&(
            a.message_index,
            a.content_index,
            a.start,
        ))
    });
    for region in &block_regions {
        let Some(envelope) = items.get_mut(region.message_index) else {
            continue;
        };
        let ResponseItem::Message { content, .. } = &mut envelope.item else {
            continue;
        };
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
            continue;
        };
        text.replace_range(region.start..region.end, &placeholder);
        result.blocks_elided += 1;
        result.tokens_freed += region
            .tokens
            .saturating_sub(approx_token_count(&placeholder)) as i64;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn output_item(call_id: &str, text: &str) -> ResponseItemEnvelope {
        ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id.to_string()),
            name: Some("shell".to_string()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text(text.to_string()),
            internal_chat_message_metadata_passthrough: None,
        })
    }

    fn big_lines(count: usize, prefix: &str) -> String {
        (0..count)
            .map(|i| format!("{prefix} number {i} with some padding words here"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// ~600+ tokens inside a fenced block.
    fn big_fenced_block() -> String {
        format!("```\n{}\n```", big_lines(700, "code line"))
    }

    /// Tail of envelopes sized to clear the 4k-token protect window so older
    /// history above it is eligible for elision.
    fn tail_pad() -> Vec<ResponseItemEnvelope> {
        let mut out = Vec::new();
        for i in 0..32 {
            out.push(text_item(
                "user",
                &format!("follow-up {i}: {}", "some padding words ".repeat(30)),
            ));
            out.push(text_item(
                "assistant",
                &format!("short answer {i}: {}", "more padding words ".repeat(30)),
            ));
        }
        out
    }

    #[test]
    fn scan_finds_fenced_block_range() {
        let text = format!("intro\n{}\noutro", big_fenced_block());
        let ranges = scan_text_for_block_ranges(&text);
        assert_eq!(ranges.len(), 1);
        let slice = &text[ranges[0].start..ranges[0].end];
        assert!(slice.starts_with("```"));
        assert!(slice.ends_with("```"));
        assert!(!slice.contains("intro"));
        assert!(!slice.contains("outro"));
    }

    #[test]
    fn scan_regions_land_on_utf8_boundaries_with_multibyte_content() {
        // Ensure byte offsets remain valid `str` boundaries even when the text
        // contains multi-byte characters around the block.
        let text = format!("émoji ✨ intro\n{}\n✓ outro", big_fenced_block());
        let ranges = scan_text_for_block_ranges(&text);
        assert_eq!(ranges.len(), 1);
        let slice = &text[ranges[0].start..ranges[0].end];
        assert!(slice.starts_with("```"));
        assert!(slice.ends_with("```"));
    }

    #[test]
    fn scan_ignores_unterminated_fence() {
        let text = "before\n```\nunterminated block body".to_string();
        assert!(scan_text_for_block_ranges(&text).is_empty());
    }

    #[test]
    fn scan_finds_top_level_xml_block() {
        let text = "lead\n<details>\nlots of content\nmore content\n</details>\ntrail";
        let ranges = scan_text_for_block_ranges(text);
        assert_eq!(ranges.len(), 1);
        let slice = &text[ranges[0].start..ranges[0].end];
        assert!(slice.starts_with("<details>"));
        assert!(slice.ends_with("</details>"));
        assert!(!slice.contains("lead"));
    }

    #[test]
    fn scan_suppresses_xml_inside_fence() {
        let text = "```\n<details>\ninside\n</details>\n```";
        let ranges = scan_text_for_block_ranges(text);
        assert_eq!(ranges.len(), 1); // fence only; XML inside is not a region
    }

    #[test]
    fn shake_elide_replaces_large_tool_output_beyond_window() {
        let big = big_lines(1200, "heavy tool output line");
        let mut items = vec![
            text_item("user", "run the thing"),
            output_item("call-1", &big),
        ];
        items.extend(tail_pad());
        let mut save = |_content: &str, _label: &str| {
            Some("artifact://00000000000000000000000000000000".to_string())
        };
        let result = shake_elide_with_recovery(&mut items, MANUAL_PROTECT_TOKENS, &mut save);
        assert_eq!(result.tool_outputs_elided, 1);
        assert!(result.tokens_freed > 0);
        let ResponseItem::FunctionCallOutput { output, .. } = &items[1].item else {
            panic!("expected output");
        };
        let text = output.body.to_text().expect("text output");
        assert!(text.starts_with("[shaken ~"), "got: {text}");
    }

    #[test]
    fn shake_elide_skips_recent_tool_output() {
        let big = big_lines(1200, "heavy tool output line");
        // The big output is the newest item -> protected (inside window).
        let mut items = vec![
            text_item("user", "run the thing"),
            output_item("call-1", &big),
        ];
        let mut save = |_content: &str, _label: &str| {
            Some("artifact://00000000000000000000000000000000".to_string())
        };
        let result = shake_elide_with_recovery(&mut items, MANUAL_PROTECT_TOKENS, &mut save);
        assert_eq!(result.tool_outputs_elided, 0);
    }

    #[test]
    fn shake_elide_preserves_original_when_artifact_save_fails() {
        let original = big_lines(1200, "heavy tool output line");
        let mut items = vec![output_item("call-1", &original)];
        items.extend(tail_pad());
        let mut save = |_content: &str, _label: &str| None;

        let result = shake_elide_with_recovery(&mut items, MANUAL_PROTECT_TOKENS, &mut save);

        assert_eq!(result.tool_outputs_elided, 0);
        let ResponseItem::FunctionCallOutput { output, .. } = &items[0].item else {
            panic!("expected output");
        };
        assert_eq!(output.body.to_text().expect("text output"), original);
    }

    #[test]
    fn shake_elide_protects_recovered_output() {
        let recovered = "[artifact source: artifact://00000000000000000000000000000000; more content; use start_byte=3071]\n";
        let mut items = vec![output_item("call-1", &recovered.repeat(60))];
        items.extend(tail_pad());

        let mut save =
            |_content: &str, _label: &str| panic!("recovered output should not be saved again");
        let result = shake_elide_with_recovery(&mut items, MANUAL_PROTECT_TOKENS, &mut save);

        assert_eq!(result.tool_outputs_elided, 0);
    }

    #[test]
    fn shake_elide_does_not_protect_plain_artifact_uri_text() {
        let plain =
            "plain text mentioning artifact://00000000000000000000000000000000\n".repeat(1_200);
        let mut items = vec![output_item("call-1", &plain)];
        items.extend(tail_pad());
        let mut save = |_content: &str, _label: &str| {
            Some("artifact://00000000000000000000000000000001".to_string())
        };

        let result = shake_elide_with_recovery(&mut items, MANUAL_PROTECT_TOKENS, &mut save);

        assert_eq!(result.tool_outputs_elided, 1);
    }

    #[test]
    fn shake_elide_replaces_large_fence_in_message() {
        let mut items = vec![text_item("assistant", &big_fenced_block())];
        items.extend(tail_pad());
        let mut save = |_content: &str, _label: &str| {
            Some("artifact://00000000000000000000000000000000".to_string())
        };
        let result = shake_elide_with_recovery(&mut items, MANUAL_PROTECT_TOKENS, &mut save);
        assert!(
            result.blocks_elided >= 1,
            "expected a block elision: {result:?}"
        );
        let ResponseItem::Message { content, .. } = &items[0].item else {
            panic!("expected message");
        };
        let ContentItem::InputText { text } = &content[0] else {
            panic!("expected text");
        };
        assert!(text.contains("[shaken ~"), "got: {text}");
    }

    #[test]
    fn shake_images_strips_from_outputs_and_messages() {
        let mut items = vec![
            ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("call-img".to_string()),
                name: Some("view_image".to_string()),
                namespace: None,
                output: FunctionCallOutputPayload::from_content_items(vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "see image".to_string(),
                    },
                    FunctionCallOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,AAA".to_string(),
                        detail: None,
                    },
                ]),
                internal_chat_message_metadata_passthrough: None,
            }),
            ResponseItemEnvelope::new(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: "look".to_string(),
                    },
                    ContentItem::InputImage {
                        image_url: "data:image/png;base64,BBB".to_string(),
                        detail: None,
                    },
                ],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
        ];
        let result = shake_images(&mut items);
        assert_eq!(result.images_dropped, 2);
        for envelope in &items {
            match &envelope.item {
                ResponseItem::Message { content, .. } => assert!(
                    !content
                        .iter()
                        .any(|c| matches!(c, ContentItem::InputImage { .. }))
                ),
                ResponseItem::FunctionCallOutput { output, .. } => {
                    let FunctionCallOutputBody::ContentItems(items) = &output.body else {
                        panic!("expected content items");
                    };
                    assert!(
                        !items
                            .iter()
                            .any(|c| matches!(c, FunctionCallOutputContentItem::InputImage { .. }))
                    );
                }
                _ => {}
            }
        }
    }

    #[test]
    fn shake_thinking_drops_reasoning_items() {
        let mut items = vec![
            ResponseItemEnvelope::new(ResponseItem::Reasoning {
                id: None,
                summary: vec![],
                content: None,
                encrypted_content: None,
                internal_chat_message_metadata_passthrough: None,
            }),
            text_item("user", "hello"),
            ResponseItemEnvelope::new(ResponseItem::Reasoning {
                id: None,
                summary: vec![],
                content: None,
                encrypted_content: None,
                internal_chat_message_metadata_passthrough: None,
            }),
        ];
        let result = shake_thinking(&mut items);
        assert_eq!(result.thinking_dropped, 2);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn summary_line_formats_counts() {
        let result = ShakeResult {
            tool_outputs_elided: 3,
            blocks_elided: 1,
            tokens_freed: 1234,
            ..Default::default()
        };
        assert_eq!(
            result.summary_line(ShakeMode::Elide),
            "Shook 3 tool outputs + 1 block (~1234 tokens freed)."
        );
        let empty = ShakeResult::default();
        assert_eq!(empty.summary_line(ShakeMode::Elide), "Nothing to shake.");
        assert_eq!(
            empty.summary_line(ShakeMode::Images),
            "No images found in this conversation."
        );
        assert_eq!(
            empty.summary_line(ShakeMode::Thinking),
            "No thinking blocks found in this conversation."
        );
    }
}
