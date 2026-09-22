use super::*;

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
