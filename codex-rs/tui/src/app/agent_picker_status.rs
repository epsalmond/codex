//! Bounded response and context details shown in `/subagents` picker rows.

use crate::multi_agents::AgentPickerContextUsage;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::models::MessagePhase;
use unicode_segmentation::UnicodeSegmentation;

const MAX_AGENT_PICKER_PREVIEW_CHARS: usize = 512;

pub(super) fn latest_completed_response_preview(turns: &[Turn]) -> Option<String> {
    turns
        .iter()
        .filter(|turn| turn.status != TurnStatus::InProgress)
        .find_map(response_preview_for_turn)
}

pub(super) fn response_preview_for_turn(turn: &Turn) -> Option<String> {
    if turn.status == TurnStatus::InProgress {
        return None;
    }

    let explicit_final = turn.items.iter().rev().find_map(|item| match item {
        ThreadItem::AgentMessage {
            text,
            phase: Some(MessagePhase::FinalAnswer),
            ..
        } => normalized_preview(text),
        _ => None,
    });
    explicit_final.or_else(|| {
        turn.items
            .iter()
            .rev()
            .find_map(|item| match item {
                ThreadItem::AgentMessage { text, phase, .. } => Some((text, phase)),
                _ => None,
            })
            .and_then(|(text, phase)| phase.is_none().then(|| normalized_preview(text)).flatten())
    })
}

fn normalized_preview(text: &str) -> Option<String> {
    let mut preview = String::new();
    let mut chars = 0;
    let mut last_grapheme_start = 0;
    let mut chars_before_last_grapheme = 0;
    let mut has_text = false;
    for word in text.split_whitespace() {
        if has_text {
            if chars == MAX_AGENT_PICKER_PREVIEW_CHARS {
                preview.truncate(last_grapheme_start);
                preview.push('…');
                return Some(preview);
            }
            last_grapheme_start = preview.len();
            chars_before_last_grapheme = chars;
            preview.push(' ');
            chars += 1;
        }
        for grapheme in word.graphemes(true) {
            let grapheme_chars = grapheme.chars().count();
            if chars + grapheme_chars > MAX_AGENT_PICKER_PREVIEW_CHARS {
                if chars == MAX_AGENT_PICKER_PREVIEW_CHARS {
                    preview.truncate(last_grapheme_start);
                    chars = chars_before_last_grapheme;
                }
                if preview.ends_with(' ') {
                    preview.pop();
                }
                if chars < MAX_AGENT_PICKER_PREVIEW_CHARS {
                    preview.push('…');
                }
                return Some(preview);
            }
            last_grapheme_start = preview.len();
            chars_before_last_grapheme = chars;
            preview.push_str(grapheme);
            chars += grapheme_chars;
        }
        has_text = true;
    }
    has_text.then_some(preview)
}

pub(super) fn context_description(context: Option<&AgentPickerContextUsage>) -> String {
    let Some(context) = context else {
        return "context ?".to_string();
    };
    if context.last_tokens < 0 {
        return "context ?".to_string();
    }
    let Some(window) = context.model_context_window.filter(|window| *window > 0) else {
        return format!("context {} / ?", context.last_tokens);
    };

    let percent = ((context.last_tokens as f64 / window as f64) * 100.0).round() as i64;
    format!("context {} / {} ({percent}%)", context.last_tokens, window)
}

pub(super) fn picker_status_label(
    is_closed: bool,
    is_running: bool,
    is_error: bool,
) -> &'static str {
    if is_closed {
        "closed"
    } else if is_running {
        "mid-turn"
    } else if is_error {
        "error"
    } else {
        "idle"
    }
}

#[cfg(test)]
#[path = "agent_picker_status_tests.rs"]
mod tests;
