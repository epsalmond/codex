//! Automatic surgical context reduction ("auto-shake").
//!
//! This module is the pure decision layer: it resolves the effective
//! `[auto_shake]` settings for a model and answers "should this thread shake
//! instead of compacting?". It performs no I/O and touches no session state, so
//! every rule below is unit-testable. The orchestration lives in
//! `session::turn::maybe_run_pre_sampling_auto_shake`.

use codex_config::config_toml::AutoShakeToml;

use crate::config::AutoShakeConfig;
use crate::config::AutoShakeModelConfig;

/// Built-in global fallback: auto-shake stays off for model families we have not
/// explicitly opted in, so a new or custom model never silently rewrites
/// history.
const DEFAULT_ENABLED: bool = false;

/// Built-in threshold: shake once active context reaches this percent of the
/// model's resolved context window. Well below the auto-compaction limit (90%
/// of the window) so a successful shake usually removes the need to compact.
const DEFAULT_THRESHOLD_PERCENT: i64 = 60;

/// Built-in minimum elidable share. A shake that frees less than this fraction
/// of the context is not worth the guaranteed prompt-cache miss, and repeating
/// it every turn would thrash.
const DEFAULT_MIN_ELIDABLE_PERCENT: i64 = 30;

/// Per-model-family defaults, applied when neither the global `[auto_shake]`
/// keys nor an `[auto_shake.models.<family>]` entry set the field.
///
/// `gpt-5.6` (sol/terra/luna) is on because its long sessions accumulate large
/// tool outputs that elide cleanly. `gpt-6-astra` is off: it is explicitly
/// listed rather than left to the global default so the intent is visible.
const FAMILY_DEFAULTS: &[(&str, AutoShakeFamilyDefault)] = &[
    (
        "gpt-5.6",
        AutoShakeFamilyDefault {
            enabled: Some(true),
            threshold_percent: Some(60),
            min_elidable_percent: None,
        },
    ),
    (
        "gpt-6-astra",
        AutoShakeFamilyDefault {
            enabled: Some(false),
            threshold_percent: None,
            min_elidable_percent: None,
        },
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AutoShakeFamilyDefault {
    enabled: Option<bool>,
    threshold_percent: Option<i64>,
    min_elidable_percent: Option<i64>,
}

/// Effective settings for one model slug, after layering all four precedence
/// levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoShakeSettings {
    pub(crate) enabled: bool,
    pub(crate) threshold_percent: i64,
    pub(crate) min_elidable_percent: i64,
}

/// Why auto-shake did not run. Recorded in the trace log so benchmark runs can
/// tell "disabled" from "not enough to elide".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoShakeSkip {
    /// Disabled for this model family (or globally).
    Disabled,
    /// The model has no resolved context window, so there is no threshold.
    NoContextWindow,
    /// Active context has not reached the threshold yet.
    BelowThreshold,
    /// Elide mode writes recovery artifacts, which an ephemeral thread cannot
    /// keep; without them the removed text would be unrecoverable.
    EphemeralThread,
}

impl AutoShakeSkip {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoContextWindow => "no_context_window",
            Self::BelowThreshold => "below_threshold",
            Self::EphemeralThread => "ephemeral_thread",
        }
    }
}

/// Outcome of the threshold check. `Preview` only authorizes measuring: the
/// shake still has to clear `min_elidable_percent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoShakeDecision {
    Skip(AutoShakeSkip),
    Preview { min_elidable_percent: i64 },
}

impl AutoShakeConfig {
    pub fn from_toml(toml: Option<&AutoShakeToml>) -> Self {
        let Some(toml) = toml else {
            return Self::default();
        };
        Self {
            enabled: toml.enabled,
            threshold_percent: toml.threshold_percent,
            min_elidable_percent: toml.min_elidable_percent,
            models: toml
                .models
                .iter()
                .map(|(family, model)| {
                    (
                        family.clone(),
                        AutoShakeModelConfig {
                            enabled: model.enabled,
                            threshold_percent: model.threshold_percent,
                            min_elidable_percent: model.min_elidable_percent,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Layer, highest precedence first: global override, user per-family entry,
    /// built-in family default, built-in global default.
    pub(crate) fn settings_for_model(&self, model_slug: &str) -> AutoShakeSettings {
        let user_family = self
            .models
            .iter()
            .find(|(family, _)| model_family_matches(model_slug, family))
            .map(|(_, model)| *model)
            .unwrap_or_default();
        let builtin_family = FAMILY_DEFAULTS
            .iter()
            .find(|(family, _)| model_family_matches(model_slug, family))
            .map(|(_, default)| *default)
            .unwrap_or(AutoShakeFamilyDefault {
                enabled: None,
                threshold_percent: None,
                min_elidable_percent: None,
            });

        AutoShakeSettings {
            enabled: self
                .enabled
                .or(user_family.enabled)
                .or(builtin_family.enabled)
                .unwrap_or(DEFAULT_ENABLED),
            threshold_percent: self
                .threshold_percent
                .or(user_family.threshold_percent)
                .or(builtin_family.threshold_percent)
                .unwrap_or(DEFAULT_THRESHOLD_PERCENT)
                .clamp(1, 100),
            min_elidable_percent: self
                .min_elidable_percent
                .or(user_family.min_elidable_percent)
                .or(builtin_family.min_elidable_percent)
                .unwrap_or(DEFAULT_MIN_ELIDABLE_PERCENT)
                .clamp(0, 100),
        }
    }

    /// Decide whether this thread should measure a shake before compacting.
    ///
    /// `context_window` is the model's *resolved* window (not the
    /// effective/usable slice) so the threshold means what the config key says.
    pub(crate) fn decide(
        &self,
        model_slug: &str,
        active_context_tokens: i64,
        context_window: Option<i64>,
        persistent_thread: bool,
    ) -> AutoShakeDecision {
        let settings = self.settings_for_model(model_slug);
        if !settings.enabled {
            return AutoShakeDecision::Skip(AutoShakeSkip::Disabled);
        }
        // Elide is the only mode that writes recovery artifacts, and artifacts
        // require a persistent thread. Refuse rather than silently discarding.
        if !persistent_thread {
            return AutoShakeDecision::Skip(AutoShakeSkip::EphemeralThread);
        }
        let Some(context_window) = context_window.filter(|window| *window > 0) else {
            return AutoShakeDecision::Skip(AutoShakeSkip::NoContextWindow);
        };
        let threshold = context_window.saturating_mul(settings.threshold_percent) / 100;
        if active_context_tokens < threshold {
            return AutoShakeDecision::Skip(AutoShakeSkip::BelowThreshold);
        }
        AutoShakeDecision::Preview {
            min_elidable_percent: settings.min_elidable_percent,
        }
    }
}

/// Share of the measured context the preview says a shake would free, as a
/// percent. Returns 0 when there is nothing to measure.
pub(crate) fn elidable_percent(tokens_before: i64, tokens_after: i64) -> i64 {
    if tokens_before <= 0 {
        return 0;
    }
    let freed = tokens_before.saturating_sub(tokens_after).max(0);
    freed.saturating_mul(100) / tokens_before
}

/// True when a family key selects this model slug.
///
/// Provider and region qualifiers (`us.openai.gpt-5.6-sol`,
/// `global.openai.gpt-5.6-sol`, `bedrock/…`) are stripped first. A family then
/// matches the bare slug exactly, or a slug that extends it with a `-` suffix,
/// so `gpt-5.6` selects `gpt-5.6-sol` but not `gpt-5.61-sol`.
fn model_family_matches(model_slug: &str, family: &str) -> bool {
    let bare = bare_model_slug(model_slug);
    bare == family
        || bare
            .strip_prefix(family)
            .is_some_and(|rest| rest.starts_with('-'))
}

fn bare_model_slug(model_slug: &str) -> &str {
    let slug = model_slug.rsplit('/').next().unwrap_or(model_slug);
    // Slugs themselves contain dots (`gpt-5.6`), so only strip the known
    // provider qualifier rather than splitting on '.'.
    match slug.rfind("openai.") {
        Some(index) => &slug[index + "openai.".len()..],
        None => slug,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LUNA: &str = "gpt-5.6-luna";
    const ASTRA: &str = "gpt-6-astra";

    fn toml(value: &str) -> AutoShakeConfig {
        let parsed: AutoShakeToml = toml::from_str(value).expect("parse auto_shake toml");
        AutoShakeConfig::from_toml(Some(&parsed))
    }

    #[test]
    fn gpt_5_6_family_is_on_at_sixty_percent_by_default() {
        let config = AutoShakeConfig::default();
        for slug in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", LUNA] {
            let settings = config.settings_for_model(slug);
            assert!(settings.enabled, "{slug} should default to enabled");
            assert_eq!(settings.threshold_percent, 60, "{slug}");
            assert_eq!(settings.min_elidable_percent, 30, "{slug}");
        }
    }

    #[test]
    fn gpt_6_astra_is_off_by_default() {
        let settings = AutoShakeConfig::default().settings_for_model(ASTRA);
        assert!(!settings.enabled);
    }

    #[test]
    fn unknown_families_are_off_by_default() {
        assert!(
            !AutoShakeConfig::default()
                .settings_for_model("some-local-model")
                .enabled
        );
    }

    #[test]
    fn provider_and_region_qualifiers_are_stripped() {
        let config = AutoShakeConfig::default();
        for slug in [
            "us.openai.gpt-5.6-sol",
            "global.openai.gpt-5.6-sol",
            "bedrock/us.openai.gpt-5.6-sol",
        ] {
            assert!(config.settings_for_model(slug).enabled, "{slug}");
        }
        // A longer version number must not be captured by the `gpt-5.6` family.
        assert!(!config.settings_for_model("gpt-5.61-sol").enabled);
    }

    #[test]
    fn global_override_wins_over_per_model_entry() {
        // Global `enabled = false` must disable a family the user turned on.
        let config = toml(
            r#"
enabled = false
[models."gpt-5.6"]
enabled = true
"#,
        );
        assert!(!config.settings_for_model(LUNA).enabled);

        // And global `enabled = true` must enable a family turned off both by
        // the built-in default and by an explicit per-model entry.
        let config = toml(
            r#"
enabled = true
threshold_percent = 75
min_elidable_percent = 10
[models."gpt-6-astra"]
enabled = false
threshold_percent = 20
min_elidable_percent = 90
"#,
        );
        let settings = config.settings_for_model(ASTRA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 75);
        assert_eq!(settings.min_elidable_percent, 10);
    }

    #[test]
    fn per_model_entry_overrides_builtin_family_default() {
        let config = toml(
            r#"
[models."gpt-6-astra"]
enabled = true
threshold_percent = 80
"#,
        );
        let settings = config.settings_for_model(ASTRA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 80);
        // Unset field still falls through to the built-in global default.
        assert_eq!(settings.min_elidable_percent, 30);

        // The gpt-5.6 default is untouched by an astra-only entry.
        assert!(config.settings_for_model(LUNA).enabled);
        assert_eq!(config.settings_for_model(LUNA).threshold_percent, 60);
    }

    #[test]
    fn out_of_range_percents_are_clamped() {
        let config = toml("threshold_percent = 400\nmin_elidable_percent = -5\n");
        let settings = config.settings_for_model(LUNA);
        assert_eq!(settings.threshold_percent, 100);
        assert_eq!(settings.min_elidable_percent, 0);
    }

    #[test]
    fn threshold_compares_against_the_resolved_context_window() {
        let config = AutoShakeConfig::default();
        // 60% of 100_000 is 60_000.
        assert_eq!(
            config.decide(LUNA, 59_999, Some(100_000), true),
            AutoShakeDecision::Skip(AutoShakeSkip::BelowThreshold)
        );
        assert_eq!(
            config.decide(LUNA, 60_000, Some(100_000), true),
            AutoShakeDecision::Preview {
                min_elidable_percent: 30
            }
        );
    }

    #[test]
    fn decide_skips_disabled_models_before_anything_else() {
        let config = AutoShakeConfig::default();
        assert_eq!(
            config.decide(ASTRA, 900_000, Some(100_000), true),
            AutoShakeDecision::Skip(AutoShakeSkip::Disabled)
        );
    }

    #[test]
    fn decide_skips_ephemeral_threads() {
        assert_eq!(
            AutoShakeConfig::default().decide(LUNA, 90_000, Some(100_000), false),
            AutoShakeDecision::Skip(AutoShakeSkip::EphemeralThread)
        );
    }

    #[test]
    fn decide_skips_models_without_a_context_window() {
        let config = AutoShakeConfig::default();
        assert_eq!(
            config.decide(LUNA, 90_000, None, true),
            AutoShakeDecision::Skip(AutoShakeSkip::NoContextWindow)
        );
        assert_eq!(
            config.decide(LUNA, 90_000, Some(0), true),
            AutoShakeDecision::Skip(AutoShakeSkip::NoContextWindow)
        );
    }

    #[test]
    fn min_elidable_share_gates_the_shake() {
        // 30% is the default minimum: exactly 30% passes, 29% does not.
        assert_eq!(elidable_percent(1_000, 700), 30);
        assert_eq!(elidable_percent(1_000, 710), 29);
        assert_eq!(elidable_percent(1_000, 1_000), 0);
        // A preview that somehow grows the context reports zero, never negative.
        assert_eq!(elidable_percent(1_000, 1_200), 0);
        assert_eq!(elidable_percent(0, 0), 0);
    }
}
