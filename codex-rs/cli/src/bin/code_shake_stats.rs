use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use codex_core::config::ConfigBuilder;
use codex_model_provider_info::CacheStaleness;
use codex_state::LogQuery;
use codex_state::LogRow;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Serialize;

#[path = "code_shake_stats/cache.rs"]
mod cache;
#[path = "code_shake_stats/output.rs"]
mod output;

use cache::SAMPLING_TARGET;
use cache::classify_cache_evidence;
use cache::parse_cache_metadata;
use output::print_report;
#[cfg(test)]
use output::render_report;

const SHAKE_TARGET: &str = "codex_core::shake";

#[derive(Debug, Parser)]
#[command(name = "code-shake-stats")]
#[command(about = "Summarize observed Shake reductions and estimated following-prompt savings")]
struct Args {
    /// Path to CODEX_HOME. Defaults to $CODEX_HOME or ~/.codex.
    #[arg(long, env = "CODEX_HOME")]
    codex_home: Option<PathBuf>,

    /// Direct path to the logs SQLite database. Overrides --codex-home.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Include shakes from this many days ago through now.
    #[arg(long, default_value_t = 3)]
    since_days: i64,

    /// Number of following model prompts used for the savings estimate.
    #[arg(long, default_value_t = 3)]
    following_turns: usize,

    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Disable ANSI colors in human-readable output.
    #[arg(long)]
    no_color: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
struct Totals {
    shakes: usize,
    tokens_freed: i64,
    cache_rewrite_burn_tokens: i64,
    cache_rewrite_requests: usize,
    cache_read_tokens: i64,
    quota_usage_tokens: i64,
    following_prompts_observed: usize,
    likely_saved_tokens: i64,
    net_estimated_tokens: i64,
    incremental_cache_rewrite_tokens: i64,
    provisional_estimates: usize,
    cache_state_counts: CacheStateCounts,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
struct CacheStateCounts {
    warm: usize,
    likely_expired: usize,
    unknown: usize,
}

#[derive(Debug, Clone, Serialize)]
struct ShakeReport {
    timestamp: String,
    thread_id: Option<String>,
    turn_id: Option<String>,
    model: String,
    provider_id: Option<String>,
    trigger: String,
    tokens_freed: i64,
    cache_rewrite_burn_tokens: Option<i64>,
    cache_read_tokens: i64,
    quota_usage_tokens: i64,
    following_prompts_observed: usize,
    likely_saved_tokens: i64,
    raw_net_estimated_tokens: i64,
    incremental_cache_rewrite_tokens: i64,
    net_estimated_tokens: i64,
    cache_state: String,
    measured_idle_secs: Option<f64>,
    cache_ttl_secs: Option<i64>,
    cache_ttl_source: Option<String>,
    previous_sampling_timestamp: Option<String>,
    evidence_source: Option<String>,
    evidence_confidence: Option<String>,
    unknown_reason: Option<String>,
    estimate_provisional: bool,
}

#[derive(Debug, Default, Serialize)]
struct ModelStats {
    model: String,
    shakes: usize,
    tokens_freed: i64,
    cache_read_tokens: i64,
    cache_rewrite_burn_tokens: i64,
    quota_usage_tokens: i64,
    likely_saved_tokens: i64,
    following_prompts_observed: usize,
    incremental_cache_rewrite_tokens: i64,
    net_estimated_tokens: i64,
    provisional_estimates: usize,
    cache_state_counts: CacheStateCounts,
}

#[derive(Debug, Serialize)]
struct Report {
    since: String,
    following_turns: usize,
    estimate_note: &'static str,
    totals: Totals,
    models: Vec<ModelStats>,
    shakes: Vec<ShakeReport>,
}

#[derive(Debug, Clone, Default)]
struct TokenUsage {
    input_tokens: i64,
    cached_input_tokens: i64,
    non_cached_input_tokens: i64,
    cache_write_input_tokens: i64,
    total_tokens: i64,
}

#[derive(Debug)]
struct ShakeEvent {
    row: LogRow,
    thread_id: Option<String>,
    model: String,
    turn_id: Option<String>,
    trigger: String,
    tokens_freed: i64,
    cache_metadata: cache::CacheMetadata,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(args.since_days >= 0, "--since-days must be non-negative");
    let sqlite = resolve_sqlite_config(&args).await?;
    let runtime = StateRuntime::init(sqlite, "code-shake-stats".to_string()).await?;
    let since_ts = Utc::now().timestamp() - args.since_days * 24 * 60 * 60;
    let query = LogQuery {
        from_ts: Some(since_ts),
        ..LogQuery::default()
    };
    // A query without a thread filter already returns both thread-bound and
    // threadless rows. Do not issue a second include-threadless query: that
    // would duplicate every threadless row in the report.
    let mut rows = runtime
        .query_logs(&query)
        .await
        .context("failed to query thread-bound Shake logs")?;
    rows.sort_by_key(|row| row.id);
    let mut sampling_rows = HashMap::new();
    for shake in rows.iter().filter(|row| {
        row.target == SHAKE_TARGET
            && row
                .message
                .as_deref()
                .is_some_and(|message| message.contains("[shake]"))
    }) {
        let Some(thread_id) = effective_thread_id(shake) else {
            sampling_rows.insert(shake.id, None);
            continue;
        };
        let preceding = runtime
            .query_logs(&LogQuery {
                thread_ids: vec![thread_id],
                before_id: Some(shake.id),
                target: Some(SAMPLING_TARGET.to_string()),
                limit: Some(1),
                descending: true,
                ..LogQuery::default()
            })
            .await
            .with_context(|| format!("failed to query sampling evidence before log {}", shake.id))?
            .into_iter()
            .next();
        sampling_rows.insert(shake.id, preceding);
    }
    let report = build_report_with_sampling(&rows, &sampling_rows, since_ts, args.following_turns);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report, !args.no_color && std::io::stdout().is_terminal());
    }
    Ok(())
}

async fn resolve_sqlite_config(args: &Args) -> anyhow::Result<SqliteConfig> {
    if let Some(db_path) = args.db.as_ref() {
        let sqlite_home = db_path
            .parent()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| PathBuf::from("."));
        return Ok(SqliteConfig::from_sqlite_home(
            AbsolutePathBuf::relative_to_current_dir(sqlite_home)?,
        ));
    }

    let mut config_builder = ConfigBuilder::default();
    if let Some(codex_home) = args.codex_home.as_ref() {
        config_builder = config_builder.codex_home(codex_home.clone());
    }
    let config = config_builder.build().await?;
    Ok(config.sqlite_config().clone())
}

#[cfg(test)]
fn build_report(rows: &[LogRow], since_ts: i64, following_turns: usize) -> Report {
    build_report_with_sampling(rows, &HashMap::new(), since_ts, following_turns)
}

fn build_report_with_sampling(
    rows: &[LogRow],
    sampling_rows: &HashMap<i64, Option<LogRow>>,
    since_ts: i64,
    following_turns: usize,
) -> Report {
    let mut current_models = HashMap::new();
    let shakes = rows
        .iter()
        .filter_map(|row| {
            let thread_id = effective_thread_id(row);
            if let Some(model) = parse_model(Some(row.message.as_deref()?)) {
                current_models.insert(thread_id.clone(), model);
            }
            parse_shake(
                row,
                thread_id.clone(),
                current_models.get(&thread_id).cloned(),
            )
        })
        .collect::<Vec<_>>();
    let mut reports = Vec::with_capacity(shakes.len());
    for (shake_index, shake) in shakes.iter().enumerate() {
        let mut following_prompts_observed = 0;
        let mut likely_saved_tokens = 0;
        let mut cache_rewrite_burn_tokens = None;
        let mut cache_read_tokens = 0;
        let mut quota_usage_tokens = 0;
        let next_shake_id = shakes
            .iter()
            .skip(shake_index + 1)
            .find(|next| next.thread_id == shake.thread_id)
            .map(|next| next.row.id);
        for row in rows.iter().filter(|row| {
            row.id > shake.row.id
                && next_shake_id.is_none_or(|next_id| row.id < next_id)
                && shake.thread_id.is_some()
                && effective_thread_id(row) == shake.thread_id
        }) {
            if let Some(usage) = parse_token_usage(row) {
                if cache_rewrite_burn_tokens.is_none() {
                    cache_rewrite_burn_tokens = Some(usage.cache_rewrite_burn_tokens());
                }
                if following_prompts_observed < following_turns {
                    following_prompts_observed += 1;
                    cache_read_tokens += usage.cached_input_tokens;
                    quota_usage_tokens += usage.total_tokens;
                    likely_saved_tokens += shake.tokens_freed.min(usage.input_tokens.max(0));
                }
                if following_prompts_observed == following_turns {
                    break;
                }
            }
        }
        let cache_evidence = classify_cache_evidence(
            &shake.row,
            &shake.cache_metadata,
            sampling_rows.get(&shake.row.id).and_then(Option::as_ref),
        );
        let incremental_cache_rewrite_tokens = match cache_evidence.state {
            CacheStaleness::LikelyExpired => 0,
            CacheStaleness::Warm | CacheStaleness::Unknown => {
                cache_rewrite_burn_tokens.unwrap_or_default()
            }
        };
        let raw_net_estimated_tokens =
            likely_saved_tokens - cache_rewrite_burn_tokens.unwrap_or_default();
        reports.push(ShakeReport {
            timestamp: format_timestamp(shake.row.ts, shake.row.ts_nanos),
            thread_id: shake.thread_id.clone(),
            turn_id: shake.turn_id.clone(),
            model: shake.model.clone(),
            provider_id: shake.cache_metadata.provider_id.clone(),
            trigger: shake.trigger.clone(),
            tokens_freed: shake.tokens_freed,
            cache_rewrite_burn_tokens,
            cache_read_tokens,
            quota_usage_tokens,
            following_prompts_observed,
            likely_saved_tokens,
            raw_net_estimated_tokens,
            incremental_cache_rewrite_tokens,
            net_estimated_tokens: likely_saved_tokens - incremental_cache_rewrite_tokens,
            cache_state: cache_evidence.state.as_str().to_string(),
            measured_idle_secs: cache_evidence.measured_idle_secs,
            cache_ttl_secs: cache_evidence.ttl_secs,
            cache_ttl_source: cache_evidence.ttl_source.clone(),
            previous_sampling_timestamp: cache_evidence
                .previous_sampling
                .map(|timestamp| format_timestamp(timestamp.ts, timestamp.ts_nanos)),
            evidence_source: cache_evidence
                .evidence_source
                .map(|source| source.as_str().to_string()),
            evidence_confidence: cache_evidence.evidence_source.map(|source| {
                match source {
                    cache::CacheEvidenceSource::HistoricalTimestamp => "lower_than_runtime_clock",
                }
                .to_string()
            }),
            unknown_reason: cache_evidence.unknown_reason.map(str::to_string),
            estimate_provisional: cache_evidence.state == CacheStaleness::Unknown,
        });
    }

    let mut totals = Totals {
        shakes: reports.len(),
        ..Totals::default()
    };
    let mut models = BTreeMap::<String, ModelStats>::new();
    for row in rows {
        if let Some(usage) = parse_token_usage(row) {
            let model_name =
                parse_model(row.message.as_deref()).unwrap_or_else(|| "unknown".to_string());
            let model = models
                .entry(model_name.clone())
                .or_insert_with(|| ModelStats {
                    model: model_name,
                    ..ModelStats::default()
                });
            model.cache_read_tokens += usage.cached_input_tokens;
            model.quota_usage_tokens += usage.total_tokens;
            totals.cache_read_tokens += usage.cached_input_tokens;
            totals.quota_usage_tokens += usage.total_tokens;
        }
    }
    for report in &reports {
        let model = models
            .entry(report.model.clone())
            .or_insert_with(|| ModelStats {
                model: report.model.clone(),
                ..ModelStats::default()
            });
        model.shakes += 1;
        model.tokens_freed += report.tokens_freed;
        model.cache_rewrite_burn_tokens += report.cache_rewrite_burn_tokens.unwrap_or_default();
        model.likely_saved_tokens += report.likely_saved_tokens;
        model.following_prompts_observed += report.following_prompts_observed;
        model.incremental_cache_rewrite_tokens += report.incremental_cache_rewrite_tokens;
        model.net_estimated_tokens += report.net_estimated_tokens;
        if report.estimate_provisional {
            model.provisional_estimates += 1;
        }
        match report.cache_state.as_str() {
            "warm" => model.cache_state_counts.warm += 1,
            "likely_expired" => model.cache_state_counts.likely_expired += 1,
            "unknown" => model.cache_state_counts.unknown += 1,
            _ => unreachable!("cache state is produced by CacheStaleness"),
        }
        totals.tokens_freed += report.tokens_freed;
        totals.likely_saved_tokens += report.likely_saved_tokens;
        totals.incremental_cache_rewrite_tokens += report.incremental_cache_rewrite_tokens;
        totals.net_estimated_tokens += report.net_estimated_tokens;
        if report.estimate_provisional {
            totals.provisional_estimates += 1;
        }
        match report.cache_state.as_str() {
            "warm" => totals.cache_state_counts.warm += 1,
            "likely_expired" => totals.cache_state_counts.likely_expired += 1,
            "unknown" => totals.cache_state_counts.unknown += 1,
            _ => unreachable!("cache state is produced by CacheStaleness"),
        }
        if let Some(tokens) = report.cache_rewrite_burn_tokens {
            totals.cache_rewrite_requests += 1;
            totals.cache_rewrite_burn_tokens += tokens;
        }
        totals.following_prompts_observed += report.following_prompts_observed;
    }
    Report {
        since: format_timestamp(since_ts, 0),
        following_turns,
        estimate_note: "Cache reads and total tokens are observed telemetry. Rewrite burn is a raw proxy for the first uncached follow-up input, not necessarily a provider cache-write charge. Incremental rewrite is zero when the cache is likely expired and otherwise uses that proxy; net is saved-input proxy minus incremental rewrite. Historical timestamp evidence has lower confidence than runtime clock evidence. Unknown-derived estimates are provisional, and no cache state guarantees a provider cache hit.",
        totals,
        models: models.into_values().collect(),
        shakes: reports,
    }
}

fn parse_shake(
    row: &LogRow,
    thread_id: Option<String>,
    model: Option<String>,
) -> Option<ShakeEvent> {
    if row.target != SHAKE_TARGET || !row.message.as_deref()?.contains("[shake]") {
        return None;
    }
    Some(ShakeEvent {
        row: row.clone(),
        thread_id,
        model: model.unwrap_or_else(|| "unknown".to_string()),
        turn_id: parse_field(row.message.as_deref()?, "turn.id")
            .or_else(|| parse_field(row.message.as_deref()?, "turn_id")),
        trigger: parse_field(row.message.as_deref()?, "trigger")
            .unwrap_or_else(|| "unknown".to_string()),
        tokens_freed: parse_metric(row.message.as_deref()?, "tokens_freed")?,
        cache_metadata: parse_cache_metadata(row.message.as_deref()),
    })
}

fn parse_token_usage(row: &LogRow) -> Option<TokenUsage> {
    let message = row.message.as_deref()?;
    Some(TokenUsage {
        input_tokens: parse_metric(message, "codex.turn.token_usage.input_tokens")?,
        cached_input_tokens: parse_metric(message, "codex.turn.token_usage.cached_input_tokens")?,
        non_cached_input_tokens: parse_metric(
            message,
            "codex.turn.token_usage.non_cached_input_tokens",
        )?,
        cache_write_input_tokens: parse_metric(
            message,
            "codex.turn.token_usage.cache_write_input_tokens",
        )?,
        total_tokens: parse_metric(message, "codex.turn.token_usage.total_tokens")?,
    })
}

fn parse_model(message: Option<&str>) -> Option<String> {
    parse_field(message?, "model")
}

fn effective_thread_id(row: &LogRow) -> Option<String> {
    row.thread_id
        .clone()
        .or_else(|| parse_field(row.message.as_deref()?, "thread.id"))
        .or_else(|| parse_field(row.message.as_deref()?, "thread_id"))
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

impl TokenUsage {
    fn cache_rewrite_burn_tokens(&self) -> i64 {
        if self.cache_write_input_tokens > 0 {
            self.cache_write_input_tokens
        } else if self.non_cached_input_tokens > 0 {
            self.non_cached_input_tokens
        } else {
            (self.input_tokens - self.cached_input_tokens).max(0)
        }
    }
}

fn parse_metric(message: &str, key: &str) -> Option<i64> {
    let value = message.split_whitespace().find_map(|field| {
        field.strip_prefix(&format!("{key}=")).and_then(|value| {
            let end = value
                .char_indices()
                .find(|(_, character)| !character.is_ascii_digit() && *character != '-')
                .map_or(value.len(), |(index, _)| index);
            value[..end].parse().ok()
        })
    })?;
    Some(value)
}

fn format_timestamp(ts: i64, ts_nanos: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, u32::try_from(ts_nanos).unwrap_or_default())
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| format!("{ts}.{ts_nanos:09}Z"))
}

#[cfg(test)]
#[path = "code_shake_stats_tests/mod.rs"]
mod tests;
