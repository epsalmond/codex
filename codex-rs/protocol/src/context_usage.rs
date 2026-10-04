//! Context observations, independent from cumulative provider token counters.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[cfg(test)]
#[path = "context_usage_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ContextReductionOutcome {
    Shaken,
    Compacted,
    Insufficient,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
pub struct ContextReductionRecord {
    /// Unix seconds when the reduction ended.
    pub at: i64,
    pub before_tokens: i64,
    pub after_tokens: Option<i64>,
    pub outcome: ContextReductionOutcome,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ContextTokenBasis {
    /// Provider measurement plus estimated newer context.
    Usage,
    /// A local estimate without a current provider measurement.
    #[default]
    Estimate,
}

/// A captured observation. Missing times or policy fields remain unavailable on replay.
/// The legacy and tool wire format deliberately retains snake_case field names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
pub struct AgentContextUsage {
    pub active_tokens: i64,
    pub basis: ContextTokenBasis,
    pub last_reduction: Option<ContextReductionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 128))]
    pub selected_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_policy_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_active_cap_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Usable model window after its reserved context margin.
    pub model_window_tokens: Option<i64>,
    /// Unix seconds when this observation was captured, not when it was replayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
    /// Unix seconds of the last actual provider measurement, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_usage_at: Option<i64>,
    /// Exclusive sealed-history-item boundary; never a token count or fraction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shake_watermark: Option<u64>,
    /// Local request estimate paired with active_tokens, retained only for safe replay.
    /// Inspector and v2 projections omit this accounting baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_request_tokens: Option<i64>,
}
