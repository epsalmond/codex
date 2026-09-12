//! Automatic surgical context reduction ("auto-shake").
//!
//! This module is the pure decision layer: it resolves the effective
//! `[auto_shake]` settings for a model and answers "should this thread shake
//! instead of compacting?". It performs no I/O and touches no session state, so
//! every rule below is unit-testable. The orchestration lives in
//! `session::turn::maybe_run_pre_sampling_auto_shake`.

use codex_config::config_toml::AutoShakeThresholdToml;
use codex_config::config_toml::AutoShakeToml;

use crate::config::AutoShakeConfig;
use crate::config::AutoShakeModelConfig;

/// Built-in global threshold default: shake once active context reaches this
/// percent of the model's resolved context window. Well below the
/// auto-compaction limit (90% of the window) so a successful shake usually
/// removes the need to compact.
const DEFAULT_THRESHOLD_PERCENT: i64 = 60;

/// Built-in minimum elidable share. A shake that frees less than this fraction
/// of the context is not worth the guaranteed prompt-cache miss, and repeating
/// it every turn would thrash.
const DEFAULT_MIN_ELIDABLE_PERCENT: i64 = 30;

/// Built-in absolute minimum-savings floor, in tokens. A secondary gate
/// alongside `min_elidable_percent`: on a small context window a shake can
/// clear the percent threshold while still freeing a trivial number of
/// tokens, which isn't worth the guaranteed prompt-cache miss. Matches
/// oh-my-pi's default automatic preset `minSavings`.
const DEFAULT_MIN_SAVINGS_TOKENS: i64 = 4_000;

/// Per-model-family threshold defaults, applied when neither the global
/// `auto_shake.threshold` key nor an `[auto_shake.models.<family>]` entry
/// resolves the field.
///
/// `gpt-5.6` (sol/terra/luna) defers to the global default (`inherit`):
/// its long sessions accumulate large tool outputs that elide cleanly, and
/// the global default already reflects that. `gpt-6-astra` is on at 40% —
/// the benchmark shows a shake costs one uncached request and pays back
/// within ~6 requests at typical 300k contexts.
const FAMILY_DEFAULTS: &[(&str, AutoShakeThresholdToml)] = &[
    ("gpt-5.6", AutoShakeThresholdToml::Inherit),
    ("gpt-6-astra", AutoShakeThresholdToml::Percent(40)),
];

/// Effective settings for one model slug, after layering all precedence
/// levels and resolving `inherit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoShakeSettings {
    pub(crate) enabled: bool,
    pub(crate) threshold_percent: i64,
    pub(crate) min_elidable_percent: i64,
    pub(crate) min_savings_tokens: i64,
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
    Preview {
        min_elidable_percent: i64,
        min_savings_tokens: i64,
    },
}

impl AutoShakeConfig {
    pub fn from_toml(toml: Option<&AutoShakeToml>) -> Result<Self, String> {
        let Some(toml) = toml else {
            return Ok(Self::default());
        };
        if toml.threshold == Some(AutoShakeThresholdToml::Inherit) {
            return Err(
                "auto_shake.threshold cannot be \"inherit\": there is nothing for the global \
                 scope to inherit from. Use \"off\" or a percent value, or omit the key to keep \
                 the built-in default."
                    .to_string(),
            );
        }
        Ok(Self {
            threshold: toml.threshold,
            min_elidable_percent: toml.min_elidable_percent,
            min_savings_tokens: toml.min_savings_tokens,
            models: toml
                .models
                .iter()
                .map(|(family, model)| {
                    (
                        family.clone(),
                        AutoShakeModelConfig {
                            threshold: model.threshold,
                            min_elidable_percent: model.min_elidable_percent,
                        },
                    )
                })
                .collect(),
        })
    }

    /// Resolve the effective global threshold: the user's global override, or
    /// the built-in global default. A global `inherit` is rejected in
    /// `from_toml`, so this never has to resolve one.
    fn resolved_global_threshold(&self) -> AutoShakeThresholdToml {
        self.threshold
            .unwrap_or(AutoShakeThresholdToml::Percent(DEFAULT_THRESHOLD_PERCENT))
    }

    /// Layer threshold precedence, highest first: a family's explicit `off`
    /// or percent, the family's `inherit`/unset resolving to the global
    /// value, or (absent any user or built-in family entry) the global value
    /// directly.
    fn resolved_threshold(&self, model_slug: &str) -> AutoShakeThresholdToml {
        let global = self.resolved_global_threshold();

        let user_family = self
            .models
            .iter()
            .find(|(family, _)| model_family_matches(model_slug, family))
            .and_then(|(_, model)| model.threshold);
        if let Some(threshold) = user_family {
            return match threshold {
                AutoShakeThresholdToml::Inherit => global,
                explicit => explicit,
            };
        }

        let builtin_family = FAMILY_DEFAULTS
            .iter()
            .find(|(family, _)| model_family_matches(model_slug, family))
            .map(|(_, default)| *default);
        match builtin_family {
            Some(AutoShakeThresholdToml::Inherit) | None => global,
            Some(explicit) => explicit,
        }
    }

    /// Layer, highest precedence first: a family's explicit threshold (`off`
    /// or a percent, whether from the user or the built-in family default),
    /// then `inherit` resolving to the global value.
    pub(crate) fn settings_for_model(&self, model_slug: &str) -> AutoShakeSettings {
        let (enabled, threshold_percent) = match self.resolved_threshold(model_slug) {
            AutoShakeThresholdToml::Off => (false, DEFAULT_THRESHOLD_PERCENT),
            AutoShakeThresholdToml::Percent(percent) => (true, percent.clamp(1, 100)),
            // `resolved_threshold` never returns `Inherit`: it always resolves
            // to the global value before returning.
            AutoShakeThresholdToml::Inherit => (false, DEFAULT_THRESHOLD_PERCENT),
        };

        let user_family_min_elidable = self
            .models
            .iter()
            .find(|(family, _)| model_family_matches(model_slug, family))
            .and_then(|(_, model)| model.min_elidable_percent);

        AutoShakeSettings {
            enabled,
            threshold_percent,
            min_elidable_percent: self
                .min_elidable_percent
                .or(user_family_min_elidable)
                .unwrap_or(DEFAULT_MIN_ELIDABLE_PERCENT)
                .clamp(0, 100),
            // Global-only knob for now (mirrors omp, which does not vary
            // `minSavings` per model family either).
            min_savings_tokens: self
                .min_savings_tokens
                .unwrap_or(DEFAULT_MIN_SAVINGS_TOKENS)
                .max(0),
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
            min_savings_tokens: settings.min_savings_tokens,
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
        AutoShakeConfig::from_toml(Some(&parsed)).expect("valid auto_shake toml")
    }

    #[test]
    fn gpt_5_6_family_inherits_the_global_default_of_sixty_percent() {
        let config = AutoShakeConfig::default();
        for slug in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", LUNA] {
            let settings = config.settings_for_model(slug);
            assert!(settings.enabled, "{slug} should default to enabled");
            assert_eq!(settings.threshold_percent, 60, "{slug}");
            assert_eq!(settings.min_elidable_percent, 30, "{slug}");
        }
    }

    #[test]
    fn gpt_6_astra_is_on_at_forty_percent_by_default() {
        let settings = AutoShakeConfig::default().settings_for_model(ASTRA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 40);
        assert_eq!(settings.min_elidable_percent, 30);
    }

    #[test]
    fn unknown_families_inherit_the_global_default() {
        let settings = AutoShakeConfig::default().settings_for_model("some-local-model");
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 60);
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
        // A longer version number must not be captured by the `gpt-5.6` family,
        // so it falls through to the (also-enabled) global default.
        assert!(config.settings_for_model("gpt-5.61-sol").enabled);
        assert_eq!(config.settings_for_model("gpt-5.61-sol").threshold_percent, 60);
    }

    #[test]
    fn family_inherit_follows_a_changed_global_value() {
        // gpt-5.6's built-in default is `inherit`, so raising the global
        // threshold must raise gpt-5.6's effective threshold too.
        let config = toml("threshold = \"75%\"\n");
        let settings = config.settings_for_model(LUNA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 75);

        let config = toml("threshold = \"off\"\n");
        assert!(!config.settings_for_model(LUNA).enabled);
    }

    #[test]
    fn family_off_beats_global_percent() {
        let config = toml(
            r#"
threshold = 75
[models."gpt-6-astra"]
threshold = "off"
"#,
        );
        assert!(!config.settings_for_model(ASTRA).enabled);
        // The global value still applies to a family that defers.
        assert!(config.settings_for_model(LUNA).enabled);
        assert_eq!(config.settings_for_model(LUNA).threshold_percent, 75);
    }

    #[test]
    fn family_percent_beats_global_off() {
        let config = toml(
            r#"
threshold = "off"
[models."gpt-6-astra"]
threshold = 55
"#,
        );
        let settings = config.settings_for_model(ASTRA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 55);
        // gpt-5.6 still inherits the (now off) global value.
        assert!(!config.settings_for_model(LUNA).enabled);
    }

    #[test]
    fn percent_string_with_percent_sign_parses() {
        let config = toml(
            r#"
[models."gpt-6-astra"]
threshold = "45%"
"#,
        );
        let settings = config.settings_for_model(ASTRA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 45);
    }

    #[test]
    fn global_inherit_is_rejected() {
        let parsed: AutoShakeToml =
            toml::from_str("threshold = \"inherit\"\n").expect("parse auto_shake toml");
        let error = AutoShakeConfig::from_toml(Some(&parsed))
            .expect_err("global inherit must be a config error");
        assert!(error.contains("inherit"), "{error}");
    }

    #[test]
    fn per_family_min_elidable_percent_overrides_the_builtin_default() {
        let config = toml(
            r#"
[models."gpt-6-astra"]
threshold = 80
min_elidable_percent = 10
"#,
        );
        let settings = config.settings_for_model(ASTRA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 80);
        assert_eq!(settings.min_elidable_percent, 10);

        // The gpt-5.6 default is untouched by an astra-only entry.
        assert!(config.settings_for_model(LUNA).enabled);
        assert_eq!(config.settings_for_model(LUNA).threshold_percent, 60);
    }

    #[test]
    fn global_min_elidable_percent_wins_over_family_entry() {
        let config = toml(
            r#"
min_elidable_percent = 5
[models."gpt-6-astra"]
min_elidable_percent = 90
"#,
        );
        assert_eq!(config.settings_for_model(ASTRA).min_elidable_percent, 5);
    }

    #[test]
    fn out_of_range_percents_are_clamped() {
        let config = toml("threshold = 400\nmin_elidable_percent = -5\n");
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
                min_elidable_percent: 30,
                min_savings_tokens: 4_000,
            }
        );
    }

    #[test]
    fn decide_skips_disabled_models_before_anything_else() {
        // LUNA's built-in family default is `inherit`, so a global `"off"`
        // disables it even with active context far past any threshold.
        let config = toml("threshold = \"off\"\n");
        assert_eq!(
            config.decide(LUNA, 900_000, Some(100_000), true),
            AutoShakeDecision::Skip(AutoShakeSkip::Disabled)
        );

        // An explicit per-family `"off"` disables a family regardless of the
        // global value.
        let config = toml(
            r#"
threshold = 75
[models."gpt-6-astra"]
threshold = "off"
"#,
        );
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
