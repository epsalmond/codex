use super::*;
use crate::OPENAI_PROVIDER_ID;

fn inputs<'a>(
    provider_id: Option<&'a str>,
    provider_override: Option<u64>,
    global_override: Option<u64>,
) -> CacheTtlInputs<'a> {
    CacheTtlInputs {
        provider_id,
        provider_override: provider_override.map(Duration::from_secs),
        global_override: global_override.map(Duration::from_secs),
    }
}

#[test]
fn ttl_override_precedence_is_provider_then_global_then_builtin() {
    assert_eq!(
        resolve_cache_ttl(inputs(Some(OPENAI_PROVIDER_ID), Some(30), Some(60))),
        ResolvedCacheTtl {
            ttl: Some(Duration::from_secs(30)),
            source: CacheTtlSource::ProviderOverride,
        }
    );
    assert_eq!(
        resolve_cache_ttl(inputs(Some(OPENAI_PROVIDER_ID), None, Some(60))),
        ResolvedCacheTtl {
            ttl: Some(Duration::from_secs(60)),
            source: CacheTtlSource::GlobalOverride,
        }
    );
    assert_eq!(
        resolve_cache_ttl(inputs(Some(OPENAI_PROVIDER_ID), None, None)),
        ResolvedCacheTtl {
            ttl: Some(Duration::from_secs(3_600)),
            source: CacheTtlSource::BuiltinOpenai,
        }
    );
}

#[test]
fn unknown_provider_does_not_use_overrides() {
    for provider_id in [None, Some(""), Some(" ")] {
        assert_eq!(
            resolve_cache_ttl(inputs(provider_id, Some(30), Some(60))),
            ResolvedCacheTtl {
                ttl: None,
                source: CacheTtlSource::Unknown,
            }
        );
    }
    assert_eq!(
        resolve_cache_ttl(inputs(Some("ollama"), None, None)),
        ResolvedCacheTtl {
            ttl: None,
            source: CacheTtlSource::Unknown,
        }
    );
}

#[test]
fn zero_ttl_is_valid_and_stale_at_zero_idle() {
    let resolved = resolve_cache_ttl(inputs(Some("ollama"), Some(0), None));
    assert_eq!(resolved.ttl, Some(Duration::ZERO));
    assert_eq!(resolved.source, CacheTtlSource::ProviderOverride);
    assert_eq!(
        classify_cache_staleness(Some(Duration::ZERO), resolved.ttl),
        CacheStaleness::LikelyExpired
    );
}

#[test]
fn staleness_uses_strictly_less_than_for_warm_boundary() {
    assert_eq!(
        classify_cache_staleness(Some(Duration::from_secs(59)), Some(Duration::from_secs(60))),
        CacheStaleness::Warm
    );
    assert_eq!(
        classify_cache_staleness(Some(Duration::from_secs(60)), Some(Duration::from_secs(60))),
        CacheStaleness::LikelyExpired
    );
    assert_eq!(
        classify_cache_staleness(Some(Duration::from_secs(61)), Some(Duration::from_secs(60))),
        CacheStaleness::LikelyExpired
    );
}

#[test]
fn missing_idle_or_ttl_is_unknown() {
    assert_eq!(
        classify_cache_staleness(None, Some(Duration::from_secs(60))),
        CacheStaleness::Unknown
    );
    assert_eq!(
        classify_cache_staleness(Some(Duration::from_secs(60)), None),
        CacheStaleness::Unknown
    );
}
