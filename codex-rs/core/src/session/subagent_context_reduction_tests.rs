use super::*;
use crate::agent::context_policy::PolicyProvenance;

fn policy(enabled: bool, threshold_tokens: i64) -> ResolvedSubagentContextPolicy {
    ResolvedSubagentContextPolicy {
        enabled,
        threshold_tokens,
        check_after_tools: true,
        shake: SubagentContextReductionShake::Inherit,
        on_failure: SubagentContextReductionOnFailure::Continue,
        enabled_from: PolicyProvenance::Config,
        threshold_tokens_from: PolicyProvenance::Config,
        check_after_tools_from: PolicyProvenance::Config,
        shake_from: PolicyProvenance::Config,
        on_failure_from: PolicyProvenance::Config,
    }
}

#[test]
fn continued_failure_retries_only_after_four_thousand_new_tokens() {
    let suppression = SuppressedContextReduction {
        after_tokens: 12_000,
        policy_revision: 4,
    };

    assert_eq!(
        suppression_decision(&suppression, 4, 15_999, 8_000),
        SuppressionDecision::Suppress
    );
    assert_eq!(
        suppression_decision(&suppression, 4, 16_000, 8_000),
        SuppressionDecision::Growth
    );
}

#[test]
fn recovery_and_policy_changes_rearm_the_failure_episode() {
    let suppression = SuppressedContextReduction {
        after_tokens: 12_000,
        policy_revision: 4,
    };

    assert_eq!(
        suppression_decision(&suppression, 4, 7_999, 8_000),
        SuppressionDecision::Recovered
    );
    assert_eq!(
        suppression_decision(&suppression, 5, 12_000, 8_000),
        SuppressionDecision::PolicyChanged
    );
}

#[test]
fn remeasured_model_limits_and_explicit_resets_remain_authoritative() {
    let enabled = policy(true, 272_000);
    let disabled = policy(false, 272_000);

    // A pending model-limit rollover only requests a shake. If remeasurement shows
    // that it cleared both the model guard and child threshold, no compaction follows.
    assert!(!should_compact_after_shake(
        /*explicit_context_reset*/ false, /*model_token_limit_reached*/ false,
        /*active_context_tokens*/ 50_000, &enabled,
    ));
    assert!(should_compact_after_shake(
        /*explicit_context_reset*/ false, /*model_token_limit_reached*/ true,
        /*active_context_tokens*/ 50_000, &enabled,
    ));
    assert!(should_compact_after_shake(
        /*explicit_context_reset*/ true, /*model_token_limit_reached*/ false,
        /*active_context_tokens*/ 50_000, &enabled,
    ));
    assert!(!should_compact_after_shake(
        /*explicit_context_reset*/ false, /*model_token_limit_reached*/ false,
        /*active_context_tokens*/ 300_000, &disabled,
    ));
}
