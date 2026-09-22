//! Shared prompt-cache TTL resolution and staleness classification.

use std::time::Duration;

/// Inputs used to resolve the prompt-cache TTL for one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheTtlInputs<'a> {
    /// The configured model-provider id. A missing or empty id is unknown even
    /// when an override is present, because the override cannot be attributed
    /// to a provider safely.
    pub provider_id: Option<&'a str>,
    /// Provider-specific TTL override.
    pub provider_override: Option<Duration>,
    /// Global TTL override.
    pub global_override: Option<Duration>,
}

/// The source used for a resolved prompt-cache TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtlSource {
    /// A provider-specific override.
    ProviderOverride,
    /// A global override.
    GlobalOverride,
    /// The built-in OpenAI TTL.
    BuiltinOpenai,
    /// No provider identity or known TTL was available.
    Unknown,
}

impl CacheTtlSource {
    /// Stable machine-readable source name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderOverride => "provider_override",
            Self::GlobalOverride => "global_override",
            Self::BuiltinOpenai => "builtin_openai",
            Self::Unknown => "unknown",
        }
    }
}

/// A prompt-cache TTL together with the source that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCacheTtl {
    /// The resolved TTL, or `None` when it is unknown.
    pub ttl: Option<Duration>,
    /// How the TTL was resolved.
    pub source: CacheTtlSource,
}

/// Resolve a provider's prompt-cache TTL.
pub fn resolve_cache_ttl(inputs: CacheTtlInputs<'_>) -> ResolvedCacheTtl {
    let Some(provider_id) = inputs
        .provider_id
        .filter(|provider_id| !provider_id.trim().is_empty())
    else {
        return ResolvedCacheTtl {
            ttl: None,
            source: CacheTtlSource::Unknown,
        };
    };

    if let Some(ttl) = inputs.provider_override {
        return ResolvedCacheTtl {
            ttl: Some(ttl),
            source: CacheTtlSource::ProviderOverride,
        };
    }
    if let Some(ttl) = inputs.global_override {
        return ResolvedCacheTtl {
            ttl: Some(ttl),
            source: CacheTtlSource::GlobalOverride,
        };
    }
    if provider_id == super::OPENAI_PROVIDER_ID {
        return ResolvedCacheTtl {
            ttl: Some(Duration::from_secs(3_600)),
            source: CacheTtlSource::BuiltinOpenai,
        };
    }
    ResolvedCacheTtl {
        ttl: None,
        source: CacheTtlSource::Unknown,
    }
}

/// Whether the observed idle interval is within the resolved cache TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStaleness {
    /// The idle interval is shorter than the TTL.
    Warm,
    /// The idle interval is equal to or longer than the TTL.
    LikelyExpired,
    /// Idle time or TTL evidence is missing.
    Unknown,
}

impl CacheStaleness {
    /// Stable machine-readable staleness name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::LikelyExpired => "likely_expired",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify prompt-cache staleness from idle time and TTL evidence.
pub fn classify_cache_staleness(idle: Option<Duration>, ttl: Option<Duration>) -> CacheStaleness {
    match (idle, ttl) {
        (Some(idle), Some(ttl)) if idle < ttl => CacheStaleness::Warm,
        (Some(_), Some(_)) => CacheStaleness::LikelyExpired,
        _ => CacheStaleness::Unknown,
    }
}

#[cfg(test)]
#[path = "cache_staleness_tests.rs"]
mod tests;
