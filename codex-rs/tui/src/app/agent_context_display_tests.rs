use super::*;
use codex_app_server_protocol::ThreadContextReduction;
use pretty_assertions::assert_eq;

fn snapshot() -> ThreadContextUsage {
    ThreadContextUsage {
        active_tokens: 80_000,
        basis: ThreadContextTokenBasis::Usage,
        last_reduction: None,
        selected_model: Some("gpt-6-luna".to_string()),
        child_policy_enabled: Some(/*value*/ true),
        child_active_cap_tokens: Some(/*value*/ 100_000),
        model_window_tokens: Some(/*value*/ 272_000),
        observed_at: Some(/*value*/ 999_990),
        provider_usage_at: Some(/*value*/ 999_980),
        shake_watermark: Some(/*value*/ 42),
    }
}

#[test]
fn context_snapshots_keep_original_freshness_and_unknowns() {
    let mut context = snapshot();
    let mut rows = vec![snapshot_description(&context, /*now*/ 1_000_000)];
    rows.push(snapshot_description(&context, /*now*/ 1_007_200));
    context.basis = ThreadContextTokenBasis::Estimate;
    context.provider_usage_at = None;
    context.active_tokens = 0;
    rows.push(snapshot_description(&context, /*now*/ 1_000_000));
    context.selected_model = None;
    context.observed_at = None;
    context.child_policy_enabled = None;
    context.model_window_tokens = None;
    context.active_tokens = -1;
    rows.push(snapshot_description(&context, /*now*/ 1_000_000));
    context.child_policy_enabled = Some(/*value*/ false);
    context.observed_at = Some(/*value*/ 1_000_001);
    rows.push(snapshot_description(&context, /*now*/ 1_000_000));
    insta::assert_snapshot!("child_context_freshness", rows.join("\n"));
}

#[test]
fn reduction_outcomes_display_unknown_post_size_and_observed_zero() {
    let mut context = snapshot();
    let rows: Vec<_> = [
        (
            ThreadContextReductionOutcome::Shaken,
            Some(/*value*/ 40_000),
        ),
        (ThreadContextReductionOutcome::Compacted, Some(/*value*/ 0)),
        (
            ThreadContextReductionOutcome::Insufficient,
            Some(/*value*/ 120_000),
        ),
        (ThreadContextReductionOutcome::Failed, None),
        (ThreadContextReductionOutcome::Cancelled, None),
    ]
    .into_iter()
    .map(|(outcome, after_tokens)| {
        context.last_reduction = Some(ThreadContextReduction {
            completed_at: 999_940,
            before_tokens: 120_000,
            after_tokens,
            outcome,
        });
        snapshot_description(&context, /*now*/ 1_000_000)
    })
    .collect();
    insta::assert_snapshot!("child_context_reductions", rows.join("\n"));
}

#[test]
fn model_text_is_bounded_and_invalid_counts_remain_unknown() {
    let mut context = snapshot();
    context.selected_model = Some("界".repeat(/*n*/ 1000));
    context.model_window_tokens = Some(/*value*/ 0);
    context.child_active_cap_tokens = Some(/*value*/ -1);
    assert_eq!(
        snapshot_description(&context, /*now*/ 1_000_000),
        format!(
            "context 80000 · cap ? · window ? · {} · usage + estimate · snapshot 10s ago · provider 20s ago",
            truncate_text(&"界".repeat(/*n*/ 1000), /*max_graphemes*/ 64)
        )
    );
}
