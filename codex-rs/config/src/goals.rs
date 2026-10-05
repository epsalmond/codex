//! Typed goal configuration; actual matching is owned by the goal extension.

use std::num::NonZeroU32;
use std::num::NonZeroU64;

use schemars::JsonSchema;
use schemars::r#gen::SchemaGenerator;
use schemars::schema::ArrayValidation;
use schemars::schema::InstanceType;
use schemars::schema::Schema;
use schemars::schema::SchemaObject;
use schemars::schema::SingleOrVec;
use schemars::schema::StringValidation;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error;

const MAX_WAITING_PREFIXES: u32 = 32;
const MAX_WAITING_PREFIX_CHARS: u32 = 64;

/// Policy for automatic goal turns suspected of making no progress.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalContinuationGuardMode {
    /// Disable stall assessment while retaining the legacy empty-response guard.
    Off,
    /// Observe suspected stalls without holding continuation; retain the legacy empty-response guard.
    Observe,
    /// Hold automatic continuation on suspected stalls while keeping the goal Active.
    #[default]
    Defer,
}

/// Goal settings loaded when a session starts or resumes, without hot reload.
/// In off/observe, startup releases only a suspected-stall hold, preserving fork holds.
/// In defer, real user input, explicit activation, or accepted new results recover a hold.
/// See `ext/goal/templates/goals/continuation.md` for runtime recovery guidance.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GoalsToml {
    /// Maximum token budget allowed for a goal and default budget for new goals.
    pub max_goal_token_budget: Option<NonZeroU64>,
    /// Stall policy, default defer. Off and observe retain the legacy empty-response guard.
    /// Changes take effect only when a session starts or resumes.
    pub continuation_guard_mode: Option<GoalContinuationGuardMode>,
    /// Consecutive suspicious automatic turns before a hold, default 2; range 1..=4294967295.
    /// Repeated actions first establish a baseline, then count repetitions.
    #[schemars(range(min = 1, max = 4294967295u32))]
    pub stall_after_no_progress_turns: Option<NonZeroU32>,
    /// Replacement assistant final or commentary waiting prefixes: at most 32 literals, each at most
    /// 64 Unicode characters as written and nonempty after whitespace normalization.
    /// Unset preserves defaults; an empty list disables only this prefix rule.
    /// Matching lowercases, collapses whitespace, and normalizes curly apostrophes
    /// using the goal extension's shared matcher; normalized message text is limited to 512 characters.
    #[serde(default, deserialize_with = "deserialize_waiting_prefixes")]
    #[schemars(schema_with = "waiting_prefixes_schema")]
    pub stall_waiting_prefixes: Option<Vec<String>>,
}

fn deserialize_waiting_prefixes<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Option::<Vec<String>>::deserialize(deserializer)?;
    if let Some(values) = &values {
        if values.len() > MAX_WAITING_PREFIXES as usize {
            return Err(D::Error::custom(
                "goals.stall_waiting_prefixes accepts at most 32 entries",
            ));
        }
        for (index, value) in values.iter().enumerate() {
            if value.split_whitespace().next().is_none() {
                return Err(D::Error::custom(format!(
                    "goals.stall_waiting_prefixes[{index}] must contain non-whitespace text"
                )));
            }
            if value.chars().count() > MAX_WAITING_PREFIX_CHARS as usize {
                return Err(D::Error::custom(format!(
                    "goals.stall_waiting_prefixes[{index}] exceeds 64 Unicode characters"
                )));
            }
        }
    }
    Ok(values)
}

fn waiting_prefixes_schema(_generator: &mut SchemaGenerator) -> Schema {
    let prefix = SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        string: Some(Box::new(StringValidation {
            min_length: Some(1),
            max_length: Some(MAX_WAITING_PREFIX_CHARS),
            ..Default::default()
        })),
        ..Default::default()
    };
    Schema::Object(SchemaObject {
        instance_type: Some(InstanceType::Array.into()),
        array: Some(Box::new(ArrayValidation {
            items: Some(SingleOrVec::Single(Box::new(Schema::Object(prefix)))),
            max_items: Some(MAX_WAITING_PREFIXES),
            ..Default::default()
        })),
        ..Default::default()
    })
}
