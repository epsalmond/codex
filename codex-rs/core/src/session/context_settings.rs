//! Resolution and per-turn projection of retained thread context choices.

use crate::config::Config;
use crate::config::ConstraintError;
use crate::config::ConstraintResult;
use crate::shake::auto::AutoShakeSettings;
use crate::shake::auto::AutoShakeThresholdKind;
use codex_config::RequirementSource;
use codex_config::config_toml::AutoShakeThresholdToml;
use codex_protocol::context_settings::CompactionThreshold;
use codex_protocol::context_settings::ContextSettingsOverrides;
use codex_protocol::context_settings::ContextSettingsState;
use codex_protocol::context_settings::ContextSettingsValues;
use codex_protocol::context_settings::ContextSettingsView;
use codex_protocol::context_settings::InheritedContextBaseline;
use codex_protocol::context_settings::ShakeThreshold;
use codex_protocol::openai_models::ModelInfo;
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

pub(crate) fn merge(target: &mut ContextSettingsOverrides, patch: &ContextSettingsOverrides) {
    macro_rules! fields {
        ($($field:ident),*) => { $(if patch.$field.is_some() { target.$field = patch.$field; })* };
    }
    fields!(
        shake_threshold,
        shake_cold_resume,
        shake_min_elidable_percent,
        shake_min_savings_tokens,
        compaction_threshold,
        compaction_scope,
        post_turn_compaction_percent,
        child_reduction_enabled,
        child_reduction_threshold_tokens
    );
}

pub(super) fn validate_state(state: &ContextSettingsState) -> ConstraintResult<()> {
    validate(&state.overrides)?;
    if let Some(basis) = &state.inherited {
        validate(&basis.overrides)?;
        validate(&ContextSettingsOverrides {
            child_reduction_threshold_tokens: Some(basis.child_reduction_threshold_tokens),
            post_turn_compaction_percent: Some(basis.post_turn_compaction_percent),
            ..Default::default()
        })?;
    }
    Ok(())
}

fn validate(value: &ContextSettingsOverrides) -> ConstraintResult<()> {
    let mut checks = vec![
        (
            "shake_min_elidable_percent",
            value.shake_min_elidable_percent,
            0,
            100,
        ),
        (
            "shake_min_savings_tokens",
            value.shake_min_savings_tokens,
            0,
            i64::MAX,
        ),
        (
            "post_turn_compaction_percent",
            value.post_turn_compaction_percent.map(i64::from),
            0,
            100,
        ),
        (
            "child_reduction_threshold_tokens",
            value.child_reduction_threshold_tokens,
            1,
            i64::MAX,
        ),
    ];
    match value.shake_threshold {
        Some(ShakeThreshold::Percent { percent }) => {
            checks.push(("shake_threshold.percent", Some(percent), 1, 100))
        }
        Some(ShakeThreshold::Tokens { tokens }) => {
            checks.push(("shake_threshold.tokens", Some(tokens), 1, i64::MAX))
        }
        Some(ShakeThreshold::Off) | None => {}
    }
    if let Some(CompactionThreshold::Tokens { tokens }) = value.compaction_threshold {
        checks.push(("compaction_threshold.tokens", Some(tokens), 1, i64::MAX));
    }
    for (field_name, candidate, minimum, maximum) in checks {
        if let Some(candidate) = candidate
            && !(minimum..=maximum).contains(&candidate)
        {
            return Err(ConstraintError::InvalidValue {
                field_name,
                candidate: candidate.to_string(),
                allowed: format!("{minimum}..={maximum}"),
                requirement_source: RequirementSource::Unknown,
            });
        }
    }
    Ok(())
}

pub(crate) fn overlay_shake(
    settings: &mut AutoShakeSettings,
    overrides: &ContextSettingsOverrides,
) {
    if let Some(threshold) = overrides.shake_threshold {
        match threshold {
            ShakeThreshold::Off => settings.enabled = false,
            ShakeThreshold::Percent { percent } => {
                settings.enabled = true;
                settings.threshold = AutoShakeThresholdKind::Percent(percent);
            }
            ShakeThreshold::Tokens { tokens } => {
                settings.enabled = true;
                settings.threshold = AutoShakeThresholdKind::Tokens(tokens);
            }
        }
    }
    if let Some(value) = overrides.shake_cold_resume {
        settings.cold_resume = value;
    }
    if let Some(value) = overrides.shake_min_elidable_percent {
        settings.min_elidable_percent = value;
    }
    if let Some(value) = overrides.shake_min_savings_tokens {
        settings.min_savings_tokens = value;
    }
}

fn selected(state: &ContextSettingsState) -> ContextSettingsOverrides {
    let mut overrides = state
        .inherited
        .as_ref()
        .map(|basis| basis.overrides.clone())
        .unwrap_or_default();
    merge(&mut overrides, &state.overrides);
    overrides
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
    let overrides = selected(state);
    config.auto_shake.thread_overrides = overrides.clone();
    if let Some(threshold) = overrides.compaction_threshold {
        config.model_auto_compact_token_limit = match threshold {
            CompactionThreshold::ModelDefault => None,
            CompactionThreshold::Tokens { tokens } => Some(tokens),
        };
    }
    if let Some(scope) = overrides.compaction_scope {
        config.model_auto_compact_token_limit_scope = scope;
    }
    if let Some(percent) = overrides.post_turn_compaction_percent {
        config.model_post_turn_compact_threshold_percent = percent;
    }
    if let Some(enabled) = overrides.child_reduction_enabled {
        config.subagent_context_reduction.enabled = enabled;
    }
    if let Some(tokens) = overrides.child_reduction_threshold_tokens {
        config.subagent_context_reduction.threshold_tokens = tokens as u64;
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
        overrides: Default::default(),
    }
}

/// Inherit selected inputs rather than effective model/window-dependent token counts.
pub(crate) fn inherit(config: &mut Config, state: &ContextSettingsState) {
    let mut basis = state
        .inherited
        .clone()
        .unwrap_or_else(|| configured_baseline(config));
    basis.overrides = selected(state);
    let child = ContextSettingsState {
        overrides: ContextSettingsOverrides::default(),
        inherited: Some(basis),
    };
    project(config, &child, ContextSettingsTarget::Child);
    config.context_settings_from_spawn = true;
}

fn overlay_values(values: &mut ContextSettingsValues, overrides: &ContextSettingsOverrides) {
    if let Some(value) = overrides.shake_threshold {
        values.shake_threshold = value;
    }
    if let Some(value) = overrides.shake_cold_resume {
        values.shake_cold_resume = value;
    }
    if let Some(value) = overrides.shake_min_elidable_percent {
        values.shake_min_elidable_percent = value;
    }
    if let Some(value) = overrides.shake_min_savings_tokens {
        values.shake_min_savings_tokens = value;
    }
    if let Some(value) = overrides.compaction_threshold {
        values.compaction_threshold = value;
    }
    if let Some(value) = overrides.compaction_scope {
        values.compaction_scope = value;
    }
    if let Some(value) = overrides.post_turn_compaction_percent {
        values.post_turn_compaction_percent = value;
    }
    if let Some(value) = overrides.child_reduction_enabled {
        values.child_reduction_enabled = value;
    }
    if let Some(value) = overrides.child_reduction_threshold_tokens {
        values.child_reduction_threshold_tokens = value;
    }
}

pub(crate) fn resolve(
    config: &Config,
    state: &ContextSettingsState,
    catalog_model: &ModelInfo,
    target: ContextSettingsTarget,
) -> (ModelInfo, ContextSettingsView) {
    let mut shake_config = config.auto_shake.clone();
    let mut compaction_limit = config.model_auto_compact_token_limit;
    let mut compaction_scope = config.model_auto_compact_token_limit_scope;
    let mut post_turn_percent = config.model_post_turn_compact_threshold_percent;
    let mut child_enabled = config.subagent_context_reduction.enabled;
    let mut child_threshold =
        i64::try_from(config.subagent_context_reduction.threshold_tokens).unwrap_or(i64::MAX);
    if let Some(basis) = &state.inherited {
        compaction_limit = basis.compaction_token_limit;
        compaction_scope = basis.compaction_scope;
        post_turn_percent = basis.post_turn_compaction_percent;
        child_enabled = basis.child_reduction_enabled;
        child_threshold = basis.child_reduction_threshold_tokens;
        shake_config.threshold = basis.shake_threshold.map(|threshold| match threshold {
            ShakeThreshold::Off => AutoShakeThresholdToml::Off,
            ShakeThreshold::Percent { percent } => AutoShakeThresholdToml::Percent(percent),
            ShakeThreshold::Tokens { tokens } => AutoShakeThresholdToml::Tokens(tokens),
        });
        shake_config.max_threshold_tokens = basis.shake_max_threshold_tokens;
        shake_config.cold_resume = basis.shake_cold_resume;
        shake_config.min_elidable_percent = basis.shake_min_elidable_percent;
        shake_config.min_savings_tokens = basis.shake_min_savings_tokens;
    }
    let shake = shake_config.configured_settings_for_model(&catalog_model.slug);
    let mut baseline = ContextSettingsValues {
        shake_threshold: if !shake.enabled {
            ShakeThreshold::Off
        } else {
            match shake.threshold {
                AutoShakeThresholdKind::Percent(percent) => ShakeThreshold::Percent { percent },
                AutoShakeThresholdKind::Tokens(tokens) => ShakeThreshold::Tokens { tokens },
            }
        },
        shake_cold_resume: shake.cold_resume,
        shake_min_elidable_percent: shake.min_elidable_percent,
        shake_min_savings_tokens: shake.min_savings_tokens,
        compaction_threshold: compaction_limit
            .map_or(CompactionThreshold::ModelDefault, |tokens| {
                CompactionThreshold::Tokens { tokens }
            }),
        compaction_scope,
        post_turn_compaction_percent: post_turn_percent,
        child_reduction_enabled: child_enabled,
        child_reduction_threshold_tokens: child_threshold,
    };
    if let Some(basis) = &state.inherited {
        overlay_values(&mut baseline, &basis.overrides);
    }
    let mut effective = baseline.clone();
    overlay_values(&mut effective, &state.overrides);
    let child_cap = (target == ContextSettingsTarget::Child && effective.child_reduction_enabled)
        .then_some(effective.child_reduction_threshold_tokens);
    if let Some(cap) = child_cap {
        effective.compaction_threshold = CompactionThreshold::Tokens {
            tokens: match effective.compaction_threshold {
                CompactionThreshold::Tokens { tokens } => tokens.min(cap),
                CompactionThreshold::ModelDefault => {
                    if selected(state).compaction_threshold
                        == Some(CompactionThreshold::ModelDefault)
                    {
                        catalog_model
                            .auto_compact_token_limit()
                            .map_or(cap, |limit| limit.min(cap))
                    } else {
                        cap
                    }
                }
            },
        };
    }
    let mut model = catalog_model.clone();
    if let CompactionThreshold::Tokens { tokens } = effective.compaction_threshold {
        model.auto_compact_token_limit = Some(tokens);
    }
    let window = model.resolved_context_window().filter(|window| *window > 0);
    let shake_threshold = match effective.shake_threshold {
        ShakeThreshold::Off => None,
        ShakeThreshold::Percent { percent } => {
            window.map(|window| window.saturating_mul(percent) / 100)
        }
        ShakeThreshold::Tokens { tokens } => {
            Some(window.map_or(tokens, |window| tokens.min(window)))
        }
    };
    let shake_threshold_tokens = if effective.shake_threshold == ShakeThreshold::Off {
        None
    } else {
        [
            shake_threshold,
            shake_config.max_threshold_tokens,
            child_cap,
        ]
        .into_iter()
        .flatten()
        .min()
    };
    let compaction_scope_token_limit = match effective.compaction_scope {
        codex_protocol::config_types::AutoCompactTokenLimitScope::Total => {
            model.auto_compact_token_limit()
        }
        codex_protocol::config_types::AutoCompactTokenLimitScope::BodyAfterPrefix => {
            match effective.compaction_threshold {
                CompactionThreshold::Tokens { tokens } => Some(tokens),
                CompactionThreshold::ModelDefault => model.auto_compact_token_limit(),
            }
        }
    };
    let view = ContextSettingsView {
        requested: state.overrides.clone(),
        inherited_child_default_tokens: (target == ContextSettingsTarget::Child)
            .then_some(baseline.child_reduction_threshold_tokens),
        baseline,
        effective,
        model: model.slug.clone(),
        model_provider_id: config.model_provider_id.clone(),
        resolved_context_window: window,
        usable_context_window: model.usable_context_window(),
        shake_threshold_tokens,
        compaction_scope_token_limit,
        inherited_child_active_cap: child_cap.map(|cap| {
            model
                .usable_context_window()
                .map_or(cap, |window| cap.min(window))
        }),
    };
    (model, view)
}

impl super::session::Session {
    pub(crate) async fn context_settings_view(&self) -> ContextSettingsView {
        let configuration = self.state.lock().await.session_configuration.clone();
        let mut overrides = configuration.model_info_overrides.clone();
        overrides.auto_compact_token_limit = None;
        let model = configuration
            .step_settings
            .resolve_model_info(self.services.models_manager.as_ref(), &overrides)
            .await;
        resolve(
            &configuration.original_config_do_not_use,
            &configuration.context_settings,
            &model,
            ContextSettingsTarget::from(&configuration.session_source),
        )
        .1
    }
}

#[cfg(test)]
#[path = "context_settings_tests.rs"]
mod tests;
