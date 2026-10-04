//! Explicit v2 projection of captured context data; conversion never refreshes clocks.

use crate::JsonSchema;
use crate::TS;
use codex_protocol::context_usage as core;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadContextTokenBasis {
    Usage,
    Estimate,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadContextReductionOutcome {
    Shaken,
    Compacted,
    Insufficient,
    Failed,
    Cancelled,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadContextReduction {
    #[ts(type = "number")]
    pub completed_at: i64,
    #[ts(type = "number")]
    pub before_tokens: i64,
    #[ts(type = "number | null")]
    pub after_tokens: Option<i64>,
    pub outcome: ThreadContextReductionOutcome,
}

/// Context and policy of a captured prepared request, separate from cumulative usage.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ThreadContextUsage {
    #[ts(type = "number")]
    pub active_tokens: i64,
    pub basis: ThreadContextTokenBasis,
    pub last_reduction: Option<ThreadContextReduction>,
    pub selected_model: Option<String>,
    pub child_policy_enabled: Option<bool>,
    #[ts(type = "number | null")]
    pub child_active_cap_tokens: Option<i64>,
    #[ts(type = "number | null")]
    pub model_window_tokens: Option<i64>,
    #[ts(type = "number | null")]
    pub observed_at: Option<i64>,
    #[ts(type = "number | null")]
    pub provider_usage_at: Option<i64>,
    /// Exclusive sealed-history-item boundary, not tokens or a percentage.
    #[ts(type = "number | null")]
    pub shake_watermark: Option<u64>,
}

impl From<core::AgentContextUsage> for ThreadContextUsage {
    fn from(value: core::AgentContextUsage) -> Self {
        Self {
            active_tokens: value.active_tokens,
            basis: match value.basis {
                core::ContextTokenBasis::Usage => ThreadContextTokenBasis::Usage,
                core::ContextTokenBasis::Estimate => ThreadContextTokenBasis::Estimate,
            },
            last_reduction: value.last_reduction.map(|record| ThreadContextReduction {
                completed_at: record.at,
                before_tokens: record.before_tokens,
                after_tokens: record.after_tokens,
                outcome: match record.outcome {
                    core::ContextReductionOutcome::Shaken => ThreadContextReductionOutcome::Shaken,
                    core::ContextReductionOutcome::Compacted => {
                        ThreadContextReductionOutcome::Compacted
                    }
                    core::ContextReductionOutcome::Insufficient => {
                        ThreadContextReductionOutcome::Insufficient
                    }
                    core::ContextReductionOutcome::Failed => ThreadContextReductionOutcome::Failed,
                    core::ContextReductionOutcome::Cancelled => {
                        ThreadContextReductionOutcome::Cancelled
                    }
                },
            }),
            selected_model: value.selected_model,
            child_policy_enabled: value.child_policy_enabled,
            child_active_cap_tokens: value.child_active_cap_tokens,
            model_window_tokens: value.model_window_tokens,
            observed_at: value.observed_at,
            provider_usage_at: value.provider_usage_at,
            shake_watermark: value.shake_watermark,
        }
    }
}
