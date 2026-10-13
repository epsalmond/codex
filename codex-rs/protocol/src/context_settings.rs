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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type", rename_all = "camelCase")]
pub enum CompactionThreshold {
    ModelDefault,
    Tokens { tokens: i64 },
}

/// Omission follows startup or inherited configuration. Patch merges these fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(default, deny_unknown_fields)]
pub struct ContextSettingsOverrides {
    pub shake_threshold: Option<ShakeThreshold>,
    pub shake_cold_resume: Option<bool>,
    pub shake_min_elidable_percent: Option<i64>,
    pub shake_min_savings_tokens: Option<i64>,
    pub compaction_threshold: Option<CompactionThreshold>,
    pub compaction_scope: Option<AutoCompactTokenLimitScope>,
    pub post_turn_compaction_percent: Option<u8>,
    pub child_reduction_enabled: Option<bool>,
    pub child_reduction_threshold_tokens: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextSettingsUpdate {
    Patch {
        overrides: ContextSettingsOverrides,
    },
    /// Remove this thread's choices and recover its configured/inherited baseline.
    Reset,
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
    #[serde(default)]
    pub overrides: ContextSettingsOverrides,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(default)]
pub struct ContextSettingsState {
    pub overrides: ContextSettingsOverrides,
    pub inherited: Option<InheritedContextBaseline>,
}

/// Family-resolved baseline and effective values use the same scalar shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct ContextSettingsValues {
    pub shake_threshold: ShakeThreshold,
    pub shake_cold_resume: bool,
    pub shake_min_elidable_percent: i64,
    pub shake_min_savings_tokens: i64,
    pub compaction_threshold: CompactionThreshold,
    pub compaction_scope: AutoCompactTokenLimitScope,
    pub post_turn_compaction_percent: u8,
    pub child_reduction_enabled: bool,
    pub child_reduction_threshold_tokens: i64,
}

/// Bounded policy readback. Full admission and compaction-scope limits are distinct.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct ContextSettingsView {
    pub requested: ContextSettingsOverrides,
    pub baseline: ContextSettingsValues,
    pub effective: ContextSettingsValues,
    pub model: String,
    pub model_provider_id: String,
    pub resolved_context_window: Option<i64>,
    pub usable_context_window: Option<i64>,
    pub shake_threshold_tokens: Option<i64>,
    pub compaction_scope_token_limit: Option<i64>,
    /// The inherited descendant default, before this thread's patch. None on roots.
    pub inherited_child_default_tokens: Option<i64>,
    /// None on roots: the child default only applies to descendants.
    pub inherited_child_active_cap: Option<i64>,
}

#[cfg(test)]
#[path = "context_settings_tests.rs"]
mod tests;
