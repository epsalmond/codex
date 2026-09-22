use std::fmt::Write as _;

use super::Report;

pub(super) fn print_report(report: &Report, color: bool) {
    print!("{}", render_report(report, color));
}

pub(super) fn render_report(report: &Report, color: bool) -> String {
    let mut output = String::new();
    let _ = writeln!(
        output,
        "{}",
        paint(
            &format!("Shake stats since {}", report.since),
            "1;36",
            color
        )
    );
    let _ = writeln!(output, "  shakes: {}", report.totals.shakes);
    let _ = writeln!(
        output,
        "  tokens shaken out: {}",
        paint(&report.totals.tokens_freed.to_string(), "1;32", color)
    );
    let _ = writeln!(
        output,
        "  cache reads: {} tokens",
        paint(&report.totals.cache_read_tokens.to_string(), "1;34", color)
    );
    let _ = writeln!(
        output,
        "  cache rewrite burn proxy: {} tokens across {} observed following prompts",
        paint(
            &report.totals.cache_rewrite_burn_tokens.to_string(),
            "1;31",
            color
        ),
        report.totals.cache_rewrite_requests
    );
    let _ = writeln!(
        output,
        "  observed quota usage: {} total tokens",
        paint(&report.totals.quota_usage_tokens.to_string(), "1;35", color)
    );
    let _ = writeln!(
        output,
        "  likely saved in following prompts: {} tokens across {} prompts",
        paint(
            &report.totals.likely_saved_tokens.to_string(),
            "1;32",
            color
        ),
        report.totals.following_prompts_observed
    );
    let _ = writeln!(
        output,
        "  net estimated input impact: {} tokens",
        paint(
            &report.totals.net_estimated_tokens.to_string(),
            "1;33",
            color
        )
    );
    let _ = writeln!(
        output,
        "  incremental rewrite estimate: {} tokens",
        paint(
            &report.totals.incremental_cache_rewrite_tokens.to_string(),
            "1;31",
            color,
        )
    );
    let _ = writeln!(
        output,
        "  cache states: warm={} likely_expired={} unknown={}",
        report.totals.cache_state_counts.warm,
        report.totals.cache_state_counts.likely_expired,
        report.totals.cache_state_counts.unknown,
    );
    if report.totals.provisional_estimates > 0 {
        let _ = writeln!(
            output,
            "  provisional estimates: {} (cache state unknown)",
            report.totals.provisional_estimates
        );
    }
    let _ = writeln!(output, "\n{}", paint("By model", "1;36", color));
    for model in &report.models {
        let _ = writeln!(
            output,
            "  {:<20} shakes={:<3} freed={:<9} reads={:<9} rewrite={:<9} incremental={:<9} saved={:<9} net={} states={}/{}/{} provisional={}",
            model.model,
            model.shakes,
            paint(&model.tokens_freed.to_string(), "32", color),
            paint(&model.cache_read_tokens.to_string(), "34", color),
            paint(&model.cache_rewrite_burn_tokens.to_string(), "31", color),
            paint(
                &model.incremental_cache_rewrite_tokens.to_string(),
                "31",
                color,
            ),
            paint(&model.likely_saved_tokens.to_string(), "32", color),
            paint(&model.net_estimated_tokens.to_string(), "33", color),
            model.cache_state_counts.warm,
            model.cache_state_counts.likely_expired,
            model.cache_state_counts.unknown,
            model.provisional_estimates,
        );
    }
    let _ = writeln!(output, "\n{}", paint("Shakes by turn", "1;36", color));
    for shake in &report.shakes {
        let _ = writeln!(
            output,
            "  {} {} provider={} {} turn={} freed={} cache_state={} idle_secs={} ttl_secs={} ttl_source={} evidence={} confidence={} rewrite={} incremental={} saved={} net={}{}",
            shake.timestamp,
            paint(&shake.trigger, "33", color),
            shake.provider_id.as_deref().unwrap_or("unknown"),
            shake.model,
            shake.turn_id.as_deref().unwrap_or("unknown"),
            paint(&shake.tokens_freed.to_string(), "32", color),
            shake.cache_state,
            shake
                .measured_idle_secs
                .map_or_else(|| "unknown".to_string(), |idle| format!("{idle:.9}")),
            shake
                .cache_ttl_secs
                .map_or_else(|| "unknown".to_string(), |ttl| ttl.to_string()),
            shake.cache_ttl_source.as_deref().unwrap_or("unknown"),
            shake.evidence_source.as_deref().unwrap_or("unknown"),
            shake.evidence_confidence.as_deref().unwrap_or("unknown"),
            paint(
                &shake
                    .cache_rewrite_burn_tokens
                    .unwrap_or_default()
                    .to_string(),
                "31",
                color,
            ),
            paint(
                &shake.incremental_cache_rewrite_tokens.to_string(),
                "31",
                color,
            ),
            paint(&shake.likely_saved_tokens.to_string(), "32", color),
            paint(&shake.net_estimated_tokens.to_string(), "33", color),
            if shake.estimate_provisional {
                " (provisional)"
            } else {
                ""
            },
        );
        if let Some(reason) = shake.unknown_reason.as_deref() {
            let _ = writeln!(output, "    unknown reason: {reason}");
        }
        if let Some(previous) = shake.previous_sampling_timestamp.as_deref() {
            let _ = writeln!(
                output,
                "    previous sampling: {previous} (historical timestamp; lower confidence than runtime clock)"
            );
        }
    }
    let _ = writeln!(output, "\n  note: {}", report.estimate_note);
    output
}

fn paint(value: &str, code: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{code}m{value}\x1b[0m")
    } else {
        value.to_string()
    }
}
