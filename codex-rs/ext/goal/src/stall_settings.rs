//! One validated, normalized configuration snapshot shared by runtime and replay.

use crate::stall::normalized;
use codex_core::config::GoalsToml;
use serde::Deserialize;
use std::num::NonZeroU32;

const PACKAGED_DEFAULTS: &str = include_str!("../config/continuation-defaults.v1.json");

#[derive(Clone, Debug)]
pub(crate) struct StallSettings {
    pub(crate) profile_version: u32,
    pub(crate) prefixes: Vec<String>,
    pub(crate) waiting_text_max_chars: NonZeroU32,
    pub(crate) unlinked_timer_recognition: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagedDefaults {
    version: u32,
    goals: GoalsToml,
}

pub(crate) fn settings(overrides: &GoalsToml) -> Result<StallSettings, String> {
    parse_settings(PACKAGED_DEFAULTS, overrides)
}

pub(crate) fn parse_settings(
    packaged: &str,
    overrides: &GoalsToml,
) -> Result<StallSettings, String> {
    // Revalidate manually constructed extension/replay settings with the user
    // configuration deserializer, rather than duplicating its bounds.
    let overrides: GoalsToml =
        serde_json::from_value(serde_json::to_value(overrides).map_err(|err| err.to_string())?)
            .map_err(|err| err.to_string())?;
    let defaults: PackagedDefaults =
        serde_json::from_str(packaged).map_err(|err| err.to_string())?;
    if defaults.version != 1 {
        return Err("unsupported packaged goal continuation defaults version".to_owned());
    }
    let default_prefixes = defaults
        .goals
        .stall_waiting_prefixes
        .ok_or_else(|| "goal continuation defaults are missing waiting prefixes".to_owned())?;
    let default_limit = defaults
        .goals
        .stall_waiting_text_max_chars
        .ok_or_else(|| "goal continuation defaults are missing waiting text limit".to_owned())?;
    let default_timer_policy =
        defaults
            .goals
            .stall_unlinked_timer_recognition
            .ok_or_else(|| {
                "goal continuation defaults are missing timer recognition policy".to_owned()
            })?;
    let prefixes = overrides.stall_waiting_prefixes.unwrap_or(default_prefixes);
    let waiting_text_max_chars = overrides
        .stall_waiting_text_max_chars
        .unwrap_or(default_limit);
    let unlinked_timer_recognition = overrides
        .stall_unlinked_timer_recognition
        .unwrap_or(default_timer_policy);
    Ok(StallSettings {
        profile_version: defaults.version,
        prefixes: prefixes.iter().map(|text| normalized(text)).collect(),
        waiting_text_max_chars,
        unlinked_timer_recognition,
    })
}

impl Default for StallSettings {
    fn default() -> Self {
        settings(&GoalsToml::default()).unwrap_or_else(|error| {
            // A malformed embedded build artifact is not evidence of a stall.
            // Runtime construction additionally disables the whole detector.
            tracing::warn!(%error, "goal stall profile unavailable");
            Self {
                profile_version: 0,
                prefixes: Vec::new(),
                waiting_text_max_chars: NonZeroU32::MIN,
                unlinked_timer_recognition: false,
            }
        })
    }
}
