use super::*;
use codex_state::LogEntry;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn row(id: i64, target: &str, message: &str, thread_id: &str) -> LogRow {
    LogRow {
        id,
        ts: id,
        ts_nanos: 0,
        level: "INFO".to_string(),
        target: target.to_string(),
        message: Some(message.to_string()),
        thread_id: Some(thread_id.to_string()),
        process_uuid: None,
        file: None,
        line: None,
    }
}

#[test]
fn report_separates_observed_reduction_rewrite_burn_and_estimate() {
    let rows = vec![
        row(1, SHAKE_TARGET, "[shake] tokens_freed=100", "thread-1"),
        row(
            2,
            "codex_otel",
            "codex.turn.token_usage.input_tokens=80 codex.turn.token_usage.cached_input_tokens=20 codex.turn.token_usage.non_cached_input_tokens=60 codex.turn.token_usage.cache_write_input_tokens=100 codex.turn.token_usage.total_tokens=90",
            "thread-1",
        ),
        row(
            3,
            "codex_otel",
            "codex.turn.token_usage.input_tokens=120 codex.turn.token_usage.cached_input_tokens=100 codex.turn.token_usage.non_cached_input_tokens=20 codex.turn.token_usage.cache_write_input_tokens=0 codex.turn.token_usage.total_tokens=130",
            "thread-1",
        ),
    ];
    let report = build_report(&rows, 0, 2);
    assert_eq!(report.totals.shakes, 1);
    assert_eq!(report.totals.tokens_freed, 100);
    assert_eq!(report.totals.cache_rewrite_burn_tokens, 100);
    assert_eq!(report.totals.cache_read_tokens, 120);
    assert_eq!(report.totals.quota_usage_tokens, 220);
    assert_eq!(report.totals.likely_saved_tokens, 180);
    assert_eq!(report.totals.following_prompts_observed, 2);
}

#[test]
fn report_does_not_cross_threads() {
    let rows = vec![
        row(1, SHAKE_TARGET, "[shake] tokens_freed=100", "thread-1"),
        row(
            2,
            "codex_otel",
            "codex.turn.token_usage.input_tokens=80 codex.turn.token_usage.cached_input_tokens=40 codex.turn.token_usage.non_cached_input_tokens=40 codex.turn.token_usage.cache_write_input_tokens=40 codex.turn.token_usage.total_tokens=90",
            "thread-2",
        ),
    ];
    let report = build_report(&rows, 0, 3);
    assert_eq!(report.totals.cache_rewrite_requests, 0);
    assert_eq!(report.totals.following_prompts_observed, 0);
}

#[test]
fn report_joins_threadless_usage_by_trace_thread_id() {
    let rows = [
        row(
            1,
            SHAKE_TARGET,
            "session_loop{thread.id=thread-1}:turn{model=gpt-5.6-luna turn.id=turn-1}: [shake] trigger=automatic tokens_freed=100",
            "thread-1",
        ),
        row(
            2,
            "codex_otel.agent_communication",
            "session_loop{thread.id=thread-1}:turn{model=gpt-5.6-luna turn.id=turn-2}: codex.turn.token_usage.input_tokens=80 codex.turn.token_usage.cached_input_tokens=20 codex.turn.token_usage.non_cached_input_tokens=60 codex.turn.token_usage.cache_write_input_tokens=0 codex.turn.token_usage.total_tokens=90",
            "thread-1",
        ),
    ];
    let mut threadless_row = rows[1].clone();
    threadless_row.thread_id = None;
    let report = build_report(&[rows[0].clone(), threadless_row], 0, 1);
    assert_eq!(report.totals.following_prompts_observed, 1);
    assert_eq!(report.totals.cache_read_tokens, 20);
    assert_eq!(report.totals.likely_saved_tokens, 80);
    assert_eq!(report.models[0].model, "gpt-5.6-luna");
}

#[test]
fn report_does_not_double_count_followups_between_shakes() {
    let rows = vec![
        row(1, SHAKE_TARGET, "[shake] tokens_freed=100", "thread-1"),
        row(
            2,
            "codex_otel",
            "codex.turn.token_usage.input_tokens=80 codex.turn.token_usage.cached_input_tokens=20 codex.turn.token_usage.non_cached_input_tokens=60 codex.turn.token_usage.cache_write_input_tokens=0 codex.turn.token_usage.total_tokens=90",
            "thread-1",
        ),
        row(3, SHAKE_TARGET, "[shake] tokens_freed=50", "thread-1"),
        row(
            4,
            "codex_otel",
            "codex.turn.token_usage.input_tokens=40 codex.turn.token_usage.cached_input_tokens=10 codex.turn.token_usage.non_cached_input_tokens=30 codex.turn.token_usage.cache_write_input_tokens=0 codex.turn.token_usage.total_tokens=45",
            "thread-1",
        ),
    ];
    let report = build_report(&rows, 0, 3);
    assert_eq!(report.totals.following_prompts_observed, 2);
    assert_eq!(report.totals.likely_saved_tokens, 120);
    assert_eq!(report.totals.net_estimated_tokens, 30);
}

fn cache_row(id: i64, ts: i64, message: &str, target: &str, thread_id: &str) -> LogRow {
    let mut row = row(id, target, message, thread_id);
    row.ts = ts;
    row
}

fn metadata_message(extra: &str) -> String {
    format!(
        "model=gpt-5.6-luna provider_id=openai cache_ttl_secs=3600 cache_ttl_source=builtin_openai tokens_freed=100 trigger=automatic [shake] {extra}"
    )
}

#[test]
fn report_classifies_warm_and_likely_expired_with_incremental_economics() {
    let warm_shake = cache_row(
        10,
        100,
        &metadata_message("warm"),
        SHAKE_TARGET,
        "thread-warm",
    );
    let expired_shake = cache_row(
        20,
        4_000,
        &metadata_message("expired"),
        SHAKE_TARGET,
        "thread-expired",
    );
    let warm_sampling = cache_row(
        9,
        0,
        "model=gpt-5.6-luna provider_id=openai",
        cache::SAMPLING_TARGET,
        "thread-warm",
    );
    let expired_sampling = cache_row(
        19,
        0,
        "model=gpt-5.6-luna provider_id=openai",
        cache::SAMPLING_TARGET,
        "thread-expired",
    );
    let warm_usage = cache_row(
        11,
        101,
        "model=gpt-5.6-luna codex.turn.token_usage.input_tokens=80 codex.turn.token_usage.cached_input_tokens=20 codex.turn.token_usage.non_cached_input_tokens=60 codex.turn.token_usage.cache_write_input_tokens=60 codex.turn.token_usage.total_tokens=90",
        "codex_otel",
        "thread-warm",
    );
    let expired_usage = cache_row(
        21,
        4_001,
        "model=gpt-5.6-luna codex.turn.token_usage.input_tokens=80 codex.turn.token_usage.cached_input_tokens=20 codex.turn.token_usage.non_cached_input_tokens=60 codex.turn.token_usage.cache_write_input_tokens=60 codex.turn.token_usage.total_tokens=90",
        "codex_otel",
        "thread-expired",
    );
    let rows = vec![warm_shake, expired_shake, warm_usage, expired_usage];
    let sampling = HashMap::from([(10, Some(warm_sampling)), (20, Some(expired_sampling))]);
    let report = build_report_with_sampling(&rows, &sampling, 0, 1);

    assert_eq!(report.totals.cache_state_counts.warm, 1);
    assert_eq!(report.totals.cache_state_counts.likely_expired, 1);
    assert_eq!(report.totals.cache_state_counts.unknown, 0);
    assert_eq!(report.totals.incremental_cache_rewrite_tokens, 60);
    assert_eq!(report.totals.net_estimated_tokens, 100);
    assert_eq!(report.shakes[0].measured_idle_secs, Some(100.0));
    assert_eq!(
        report.shakes[0].evidence_source.as_deref(),
        Some("historical_timestamp")
    );
    assert_eq!(report.shakes[1].incremental_cache_rewrite_tokens, 0);
    assert_eq!(report.shakes[1].net_estimated_tokens, 80);

    let json = serde_json::to_value(&report).expect("serialize report");
    assert_eq!(json["totals"]["cache_state_counts"]["warm"], 1);
    assert_eq!(json["shakes"][0]["cache_ttl_source"], "builtin_openai");
    assert_eq!(
        json["shakes"][0]["evidence_confidence"],
        "lower_than_runtime_clock"
    );
}

#[test]
fn report_marks_missing_and_negative_sampling_evidence_provisional() {
    let missing_sampling_shake = cache_row(
        10,
        100,
        &metadata_message("missing"),
        SHAKE_TARGET,
        "thread-missing",
    );
    let negative_shake = cache_row(
        20,
        90,
        &metadata_message("negative"),
        SHAKE_TARGET,
        "thread-negative",
    );
    let negative_sampling = cache_row(
        19,
        100,
        "model=gpt-5.6-luna provider_id=openai",
        cache::SAMPLING_TARGET,
        "thread-negative",
    );
    let report = build_report_with_sampling(
        &[missing_sampling_shake, negative_shake],
        &HashMap::from([(20, Some(negative_sampling))]),
        0,
        0,
    );

    assert_eq!(report.totals.cache_state_counts.unknown, 2);
    assert_eq!(report.totals.provisional_estimates, 2);
    assert_eq!(
        report.shakes[0].unknown_reason.as_deref(),
        Some("missing_sampling_evidence")
    );
    assert_eq!(
        report.shakes[1].unknown_reason.as_deref(),
        Some("negative_elapsed")
    );
    assert_eq!(report.shakes[1].previous_sampling_timestamp.is_some(), true);
    assert_eq!(report.shakes[0].incremental_cache_rewrite_tokens, 0);
}

#[tokio::test]
async fn database_seed_outside_window_is_excluded_from_usage_totals() {
    let codex_home = TempDir::new().expect("create test home");
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(codex_home.path().abs()),
        "code-shake-stats-test".to_string(),
    )
    .await
    .expect("initialize test runtime");
    runtime
        .insert_logs(&[
            LogEntry {
                ts: 900,
                ts_nanos: 999,
                level: "INFO".to_string(),
                target: cache::SAMPLING_TARGET.to_string(),
                message: Some(
                    "model=gpt-5.6-luna provider_id=openai codex.turn.token_usage.input_tokens=999 codex.turn.token_usage.cached_input_tokens=999 codex.turn.token_usage.non_cached_input_tokens=0 codex.turn.token_usage.cache_write_input_tokens=0 codex.turn.token_usage.total_tokens=999".to_string(),
                ),
                feedback_log_body: Some(
                    "model=gpt-5.6-luna provider_id=openai codex.turn.token_usage.input_tokens=999 codex.turn.token_usage.cached_input_tokens=999 codex.turn.token_usage.non_cached_input_tokens=0 codex.turn.token_usage.cache_write_input_tokens=0 codex.turn.token_usage.total_tokens=999".to_string(),
                ),
                thread_id: Some("thread-1".to_string()),
                process_uuid: None,
                file: None,
                line: None,
                module_path: None,
            },
            LogEntry {
                ts: 1_100,
                ts_nanos: 1,
                level: "INFO".to_string(),
                target: SHAKE_TARGET.to_string(),
                message: Some(metadata_message("database")),
                feedback_log_body: Some(metadata_message("database")),
                thread_id: Some("thread-1".to_string()),
                process_uuid: None,
                file: None,
                line: None,
                module_path: None,
            },
        ])
        .await
        .expect("insert test logs");

    let report_rows = runtime
        .query_logs(&LogQuery {
            from_ts: Some(1_000),
            ..LogQuery::default()
        })
        .await
        .expect("query report rows");
    assert_eq!(report_rows.len(), 1);
    let shake = &report_rows[0];
    let sampling = runtime
        .query_logs(&LogQuery {
            thread_ids: vec!["thread-1".to_string()],
            before_id: Some(shake.id),
            target: Some(cache::SAMPLING_TARGET.to_string()),
            limit: Some(1),
            descending: true,
            ..LogQuery::default()
        })
        .await
        .expect("query seed row")
        .into_iter()
        .next();
    let sampling_rows = HashMap::from([(shake.id, sampling)]);
    let report = build_report_with_sampling(&report_rows, &sampling_rows, 1_000, 1);

    assert_eq!(report.totals.cache_state_counts.warm, 1);
    assert_eq!(report.totals.cache_read_tokens, 0);
    assert_eq!(report.totals.quota_usage_tokens, 0);
    assert_eq!(report.shakes[0].previous_sampling_timestamp.is_some(), true);
}

#[test]
fn human_report_snapshot_covers_cache_state_summary() {
    let report = build_report(&[], 0, 3);
    insta::assert_snapshot!(
        render_report(&report, false),
        @"
    Shake stats since 1970-01-01T00:00:00+00:00
      shakes: 0
      tokens shaken out: 0
      cache reads: 0 tokens
      cache rewrite burn proxy: 0 tokens across 0 observed following prompts
      observed quota usage: 0 total tokens
      likely saved in following prompts: 0 tokens across 0 prompts
      net estimated input impact: 0 tokens
      incremental rewrite estimate: 0 tokens
      cache states: warm=0 likely_expired=0 unknown=0

    By model

    Shakes by turn

      note: Cache reads and total tokens are observed telemetry. Rewrite burn is a raw proxy for the first uncached follow-up input, not necessarily a provider cache-write charge. Incremental rewrite is zero when the cache is likely expired and otherwise uses that proxy; net is saved-input proxy minus incremental rewrite. Historical timestamp evidence has lower confidence than runtime clock evidence. Unknown-derived estimates are provisional, and no cache state guarantees a provider cache hit.
    "
    );
}

#[test]
fn human_report_snapshot_covers_classified_and_provisional_shakes() {
    let warm_shake = cache_row(
        10,
        100,
        &metadata_message("warm"),
        SHAKE_TARGET,
        "thread-warm",
    );
    let unknown_shake = cache_row(
        20,
        200,
        "model=gpt-5.6-luna trigger=automatic [shake] tokens_freed=50",
        SHAKE_TARGET,
        "thread-unknown",
    );
    let sampling = cache_row(
        9,
        0,
        "model=gpt-5.6-luna provider_id=openai",
        cache::SAMPLING_TARGET,
        "thread-warm",
    );
    let usage = cache_row(
        11,
        101,
        "model=gpt-5.6-luna codex.turn.token_usage.input_tokens=80 codex.turn.token_usage.cached_input_tokens=20 codex.turn.token_usage.non_cached_input_tokens=60 codex.turn.token_usage.cache_write_input_tokens=60 codex.turn.token_usage.total_tokens=90",
        "codex_otel",
        "thread-warm",
    );
    let unknown_usage = cache_row(
        21,
        201,
        "model=gpt-5.6-luna codex.turn.token_usage.input_tokens=40 codex.turn.token_usage.cached_input_tokens=10 codex.turn.token_usage.non_cached_input_tokens=30 codex.turn.token_usage.cache_write_input_tokens=30 codex.turn.token_usage.total_tokens=45",
        "codex_otel",
        "thread-unknown",
    );
    let report = build_report_with_sampling(
        &[warm_shake, unknown_shake, usage, unknown_usage],
        &HashMap::from([(10, Some(sampling))]),
        0,
        1,
    );
    assert_eq!(report.shakes[1].incremental_cache_rewrite_tokens, 30);
    assert_eq!(report.shakes[1].net_estimated_tokens, 10);
    assert!(report.shakes[1].estimate_provisional);
    assert_eq!(report.totals.incremental_cache_rewrite_tokens, 90);
    assert_eq!(report.totals.net_estimated_tokens, 30);
    assert_eq!(report.totals.provisional_estimates, 1);
    insta::assert_snapshot!(render_report(&report, false));
}
