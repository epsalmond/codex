//! Sparse automatic context choices. These values do not carry usage observations.

use crate::config_types::AutoCompactTokenLimitScope;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type", rename_all = "camelCase")]
pub enum ShakeThreshold {
    Off,
    Percent { percent: i64 },
    Tokens { tokens: i64 },
}

/// Inputs before the inherited child clamp, retained across nesting and reload.
/// This is configuration, not an ancestor-imposed security ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct InheritedContextBaseline {
    pub compaction_token_limit: Option<i64>,
    /// Raw global threshold; omission still follows configured family defaults.
    #[serde(default)]
    pub shake_threshold: Option<ShakeThreshold>,
    pub shake_max_threshold_tokens: Option<i64>,
    pub shake_cold_resume: Option<bool>,
    pub shake_min_elidable_percent: Option<i64>,
    pub shake_min_savings_tokens: Option<i64>,
    pub compaction_scope: AutoCompactTokenLimitScope,
    pub post_turn_compaction_percent: u8,
    pub child_reduction_enabled: bool,
    pub child_reduction_threshold_tokens: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(default)]
pub struct ContextSettingsState {
    pub inherited: Option<InheritedContextBaseline>,
}
