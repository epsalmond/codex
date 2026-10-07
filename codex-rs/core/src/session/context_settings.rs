//! Retained pre-clamp context inputs for child startup and reload.

use crate::config::Config;
use codex_config::config_toml::AutoShakeThresholdToml;
use codex_protocol::context_settings::ContextSettingsState;
use codex_protocol::context_settings::InheritedContextBaseline;
use codex_protocol::context_settings::ShakeThreshold;
use codex_protocol::protocol::SessionSource;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextSettingsTarget {
    Root,
    Child,
}

impl From<&SessionSource> for ContextSettingsTarget {
    fn from(source: &SessionSource) -> Self {
        if matches!(source, SessionSource::SubAgent(_)) {
            Self::Child
        } else {
            Self::Root
        }
    }
}

/// Project choices onto the existing config consumers, before resolving model metadata.
/// Always start from retained inputs, never from the previously clamped effective limit.
pub(crate) fn project(
    config: &mut Config,
    state: &ContextSettingsState,
    target: ContextSettingsTarget,
) {
    if let Some(basis) = &state.inherited {
        config.model_auto_compact_token_limit = basis.compaction_token_limit;
        config.auto_shake.threshold = basis.shake_threshold.map(|threshold| match threshold {
            ShakeThreshold::Off => AutoShakeThresholdToml::Off,
            ShakeThreshold::Percent { percent } => AutoShakeThresholdToml::Percent(percent),
            ShakeThreshold::Tokens { tokens } => AutoShakeThresholdToml::Tokens(tokens),
        });
        config.auto_shake.max_threshold_tokens = basis.shake_max_threshold_tokens;
        config.auto_shake.cold_resume = basis.shake_cold_resume;
        config.auto_shake.min_elidable_percent = basis.shake_min_elidable_percent;
        config.auto_shake.min_savings_tokens = basis.shake_min_savings_tokens;
        config.model_auto_compact_token_limit_scope = basis.compaction_scope;
        config.model_post_turn_compact_threshold_percent = basis.post_turn_compaction_percent;
        config.subagent_context_reduction.enabled = basis.child_reduction_enabled;
        config.subagent_context_reduction.threshold_tokens =
            basis.child_reduction_threshold_tokens as u64;
    }
    if target == ContextSettingsTarget::Child && config.subagent_context_reduction.enabled {
        let cap =
            i64::try_from(config.subagent_context_reduction.threshold_tokens).unwrap_or(i64::MAX);
        config.model_auto_compact_token_limit = Some(
            config
                .model_auto_compact_token_limit
                .map_or(cap, |limit| limit.min(cap)),
        );
        config.auto_shake.max_threshold_tokens = Some(
            config
                .auto_shake
                .max_threshold_tokens
                .map_or(cap, |limit| limit.min(cap)),
        );
    }
    config.context_settings = Some(state.clone());
}

pub(super) fn configured_baseline(config: &Config) -> InheritedContextBaseline {
    InheritedContextBaseline {
        compaction_token_limit: config.model_auto_compact_token_limit,
        shake_threshold: config
            .auto_shake
            .threshold
            .map(|threshold| match threshold {
                AutoShakeThresholdToml::Off | AutoShakeThresholdToml::Inherit => {
                    ShakeThreshold::Off
                }
                AutoShakeThresholdToml::Percent(percent) => ShakeThreshold::Percent { percent },
                AutoShakeThresholdToml::Tokens(tokens) => ShakeThreshold::Tokens { tokens },
            }),
        shake_max_threshold_tokens: config.auto_shake.max_threshold_tokens,
        shake_cold_resume: config.auto_shake.cold_resume,
        shake_min_elidable_percent: config.auto_shake.min_elidable_percent,
        shake_min_savings_tokens: config.auto_shake.min_savings_tokens,
        compaction_scope: config.model_auto_compact_token_limit_scope,
        post_turn_compaction_percent: config.model_post_turn_compact_threshold_percent,
        child_reduction_enabled: config.subagent_context_reduction.enabled,
        child_reduction_threshold_tokens: i64::try_from(
            config.subagent_context_reduction.threshold_tokens,
        )
        .unwrap_or(i64::MAX),
    }
}

/// Inherit selected inputs rather than effective model/window-dependent token counts.
pub(crate) fn inherit(config: &mut Config, state: &ContextSettingsState) {
    let basis = state
        .inherited
        .clone()
        .unwrap_or_else(|| configured_baseline(config));
    let child = ContextSettingsState {
        inherited: Some(basis),
    };
    project(config, &child, ContextSettingsTarget::Child);
    config.context_settings_from_spawn = true;
}

#[cfg(test)]
#[path = "context_settings_tests.rs"]
mod tests;
