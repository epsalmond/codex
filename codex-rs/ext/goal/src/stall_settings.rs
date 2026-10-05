//! One validated configuration path shared by runtime and replay.

use codex_core::config::GoalsToml;
use serde::Deserialize;

use crate::stall::normalized;

const PACKAGED_DEFAULTS: &str = include_str!("../config/continuation-defaults.v1.json");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagedDefaults {
    version: u32,
    goals: GoalsToml,
}

pub(crate) fn waiting_prefixes(replacement: Option<&[String]>) -> Result<Vec<String>, String> {
    parse_waiting_prefixes(PACKAGED_DEFAULTS, replacement)
}

pub(crate) fn parse_waiting_prefixes(
    packaged: &str,
    replacement: Option<&[String]>,
) -> Result<Vec<String>, String> {
    let goals = if let Some(replacement) = replacement {
        // Apply the same bounded deserializer as user configuration, including
        // manually constructed extension settings that bypass the TOML loader.
        serde_json::from_value::<GoalsToml>(serde_json::json!({
            "stall_waiting_prefixes": replacement,
        }))
        .map_err(|err| err.to_string())?
    } else {
        let defaults: PackagedDefaults =
            serde_json::from_str(packaged).map_err(|err| err.to_string())?;
        if defaults.version != 1 {
            return Err("unsupported packaged goal continuation defaults version".to_owned());
        }
        defaults.goals
    };
    let prefixes = goals.stall_waiting_prefixes.ok_or_else(|| {
        "packaged goal continuation defaults are missing waiting prefixes".to_owned()
    })?;
    Ok(prefixes.iter().map(|text| normalized(text)).collect())
}
