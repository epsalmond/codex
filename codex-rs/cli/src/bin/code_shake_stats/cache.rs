use std::time::Duration;

use codex_model_provider_info::CacheStaleness;
use codex_model_provider_info::classify_cache_staleness;
use codex_state::LogRow;

pub(super) const SAMPLING_TARGET: &str = "codex_core::sampling_request_started";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CacheMetadata {
    pub(super) provider_id: Option<String>,
    pub(super) ttl_secs: Option<i64>,
    pub(super) ttl_source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheEvidenceSource {
    HistoricalTimestamp,
}

impl CacheEvidenceSource {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::HistoricalTimestamp => "historical_timestamp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SamplingTimestamp {
    pub(super) ts: i64,
    pub(super) ts_nanos: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct CacheEvidence {
    pub(super) state: CacheStaleness,
    pub(super) measured_idle_secs: Option<f64>,
    pub(super) ttl_secs: Option<i64>,
    pub(super) ttl_source: Option<String>,
    pub(super) previous_sampling: Option<SamplingTimestamp>,
    pub(super) evidence_source: Option<CacheEvidenceSource>,
    pub(super) unknown_reason: Option<&'static str>,
}

impl Default for CacheEvidence {
    fn default() -> Self {
        Self {
            state: CacheStaleness::Unknown,
            measured_idle_secs: None,
            ttl_secs: None,
            ttl_source: None,
            previous_sampling: None,
            evidence_source: None,
            unknown_reason: Some("missing_cache_metadata"),
        }
    }
}

pub(super) fn parse_cache_metadata(message: Option<&str>) -> CacheMetadata {
    let Some(message) = message else {
        return CacheMetadata::default();
    };
    CacheMetadata {
        provider_id: parse_field(message, "provider_id"),
        ttl_secs: parse_metric(message, "cache_ttl_secs"),
        ttl_source: parse_field(message, "cache_ttl_source"),
    }
}

/// Classify the cache state using evidence persisted alongside the Shake.
///
/// Reports reconstruct the interval from the latest preceding sampling event;
/// this path is deliberately marked as lower-confidence by callers.
pub(super) fn classify_cache_evidence(
    shake: &LogRow,
    metadata: &CacheMetadata,
    previous_sampling: Option<&LogRow>,
) -> CacheEvidence {
    let mut evidence = CacheEvidence {
        ttl_secs: metadata.ttl_secs,
        ttl_source: metadata.ttl_source.clone(),
        ..CacheEvidence::default()
    };
    if metadata
        .provider_id
        .as_deref()
        .is_none_or(|provider| provider.trim().is_empty())
    {
        evidence.unknown_reason = Some("missing_provider_id");
        return evidence;
    }
    let Some(ttl_secs) = metadata.ttl_secs else {
        evidence.unknown_reason = Some("missing_cache_ttl");
        return evidence;
    };
    if ttl_secs < 0 {
        evidence.unknown_reason = Some("invalid_cache_ttl");
        return evidence;
    }
    if metadata
        .ttl_source
        .as_deref()
        .is_none_or(|source| source.trim().is_empty())
    {
        evidence.unknown_reason = Some("missing_cache_ttl_source");
        return evidence;
    }
    if !matches!(
        metadata.ttl_source.as_deref(),
        Some("provider_override" | "global_override" | "builtin_openai")
    ) {
        evidence.unknown_reason = Some("unknown_cache_ttl_source");
        return evidence;
    }
    let Some(previous_sampling) = previous_sampling else {
        evidence.unknown_reason = Some("missing_sampling_evidence");
        return evidence;
    };
    evidence.previous_sampling = Some(SamplingTimestamp {
        ts: previous_sampling.ts,
        ts_nanos: previous_sampling.ts_nanos,
    });
    evidence.evidence_source = Some(CacheEvidenceSource::HistoricalTimestamp);
    let idle_nanos = match elapsed_nanos(previous_sampling, shake) {
        Ok(idle_nanos) => idle_nanos,
        Err(reason) => {
            evidence.unknown_reason = Some(reason);
            return evidence;
        }
    };
    let idle = Duration::from_nanos(idle_nanos);
    evidence.measured_idle_secs = Some(idle.as_nanos() as f64 / 1_000_000_000.0);
    evidence.state =
        classify_cache_staleness(Some(idle), Some(Duration::from_secs(ttl_secs as u64)));
    evidence.unknown_reason = None;
    evidence
}

fn elapsed_nanos(previous: &LogRow, current: &LogRow) -> Result<u64, &'static str> {
    let previous = i128::from(previous.ts) * 1_000_000_000 + i128::from(previous.ts_nanos);
    let current = i128::from(current.ts) * 1_000_000_000 + i128::from(current.ts_nanos);
    let delta = current.checked_sub(previous).ok_or("elapsed_overflow")?;
    if delta < 0 {
        return Err("negative_elapsed");
    }
    u64::try_from(delta).map_err(|_| "elapsed_overflow")
}

fn parse_field(message: &str, key: &str) -> Option<String> {
    let value = message.split_once(&format!("{key}="))?.1;
    if let Some(quoted) = value.strip_prefix('"') {
        return Some(quoted.split('"').next()?.to_string());
    }
    let end = value
        .char_indices()
        .find(|(_, character)| matches!(character, '"' | '}' | ':' | ')' | ',' | ' ' | '\t'))
        .map_or(value.len(), |(index, _)| index);
    Some(
        value[..end]
            .trim_matches(|character| matches!(character, '"' | '}' | ':' | ')' | ','))
            .to_string(),
    )
}

fn parse_metric(message: &str, key: &str) -> Option<i64> {
    let value = message.split_whitespace().find_map(|field| {
        field.strip_prefix(&format!("{key}=")).and_then(|value| {
            let value = value
                .strip_prefix("Some(")
                .unwrap_or(value)
                .trim_end_matches(')');
            let end = value
                .char_indices()
                .find(|(_, character)| !character.is_ascii_digit() && *character != '-')
                .map_or(value.len(), |(index, _)| index);
            value[..end].parse().ok()
        })
    })?;
    Some(value)
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
