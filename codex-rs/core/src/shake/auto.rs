//! Automatic surgical context reduction ("auto-shake").
//!
//! This module is the pure decision layer: it resolves the effective
//! `[auto_shake]` settings for a model and answers "should this thread shake
//! instead of compacting?". It performs no I/O and touches no session state, so
//! every rule below is unit-testable. The orchestration lives in
//! `session::turn::maybe_run_pre_sampling_auto_shake`.

use std::time::Duration;

use codex_config::config_toml::AutoShakeDurationToml;
use codex_config::config_toml::AutoShakeThresholdToml;
use codex_config::config_toml::AutoShakeToml;
use codex_model_provider_info::CacheStaleness;
use codex_model_provider_info::CacheTtlInputs;
use codex_model_provider_info::ResolvedCacheTtl;
use codex_model_provider_info::classify_cache_staleness;
use codex_model_provider_info::resolve_cache_ttl;

use crate::config::AutoShakeConfig;
use crate::config::AutoShakeModelConfig;
use crate::config::AutoShakeProviderConfig;

/// Built-in global threshold default: shake once active context reaches this
/// percent of the model's resolved context window. Well below the
/// auto-compaction limit (90% of the window) so a successful shake usually
/// removes the need to compact.
const DEFAULT_THRESHOLD_PERCENT: i64 = 60;

/// Built-in minimum elidable share. A shake that frees less than this fraction
/// of the context is not worth the guaranteed prompt-cache miss, and repeating
/// it every turn would thrash.
const DEFAULT_MIN_ELIDABLE_PERCENT: i64 = 30;

/// Built-in absolute minimum-savings floor, in tokens. A secondary check
/// alongside `min_elidable_percent`: on a small context window a shake can
/// clear the percent threshold while still freeing a trivial number of
/// tokens, which isn't worth the guaranteed prompt-cache miss. Matches
/// oh-my-pi's default automatic preset `minSavings`.
const DEFAULT_MIN_SAVINGS_TOKENS: i64 = 4_000;

/// `gpt-5.6`'s default absolute threshold, in tokens. gpt-5.6 models pay a
/// long-context penalty above 272k input tokens. A percent of a large
/// configured context window (some users run windows above 800k) can put the
/// first shake inside the penalty band, or after auto-compaction has already
/// fired. 160,000 is about 60% of the old 272k default window, and
/// comfortably below both the 272k penalty line and the 80% compaction point
/// of a 272k window — so an absolute count keeps the first shake at roughly
/// the same point in a session regardless of the configured window size.
const GPT_5_6_DEFAULT_THRESHOLD_TOKENS: i64 = 160_000;

/// Per-model-family threshold defaults, applied when neither the global
/// `auto_shake.threshold` key nor an `[auto_shake.models.<family>]` entry
/// resolves the field.
///
/// `gpt-5.6` (sol/terra/luna) defaults to an absolute token count rather than
/// a percent of the window (see `GPT_5_6_DEFAULT_THRESHOLD_TOKENS`).
/// `gpt-6-astra` is on at 40% — the benchmark shows a shake costs one
/// uncached request and pays back within ~6 requests at typical 300k
/// contexts.
const FAMILY_DEFAULTS: &[(&str, AutoShakeThresholdToml)] = &[
    (
        "gpt-5.6",
        AutoShakeThresholdToml::Tokens(GPT_5_6_DEFAULT_THRESHOLD_TOKENS),
    ),
    ("gpt-6-astra", AutoShakeThresholdToml::Percent(40)),
];

/// Built-in default for the cold-resume (prompt-cache-expiry) trigger.
///
/// On by default because a shake on a *cold* thread is free: shake-bench
/// (2026-09-12) measured that a shake on a warm thread costs one uncached
/// request of survivor size, while on a thread whose prompt cache has already
/// expired the full rebuild is paid on the next request either way — so the
/// shake costs nothing relative to not shaking, and every later request is
/// cheaper.
const DEFAULT_COLD_RESUME: bool = true;

/// A resolved threshold, after layering precedence and resolving `inherit`:
/// either a percent of the model's resolved context window, or an absolute
/// token count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoShakeThresholdKind {
    Percent(i64),
    Tokens(i64),
}

/// Effective settings for one model slug, after layering all precedence
/// levels and resolving `inherit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoShakeSettings {
    pub(crate) enabled: bool,
    pub(crate) threshold: AutoShakeThresholdKind,
    pub(crate) min_elidable_percent: i64,
    pub(crate) min_savings_tokens: i64,
    /// Whether the cold-resume (prompt-cache-expiry) trigger is active. Only
    /// consulted when `enabled` is true: `threshold = "off"` turns off every
    /// auto-shake trigger, this one included.
    pub(crate) cold_resume: bool,
}

/// Why auto-shake did not run. Recorded in the trace log so benchmark runs can
/// tell "disabled" from "not enough to elide".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoShakeSkip {
    /// Disabled for this model family (or globally).
    Disabled,
    /// The model has no resolved context window, so a percent threshold has
    /// nothing to compare against. An absolute (token-count) threshold does
    /// not need a context window and never produces this skip reason.
    NoContextWindow,
    /// Active context has not reached the threshold yet.
    BelowThreshold,
    /// Elide mode writes recovery artifacts, which an ephemeral thread cannot
    /// keep; without them the removed text would be unrecoverable.
    EphemeralThread,
    /// `auto_shake.cold_resume = false`: the operator turned off just this
    /// trigger, leaving the threshold triggers alone.
    ColdResumeDisabled,
    /// The thread has not been idle long enough for the provider's prompt
    /// cache to have expired, so a shake here would still cost a full uncached
    /// request.
    CacheStillWarm,
    /// A cold-resume shake was already decided for this idle window (the same
    /// last-request timestamp). Fires at most once per window.
    ColdResumeAlreadyDecided,
    /// The idle interval or provider TTL could not be resolved.
    CacheStalenessUnknown,
}

impl AutoShakeSkip {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoContextWindow => "no_context_window",
            Self::BelowThreshold => "below_threshold",
            Self::EphemeralThread => "ephemeral_thread",
            Self::ColdResumeDisabled => "cold_resume_disabled",
            Self::CacheStillWarm => "cache_still_warm",
            Self::ColdResumeAlreadyDecided => "cold_resume_already_decided",
            Self::CacheStalenessUnknown => "cache_staleness_unknown",
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
            cold_resume: toml.cold_resume,
            cache_ttl: toml.cache_ttl,
            providers: toml
                .providers
                .iter()
                .map(|(provider, settings)| {
                    (
                        provider.clone(),
                        AutoShakeProviderConfig {
                            cache_ttl: settings.cache_ttl,
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

    /// Layer, highest precedence first: a family's explicit threshold (`off`,
    /// a percent, or an absolute token count, whether from the user or the
    /// built-in family default), then `inherit` resolving to the global
    /// value.
    pub(crate) fn settings_for_model(&self, model_slug: &str) -> AutoShakeSettings {
        let (enabled, threshold) = match self.resolved_threshold(model_slug) {
            AutoShakeThresholdToml::Off => (
                false,
                AutoShakeThresholdKind::Percent(DEFAULT_THRESHOLD_PERCENT),
            ),
            AutoShakeThresholdToml::Percent(percent) => {
                (true, AutoShakeThresholdKind::Percent(percent.clamp(1, 100)))
            }
            AutoShakeThresholdToml::Tokens(tokens) => {
                (true, AutoShakeThresholdKind::Tokens(tokens.max(1)))
            }
            // `resolved_threshold` never returns `Inherit`: it always resolves
            // to the global value before returning.
            AutoShakeThresholdToml::Inherit => (
                false,
                AutoShakeThresholdKind::Percent(DEFAULT_THRESHOLD_PERCENT),
            ),
        };

        let user_family_min_elidable = self
            .models
            .iter()
            .find(|(family, _)| model_family_matches(model_slug, family))
            .and_then(|(_, model)| model.min_elidable_percent);

        AutoShakeSettings {
            enabled,
            threshold,
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
            // Global-only knob, like `min_savings_tokens`: prompt-cache
            // lifetime is a property of the provider, not of the model family.
            cold_resume: self.cold_resume.unwrap_or(DEFAULT_COLD_RESUME),
        }
    }

    /// Resolve the prompt-cache TTL using the shared provider policy.
    pub(crate) fn resolved_cache_ttl_for_provider(
        &self,
        provider_id: Option<&str>,
    ) -> ResolvedCacheTtl {
        resolve_cache_ttl(CacheTtlInputs {
            provider_id,
            provider_override: provider_id
                .and_then(|provider_id| self.providers.get(provider_id))
                .and_then(|provider| provider.cache_ttl)
                .map(auto_shake_duration),
            global_override: self.cache_ttl.map(auto_shake_duration),
        })
    }

    /// Decide whether this thread should measure a shake because its prompt
    /// cache has expired ("cold resume"), independently of how full the
    /// context is.
    ///
    /// The rationale is the one shake-bench established on 2026-09-12: a shake
    /// on a warm thread costs one uncached request of survivor size, but once
    /// the provider's prompt cache has expired that rebuild is already paid, so
    /// shaking is free relative to not shaking. The only remaining reason not
    /// to shake is that there is too little to remove, which the caller still
    /// checks via `min_elidable_percent` / `min_savings_tokens` exactly as the
    /// threshold triggers do.
    ///
    /// `idle` is the time since the last sampling request on this thread, or
    /// `None` when no request has been made yet (a brand-new session has
    /// nothing cached and nothing accumulated to shake).
    ///
    /// `already_decided` is the dedupe condition: true when a cold-resume shake
    /// was already decided against this same last-request timestamp, so the
    /// trigger fires at most once per idle window.
    pub(crate) fn decide_cold_resume(
        &self,
        model_slug: &str,
        provider_id: Option<&str>,
        persistent_thread: bool,
        idle: Option<Duration>,
        already_decided: bool,
    ) -> AutoShakeDecision {
        let settings = self.settings_for_model(model_slug);
        // `threshold = "off"` disables auto-shake wholesale, cold resume
        // included; `cold_resume = false` disables only this trigger.
        if !settings.enabled {
            return AutoShakeDecision::Skip(AutoShakeSkip::Disabled);
        }
        if !settings.cold_resume {
            return AutoShakeDecision::Skip(AutoShakeSkip::ColdResumeDisabled);
        }
        if !persistent_thread {
            return AutoShakeDecision::Skip(AutoShakeSkip::EphemeralThread);
        }
        if already_decided {
            return AutoShakeDecision::Skip(AutoShakeSkip::ColdResumeAlreadyDecided);
        }
        match classify_cache_staleness(idle, self.resolved_cache_ttl_for_provider(provider_id).ttl)
        {
            CacheStaleness::Warm => {
                return AutoShakeDecision::Skip(AutoShakeSkip::CacheStillWarm);
            }
            CacheStaleness::LikelyExpired => {}
            CacheStaleness::Unknown => {
                return AutoShakeDecision::Skip(AutoShakeSkip::CacheStalenessUnknown);
            }
        }
        AutoShakeDecision::Preview {
            min_elidable_percent: settings.min_elidable_percent,
            min_savings_tokens: settings.min_savings_tokens,
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
        let context_window = context_window.filter(|window| *window > 0);
        let threshold = match settings.threshold {
            AutoShakeThresholdKind::Percent(percent) => {
                // A percent threshold has nothing to compare against without a
                // resolved window.
                let Some(context_window) = context_window else {
                    return AutoShakeDecision::Skip(AutoShakeSkip::NoContextWindow);
                };
                context_window.saturating_mul(percent) / 100
            }
            AutoShakeThresholdKind::Tokens(tokens) => {
                // An absolute threshold works without a window. When the
                // window is known, cap the threshold at it so a misconfigured
                // absolute value above the window still fires rather than
                // silently never triggering.
                match context_window {
                    Some(context_window) => tokens.min(context_window),
                    None => tokens,
                }
            }
        };
        if active_context_tokens < threshold {
            return AutoShakeDecision::Skip(AutoShakeSkip::BelowThreshold);
        }
        AutoShakeDecision::Preview {
            min_elidable_percent: settings.min_elidable_percent,
            min_savings_tokens: settings.min_savings_tokens,
        }
    }
}

fn auto_shake_duration(value: AutoShakeDurationToml) -> Duration {
    Duration::from_secs(value.as_secs().unsigned_abs())
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
    fn gpt_5_6_family_defaults_to_an_absolute_160k_token_threshold() {
        let config = AutoShakeConfig::default();
        for slug in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", LUNA] {
            let settings = config.settings_for_model(slug);
            assert!(settings.enabled, "{slug} should default to enabled");
            assert_eq!(
                settings.threshold,
                AutoShakeThresholdKind::Tokens(160_000),
                "{slug}"
            );
            assert_eq!(settings.min_elidable_percent, 30, "{slug}");
        }
    }

    #[test]
    fn gpt_5_6_family_can_be_overridden_to_inherit_the_global_value() {
        // Explicitly opting gpt-5.6 back into `inherit` must follow a changed
        // global percent, exactly like any other family's `inherit`.
        let config = toml(
            r#"
threshold = "75%"
[models."gpt-5.6"]
threshold = "inherit"
"#,
        );
        let settings = config.settings_for_model(LUNA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Percent(75));
    }

    #[test]
    fn gpt_6_astra_is_on_at_forty_percent_by_default() {
        let settings = AutoShakeConfig::default().settings_for_model(ASTRA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Percent(40));
        assert_eq!(settings.min_elidable_percent, 30);
    }

    #[test]
    fn unknown_families_inherit_the_global_default() {
        let settings = AutoShakeConfig::default().settings_for_model("some-local-model");
        assert!(settings.enabled);
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Percent(60));
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
        let settings = config.settings_for_model("gpt-5.61-sol");
        assert!(settings.enabled);
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Percent(60));
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
        // A family with no built-in default of its own still defers to the
        // global value.
        let settings = config.settings_for_model("some-local-model");
        assert!(settings.enabled);
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Percent(75));
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
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Percent(55));
        // gpt-5.6's built-in default is now an explicit absolute threshold
        // (not `inherit`), so a global `"off"` does not disable it.
        assert!(config.settings_for_model(LUNA).enabled);
        assert_eq!(
            config.settings_for_model(LUNA).threshold,
            AutoShakeThresholdKind::Tokens(160_000)
        );
        // A family with no built-in default still inherits the (now off)
        // global value.
        assert!(!config.settings_for_model("some-local-model").enabled);
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
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Percent(45));
    }

    #[test]
    fn absolute_token_threshold_string_parses() {
        let config = toml(
            r#"
[models."gpt-6-astra"]
threshold = "160k"
"#,
        );
        let settings = config.settings_for_model(ASTRA);
        assert!(settings.enabled);
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Tokens(160_000));
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
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Percent(80));
        assert_eq!(settings.min_elidable_percent, 10);

        // The gpt-5.6 default is untouched by an astra-only entry.
        assert!(config.settings_for_model(LUNA).enabled);
        assert_eq!(
            config.settings_for_model(LUNA).threshold,
            AutoShakeThresholdKind::Tokens(160_000)
        );
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
        // A bare integer above 100 means an absolute token count (disambiguation
        // rule), so an out-of-range *percent* must use the "%" spelling.
        // "some-local-model" has no built-in family default, so it inherits
        // the (out-of-range) global percent directly.
        let config = toml("threshold = \"400%\"\nmin_elidable_percent = -5\n");
        let settings = config.settings_for_model("some-local-model");
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Percent(100));
        assert_eq!(settings.min_elidable_percent, 0);
    }

    #[test]
    fn a_bare_integer_above_one_hundred_is_an_absolute_token_count() {
        let config = toml("threshold = 400\n");
        let settings = config.settings_for_model("some-local-model");
        assert_eq!(settings.threshold, AutoShakeThresholdKind::Tokens(400));
    }

    #[test]
    fn threshold_compares_against_the_resolved_context_window() {
        let config = AutoShakeConfig::default();
        // Astra defaults to 40%; 40% of 100_000 is 40_000.
        assert_eq!(
            config.decide(ASTRA, 39_999, Some(100_000), true),
            AutoShakeDecision::Skip(AutoShakeSkip::BelowThreshold)
        );
        assert_eq!(
            config.decide(ASTRA, 40_000, Some(100_000), true),
            AutoShakeDecision::Preview {
                min_elidable_percent: 30,
                min_savings_tokens: 4_000,
            }
        );
    }

    #[test]
    fn absolute_threshold_fires_without_a_context_window() {
        let config = AutoShakeConfig::default();
        // LUNA (gpt-5.6) defaults to an absolute 160_000-token threshold,
        // which needs no resolved context window.
        assert_eq!(
            config.decide(LUNA, 159_999, None, true),
            AutoShakeDecision::Skip(AutoShakeSkip::BelowThreshold)
        );
        assert_eq!(
            config.decide(LUNA, 160_000, None, true),
            AutoShakeDecision::Preview {
                min_elidable_percent: 30,
                min_savings_tokens: 4_000,
            }
        );
    }

    #[test]
    fn absolute_threshold_is_capped_at_a_smaller_context_window() {
        // A misconfigured absolute threshold above the model's resolved
        // window must still fire at the window, not silently never trigger.
        let config = AutoShakeConfig::default();
        assert_eq!(
            config.decide(LUNA, 99_999, Some(100_000), true),
            AutoShakeDecision::Skip(AutoShakeSkip::BelowThreshold)
        );
        assert_eq!(
            config.decide(LUNA, 100_000, Some(100_000), true),
            AutoShakeDecision::Preview {
                min_elidable_percent: 30,
                min_savings_tokens: 4_000,
            }
        );
    }

    #[test]
    fn absolute_threshold_uses_its_own_value_under_a_larger_context_window() {
        // Eric's 872k window: the 160k absolute default must fire at 160k,
        // not at some percent of the much larger window.
        let config = AutoShakeConfig::default();
        assert_eq!(
            config.decide(LUNA, 159_999, Some(872_000), true),
            AutoShakeDecision::Skip(AutoShakeSkip::BelowThreshold)
        );
        assert_eq!(
            config.decide(LUNA, 160_000, Some(872_000), true),
            AutoShakeDecision::Preview {
                min_elidable_percent: 30,
                min_savings_tokens: 4_000,
            }
        );
    }

    #[test]
    fn decide_skips_disabled_models_before_anything_else() {
        // gpt-5.6's built-in default is now an explicit absolute threshold,
        // so disabling it requires an explicit per-family `"off"` rather than
        // a global one.
        let config = toml(
            r#"
[models."gpt-5.6"]
threshold = "off"
"#,
        );
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

        // A family with no built-in default of its own is still disabled by
        // a global `"off"`.
        let config = toml("threshold = \"off\"\n");
        assert_eq!(
            config.decide("some-local-model", 900_000, Some(100_000), true),
            AutoShakeDecision::Skip(AutoShakeSkip::Disabled)
        );
    }

    #[test]
    fn decide_skips_ephemeral_threads() {
        assert_eq!(
            AutoShakeConfig::default().decide(LUNA, 900_000, Some(100_000), false),
            AutoShakeDecision::Skip(AutoShakeSkip::EphemeralThread)
        );
    }

    #[test]
    fn decide_skips_percent_thresholds_without_a_context_window() {
        // ASTRA's default is a percent threshold, which needs a resolved
        // context window to compare against.
        let config = AutoShakeConfig::default();
        assert_eq!(
            config.decide(ASTRA, 90_000, None, true),
            AutoShakeDecision::Skip(AutoShakeSkip::NoContextWindow)
        );
        assert_eq!(
            config.decide(ASTRA, 90_000, Some(0), true),
            AutoShakeDecision::Skip(AutoShakeSkip::NoContextWindow)
        );
    }

    const OPENAI: &str = "openai";
    const HOUR: Duration = Duration::from_secs(3_600);

    fn preview(min_elidable_percent: i64) -> AutoShakeDecision {
        AutoShakeDecision::Preview {
            min_elidable_percent,
            min_savings_tokens: 4_000,
        }
    }

    #[test]
    fn the_builtin_cache_ttl_policy_only_identifies_openai() {
        let config = AutoShakeConfig::default();
        // ChatGPT/Codex + OpenAI Responses: one hour.
        assert_eq!(
            config.resolved_cache_ttl_for_provider(Some(OPENAI)).ttl,
            Some(HOUR)
        );
        // Unknown providers have no safe built-in TTL.
        for provider in [
            "amazon-bedrock",
            "amazon-bedrock-runtime",
            "ollama",
            "lmstudio",
            "some-custom-provider",
        ] {
            assert_eq!(
                config.resolved_cache_ttl_for_provider(Some(provider)).ttl,
                None,
                "{provider}"
            );
        }
    }

    #[test]
    fn cold_resume_skips_when_provider_ttl_is_unknown() {
        assert_eq!(
            AutoShakeConfig::default().decide_cold_resume(
                LUNA,
                Some("ollama"),
                true,
                Some(Duration::from_secs(3_600)),
                false,
            ),
            AutoShakeDecision::Skip(AutoShakeSkip::CacheStalenessUnknown)
        );
    }

    #[test]
    fn cache_ttl_precedence_is_provider_then_global_then_builtin() {
        let config = toml("cache_ttl = \"30m\"\n");
        assert_eq!(
            config.resolved_cache_ttl_for_provider(Some(OPENAI)).ttl,
            Some(Duration::from_secs(1_800)),
            "a global override replaces the built-in openai entry"
        );
        assert_eq!(
            config.resolved_cache_ttl_for_provider(Some("ollama")).ttl,
            Some(Duration::from_secs(1_800)),
            "a global override resolves an otherwise unknown provider"
        );

        let config = toml(
            r#"
cache_ttl = "30m"
[providers."ollama"]
cache_ttl = "2h"
"#,
        );
        assert_eq!(
            config.resolved_cache_ttl_for_provider(Some("ollama")).ttl,
            Some(Duration::from_secs(7_200))
        );
        assert_eq!(
            config.resolved_cache_ttl_for_provider(Some(OPENAI)).ttl,
            Some(Duration::from_secs(1_800))
        );
    }

    #[test]
    fn cold_resume_fires_once_the_provider_ttl_has_elapsed() {
        let config = AutoShakeConfig::default();
        // One second short of openai's 1h TTL: the cache is still warm, so a
        // shake would cost a full uncached request for nothing.
        assert_eq!(
            config.decide_cold_resume(
                LUNA,
                Some(OPENAI),
                /*persistent_thread*/ true,
                Some(HOUR - Duration::from_secs(1)),
                /*already_decided*/ false,
            ),
            AutoShakeDecision::Skip(AutoShakeSkip::CacheStillWarm)
        );
        // Exactly at the TTL, and beyond it, the rebuild is already owed.
        for idle in [HOUR, HOUR + Duration::from_secs(1), HOUR * 10] {
            assert_eq!(
                config.decide_cold_resume(LUNA, Some(OPENAI), true, Some(idle), false),
                preview(30),
                "{idle:?}"
            );
        }
    }

    #[test]
    fn cold_resume_ignores_the_context_threshold_entirely() {
        // LUNA's threshold is an absolute 160k tokens; a thread far below it
        // (and with no context window at all) still cold-resume shakes.
        let config = AutoShakeConfig::default();
        assert_eq!(
            config.decide(LUNA, 1_000, None, true),
            AutoShakeDecision::Skip(AutoShakeSkip::BelowThreshold)
        );
        assert_eq!(
            config.decide_cold_resume(LUNA, Some(OPENAI), true, Some(HOUR), false),
            preview(30)
        );
    }

    #[test]
    fn cold_resume_is_deduped_within_one_idle_window() {
        let config = AutoShakeConfig::default();
        assert_eq!(
            config.decide_cold_resume(
                LUNA,
                Some(OPENAI),
                true,
                Some(HOUR * 5),
                /*already_decided*/ true
            ),
            AutoShakeDecision::Skip(AutoShakeSkip::ColdResumeAlreadyDecided)
        );
    }

    #[test]
    fn a_thread_that_never_sampled_has_nothing_to_cold_resume_from() {
        assert_eq!(
            AutoShakeConfig::default().decide_cold_resume(LUNA, Some(OPENAI), true, None, false),
            AutoShakeDecision::Skip(AutoShakeSkip::CacheStalenessUnknown)
        );
    }

    #[test]
    fn cold_resume_false_disables_only_this_trigger() {
        let config = toml("cold_resume = false\n");
        assert_eq!(
            config.decide_cold_resume(LUNA, Some(OPENAI), true, Some(HOUR * 5), false),
            AutoShakeDecision::Skip(AutoShakeSkip::ColdResumeDisabled)
        );
        // The threshold triggers are untouched.
        assert_eq!(config.decide(LUNA, 160_000, None, true), preview(30));
    }

    #[test]
    fn threshold_off_disables_cold_resume_too() {
        // `"off"` means "no auto-shake for this model", which has to include
        // the cold-resume trigger — otherwise there would be no way to opt a
        // family out of shaking entirely.
        let config = toml(
            r#"
[models."gpt-5.6"]
threshold = "off"
"#,
        );
        assert_eq!(
            config.decide_cold_resume(LUNA, Some(OPENAI), true, Some(HOUR * 5), false),
            AutoShakeDecision::Skip(AutoShakeSkip::Disabled)
        );
        // A family that is still enabled keeps cold resume.
        assert_eq!(
            config.decide_cold_resume(ASTRA, Some(OPENAI), true, Some(HOUR * 5), false),
            preview(30)
        );
    }

    #[test]
    fn cold_resume_still_needs_a_persistent_thread() {
        assert_eq!(
            AutoShakeConfig::default().decide_cold_resume(
                LUNA,
                Some(OPENAI),
                /*persistent_thread*/ false,
                Some(HOUR * 5),
                false,
            ),
            AutoShakeDecision::Skip(AutoShakeSkip::EphemeralThread)
        );
    }

    #[test]
    fn cold_resume_carries_the_resolved_min_elidable_condition() {
        // The anti-thrash conditions are the caller's to enforce, so
        // `decide_cold_resume` must hand back the same resolved values the
        // threshold path would.
        let config = toml(
            r#"
min_elidable_percent = 12
min_savings_tokens = 900
"#,
        );
        assert_eq!(
            config.decide_cold_resume(LUNA, Some(OPENAI), true, Some(HOUR * 5), false),
            AutoShakeDecision::Preview {
                min_elidable_percent: 12,
                min_savings_tokens: 900,
            }
        );
    }

    #[test]
    fn a_zero_cache_ttl_treats_every_gap_as_expired() {
        // Useful for tests and for anyone who wants to shake on every idle
        // turn; must not be rejected or silently turned into the default.
        let config = toml("cache_ttl = 0\n");
        assert_eq!(
            config.resolved_cache_ttl_for_provider(Some(OPENAI)).ttl,
            Some(Duration::ZERO)
        );
        assert_eq!(
            config.decide_cold_resume(LUNA, Some(OPENAI), true, Some(Duration::ZERO), false),
            preview(30)
        );
    }

    #[test]
    fn duration_strings_accept_seconds_minutes_hours_and_days() {
        for (written, expected_secs) in [
            ("\"90s\"", 90),
            ("\"30m\"", 1_800),
            ("\"1h\"", 3_600),
            ("\"2d\"", 172_800),
            ("450", 450),
            ("\"450\"", 450),
        ] {
            let config = toml(&format!("cache_ttl = {written}\n"));
            assert_eq!(
                config.resolved_cache_ttl_for_provider(Some(OPENAI)).ttl,
                Some(Duration::from_secs(expected_secs)),
                "{written}"
            );
        }
    }

    #[test]
    fn a_negative_cache_ttl_is_a_config_error() {
        let error = toml::from_str::<AutoShakeToml>("cache_ttl = -5\n")
            .expect_err("a negative duration must be rejected");
        assert!(error.to_string().contains("negative"), "{error}");
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
