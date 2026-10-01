use codex_history::ResponseItemEnvelope;
use codex_history::ShakeHistoryState;
use codex_protocol::models::ResponseItem;
use sha1::Digest;
use sha1::Sha1;
use std::collections::HashMap;
use uuid::Uuid;

const SHA1_HEX_LENGTH: usize = 40;
pub(crate) const UNSEALED_REPLAY_EPOCH_ID: &str = "unsealed-replay";

pub(crate) fn state_for_epoch(
    epoch_id: String,
    watermark: usize,
    items: &[ResponseItemEnvelope],
) -> Result<ShakeHistoryState, String> {
    if epoch_id == UNSEALED_REPLAY_EPOCH_ID && watermark != 0 {
        return Err("the unsealed replay state cannot be promoted to a watermark".to_string());
    }
    if watermark > items.len() {
        return Err("Shake watermark exceeds history length".to_string());
    }
    let persisted_watermark = u64::try_from(watermark)
        .map_err(|_| "Shake watermark does not fit in its persisted representation".to_string())?;
    Ok(ShakeHistoryState {
        epoch_id,
        watermark: persisted_watermark,
        sealed_prefix_digest: history_prefix_digest(&items[..watermark])?,
    })
}

pub(crate) fn fresh_epoch() -> ShakeHistoryState {
    ShakeHistoryState {
        epoch_id: Uuid::new_v4().to_string(),
        watermark: 0,
        sealed_prefix_digest: format!("{:x}", Sha1::digest([])),
    }
}

pub(crate) fn unsealed_replay_state() -> ShakeHistoryState {
    ShakeHistoryState {
        epoch_id: UNSEALED_REPLAY_EPOCH_ID.to_string(),
        watermark: 0,
        sealed_prefix_digest: format!("{:x}", Sha1::digest([])),
    }
}

pub(crate) fn rebase_after_media_preparation(
    state: ShakeHistoryState,
    checkpoint_was_valid: bool,
    items: &[ResponseItemEnvelope],
) -> (ShakeHistoryState, bool) {
    if checkpoint_was_valid && !matches_history(&state, items) {
        (fresh_epoch(), true)
    } else {
        (state, false)
    }
}

pub(crate) fn matches_history(state: &ShakeHistoryState, items: &[ResponseItemEnvelope]) -> bool {
    let Ok(watermark) = usize::try_from(state.watermark) else {
        return false;
    };
    watermark <= items.len()
        && (state.epoch_id != UNSEALED_REPLAY_EPOCH_ID || watermark == 0)
        && !state.epoch_id.is_empty()
        && state.epoch_id.len() <= 64
        && state.sealed_prefix_digest.len() == SHA1_HEX_LENGTH
        && state
            .sealed_prefix_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && history_prefix_digest(&items[..watermark])
            .is_ok_and(|digest| digest == state.sealed_prefix_digest)
}

pub(crate) fn canonical_json_bytes<T: serde::Serialize + ?Sized>(
    value: &T,
) -> serde_json::Result<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    serde_json::to_vec(&value)
}

pub(crate) fn history_prefix_digest(items: &[ResponseItemEnvelope]) -> Result<String, String> {
    let mut digest = Sha1::new();
    for item in items {
        // Checkpoint metadata uses a sidecar vector. If any item has metadata,
        // serialization fills missing entries with the default DTO, so treat
        // `None` and `Some(default)` as the same persisted envelope value.
        let metadata = item
            .metadata
            .clone()
            .unwrap_or_else(codex_history::CodexHarnessMetadata::default);
        let bytes =
            canonical_json_bytes(&(&item.item, metadata)).map_err(|error| error.to_string())?;
        let length = u64::try_from(bytes.len())
            .map_err(|_| "serialized response history envelope is too large".to_string())?;
        digest.update(length.to_be_bytes());
        digest.update(bytes);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Move a proposed boundary back to the earliest call/output pair it would split.
pub(crate) fn close_over_tool_calls(items: &[ResponseItemEnvelope], candidate_end: usize) -> usize {
    let candidate_end = candidate_end.min(items.len());
    let mut calls: HashMap<(ToolCallKind, &str), Vec<usize>> = HashMap::new();
    let mut outputs: HashMap<(ToolCallKind, &str), Vec<usize>> = HashMap::new();
    for (index, envelope) in items.iter().enumerate() {
        if let Some((kind, call_id)) = tool_call_identity(&envelope.item) {
            calls.entry((kind, call_id)).or_default().push(index);
        }
        if let Some((kind, call_id)) = tool_output_identity(&envelope.item) {
            outputs.entry((kind, call_id)).or_default().push(index);
        }
    }

    let mut end = candidate_end;
    for (index, envelope) in items.iter().take(candidate_end).enumerate() {
        let Some((kind, call_id)) = tool_call_identity(&envelope.item) else {
            continue;
        };
        let paired_output = outputs
            .get(&(kind, call_id))
            .and_then(|indices| indices.iter().copied().find(|output| *output > index));
        if paired_output.is_none_or(|output| output >= candidate_end) {
            end = end.min(index);
        }
    }
    end
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ToolCallKind {
    Function,
    Custom,
    ToolSearch,
}

fn tool_call_identity(item: &ResponseItem) -> Option<(ToolCallKind, &str)> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        } => Some((ToolCallKind::Function, call_id)),
        ResponseItem::CustomToolCall { call_id, .. } => Some((ToolCallKind::Custom, call_id)),
        ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        } => Some((ToolCallKind::ToolSearch, call_id)),
        _ => None,
    }
}

fn tool_output_identity(item: &ResponseItem) -> Option<(ToolCallKind, &str)> {
    match item {
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            ..
        } => Some((ToolCallKind::Function, call_id)),
        ResponseItem::CustomToolCallOutput { call_id, .. } => Some((ToolCallKind::Custom, call_id)),
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some((ToolCallKind::ToolSearch, call_id)),
        _ => None,
    }
}

#[cfg(test)]
#[path = "watermark_tests.rs"]
mod tests;
