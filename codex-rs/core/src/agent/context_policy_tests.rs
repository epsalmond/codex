use super::*;

#[test]
fn inherited_policy_and_child_only_override_stay_separate() {
    let mut parent = SubagentContextReductionPolicyState::default();
    update_local(
        &mut parent,
        SubagentContextReductionOverrides {
            threshold_tokens: Some(210_000),
            check_after_tools: Some(false),
            ..Default::default()
        },
        /*inherit_to_children*/ true,
    );

    let child = for_child(
        &parent,
        SubagentContextReductionOverrides {
            threshold_tokens: Some(180_000),
            ..Default::default()
        },
        /*inherit_to_children*/ false,
    );

    assert_eq!(child.inherited.threshold_tokens, Some(210_000));
    assert_eq!(child.inherited.check_after_tools, Some(false));
    assert_eq!(child.local.threshold_tokens, Some(180_000));
    assert_eq!(child.inheritable.threshold_tokens, None);
    assert_eq!(child.inheritable.check_after_tools, None);
    assert_eq!(child.desired_revision, 1);

    let descendant_defaults = inherited_by_children(&child);
    assert_eq!(descendant_defaults.threshold_tokens, Some(210_000));
    assert_eq!(descendant_defaults.check_after_tools, Some(false));
}

#[test]
fn local_non_inheritable_update_replaces_the_local_value_only() {
    let mut state = SubagentContextReductionPolicyState {
        inherited: SubagentContextReductionOverrides {
            threshold_tokens: Some(250_000),
            ..Default::default()
        },
        ..Default::default()
    };
    update_local(
        &mut state,
        SubagentContextReductionOverrides {
            threshold_tokens: Some(200_000),
            ..Default::default()
        },
        /*inherit_to_children*/ true,
    );
    update_local(
        &mut state,
        SubagentContextReductionOverrides {
            threshold_tokens: Some(180_000),
            ..Default::default()
        },
        /*inherit_to_children*/ false,
    );

    assert_eq!(state.local.threshold_tokens, Some(180_000));
    assert_eq!(state.inheritable.threshold_tokens, None);
    assert_eq!(state.inherited.threshold_tokens, Some(250_000));
    assert_eq!(
        inherited_by_children(&state).threshold_tokens,
        Some(250_000)
    );
    assert_eq!(state.desired_revision, 2);
}

#[test]
fn resetting_local_overrides_restores_inherited_values() {
    let mut state = SubagentContextReductionPolicyState {
        inherited: SubagentContextReductionOverrides {
            enabled: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    update_local(
        &mut state,
        SubagentContextReductionOverrides {
            enabled: Some(true),
            ..Default::default()
        },
        /*inherit_to_children*/ true,
    );
    reset_local(&mut state);

    assert_eq!(state.local, SubagentContextReductionOverrides::default());
    assert_eq!(
        state.inheritable,
        SubagentContextReductionOverrides::default()
    );
    assert_eq!(state.inherited.enabled, Some(false));
    assert_eq!(state.desired_revision, 2);
}

#[test]
fn warning_episode_dedupes_by_policy_revision_not_turn_id() {
    assert!(should_report_failure_episode(None, 7));
    assert!(!should_report_failure_episode(Some(7), 7));
    assert!(should_report_failure_episode(Some(6), 7));
}

#[test]
fn runtime_overrides_reject_nonpositive_thresholds() {
    for threshold_tokens in [0, -1] {
        assert_eq!(
            validate_overrides(&SubagentContextReductionOverrides {
                threshold_tokens: Some(threshold_tokens),
                ..Default::default()
            }),
            Err("subagent context threshold_tokens must be positive".to_string())
        );
    }
}
