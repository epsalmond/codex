//! Measurements for a non-mutating surgical context reduction preview.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ShakePreview {
    /// Opaque identifier of the history and mode measured by this preview.
    pub fingerprint: String,
    /// Approximate conversation tokens, excluding tool schemas and base instructions.
    #[ts(type = "number")]
    pub tokens_before: i64,
    #[ts(type = "number")]
    pub tokens_after: i64,
    pub tool_outputs: u32,
    pub text_blocks: u32,
    pub images: u32,
    pub thinking_blocks: u32,
    /// Present when this mode cannot be applied to the current thread.
    pub unavailable_reason: Option<String>,
}
