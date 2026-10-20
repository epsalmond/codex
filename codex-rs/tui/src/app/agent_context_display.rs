//! Rendering of captured child context, without refreshing or inferring measurements.

use crate::status::format_tokens_compact;
use crate::text_formatting::truncate_text;
use codex_app_server_protocol::ThreadContextReductionOutcome;
use codex_app_server_protocol::ThreadContextTokenBasis;
use codex_app_server_protocol::ThreadContextUsage;

pub(super) fn snapshot_summary(snapshot: &ThreadContextUsage) -> String {
    let tokens = count(snapshot.active_tokens);
    let cap = match snapshot.child_policy_enabled {
        Some(/*value*/ false) => "off".to_string(),
        Some(/*value*/ true) => positive_count(snapshot.child_active_cap_tokens),
        None => "?".to_string(),
    };
    let window = positive_count(snapshot.model_window_tokens);
    format!("context {tokens} · cap {cap} · window {window}")
}

pub(super) fn snapshot_description(snapshot: &ThreadContextUsage, now: i64) -> String {
    let summary = snapshot_summary(snapshot);
    let model = snapshot
        .selected_model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .map(|model| truncate_text(model, /*max_graphemes*/ 64))
        .unwrap_or_else(|| "?".to_string());
    let basis = match snapshot.basis {
        ThreadContextTokenBasis::Usage => "usage + estimate",
        ThreadContextTokenBasis::Estimate => "estimate",
    };
    let observed = age(snapshot.observed_at, now);
    let provider = age(snapshot.provider_usage_at, now);
    let mut description =
        format!("{summary} · {model} · {basis} · snapshot {observed} · provider {provider}");
    if let Some(reduction) = &snapshot.last_reduction {
        let outcome = match reduction.outcome {
            ThreadContextReductionOutcome::Shaken => "shaken",
            ThreadContextReductionOutcome::Compacted => "compacted",
            ThreadContextReductionOutcome::Insufficient => "insufficient",
            ThreadContextReductionOutcome::Failed => "failed",
            ThreadContextReductionOutcome::Cancelled => "cancelled",
        };
        let before = count(reduction.before_tokens);
        let after = reduction
            .after_tokens
            .map(count)
            .unwrap_or_else(|| "?".to_string());
        let age = age(Some(reduction.completed_at), now);
        description.push_str(&format!(" · {outcome} {before}→{after} ({age})"));
    }
    description
}

/// Matches the status bar's compact token counts, e.g. `272K`.
fn count(tokens: i64) -> String {
    if tokens < 0 {
        "?".to_string()
    } else {
        format_tokens_compact(tokens)
    }
}

fn positive_count(tokens: Option<i64>) -> String {
    tokens
        .filter(|tokens| *tokens > 0)
        .map(count)
        .unwrap_or_else(|| "?".to_string())
}

fn age(at: Option<i64>, now: i64) -> String {
    let Some(seconds) = at
        .filter(|at| *at > 0 && *at <= now)
        .map(|at| now.saturating_sub(at))
    else {
        return "?".to_string();
    };
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

#[cfg(test)]
#[path = "agent_context_display_tests.rs"]
mod tests;
