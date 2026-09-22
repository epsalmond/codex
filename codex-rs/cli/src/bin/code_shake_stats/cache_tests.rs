use super::*;

fn row(id: i64, ts: i64, ts_nanos: i64, message: &str) -> LogRow {
    LogRow {
        id,
        ts,
        ts_nanos,
        level: "INFO".to_string(),
        target: "codex_core::shake".to_string(),
        message: Some(message.to_string()),
        thread_id: Some("thread-1".to_string()),
        process_uuid: None,
        file: None,
        line: None,
    }
}

fn metadata() -> CacheMetadata {
    CacheMetadata {
        provider_id: Some("openai".to_string()),
        ttl_secs: Some(3_600),
        ttl_source: Some("builtin_openai".to_string()),
    }
}

#[test]
fn historical_evidence_preserves_subsecond_idle_and_boundary() {
    let shake = row(2, 3_600, 300, "[shake]");
    let sampling = row(1, 0, 0, "sampling");
    let evidence = classify_cache_evidence(&shake, &metadata(), Some(&sampling));
    assert_eq!(evidence.state, CacheStaleness::LikelyExpired);
    assert_eq!(evidence.measured_idle_secs, Some(3_600.0000003));
    assert_eq!(
        evidence.evidence_source,
        Some(CacheEvidenceSource::HistoricalTimestamp)
    );
    assert_eq!(
        evidence.previous_sampling.map(|value| value.ts_nanos),
        Some(0)
    );
}

#[test]
fn warm_idle_and_zero_ttl_use_shared_classifier() {
    let shake = row(2, 10, 0, "[shake]");
    let sampling = row(1, 0, 0, "sampling");
    let mut warm_metadata = metadata();
    warm_metadata.ttl_secs = Some(60);
    assert_eq!(
        classify_cache_evidence(&shake, &warm_metadata, Some(&sampling)).state,
        CacheStaleness::Warm
    );

    warm_metadata.ttl_secs = Some(0);
    assert_eq!(
        classify_cache_evidence(&shake, &warm_metadata, Some(&sampling)).state,
        CacheStaleness::LikelyExpired
    );
}

#[test]
fn missing_metadata_and_negative_elapsed_are_explicit_unknowns() {
    let shake = row(2, 10, 0, "[shake]");
    let sampling = row(1, 11, 0, "sampling");
    let mut missing_provider = metadata();
    missing_provider.provider_id = None;
    let evidence = classify_cache_evidence(&shake, &missing_provider, Some(&sampling));
    assert_eq!(evidence.state, CacheStaleness::Unknown);
    assert_eq!(evidence.unknown_reason, Some("missing_provider_id"));
    assert_eq!(evidence.previous_sampling, None);

    let evidence = classify_cache_evidence(&shake, &metadata(), Some(&sampling));
    assert_eq!(evidence.state, CacheStaleness::Unknown);
    assert_eq!(evidence.unknown_reason, Some("negative_elapsed"));
    assert_eq!(evidence.previous_sampling.map(|value| value.ts), Some(11));
    assert_eq!(
        evidence.evidence_source,
        Some(CacheEvidenceSource::HistoricalTimestamp)
    );
}
