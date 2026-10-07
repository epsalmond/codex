use super::*;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn sparse_overlay_model_default_and_reset_resolve_against_catalog() {
    let (_, turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.model_auto_compact_token_limit = Some(777);
    let mut model = (**turn.model_info()).clone();
    model.slug = "gpt-5.6-sol".into();
    model.context_window = Some(10000);
    model.max_context_window = Some(10000);
    model.auto_compact_token_limit = Some(8000);
    let state = ContextSettingsState::default();
    let baseline = resolve(&config, &state, &model, ContextSettingsTarget::Root).1;
    for threshold in [
        ShakeThreshold::Off,
        ShakeThreshold::Percent { percent: 61 },
        ShakeThreshold::Tokens { tokens: 2300 },
    ] {
        let next = {
            let mut next = state.clone();
            merge(
                &mut next.overrides,
                &ContextSettingsOverrides {
                    shake_threshold: Some(threshold),
                    compaction_threshold: Some(CompactionThreshold::ModelDefault),
                    ..Default::default()
                },
            );
            next
        };
        let (resolved_model, view) = resolve(&config, &next, &model, ContextSettingsTarget::Root);
        let mut expected = baseline.clone();
        expected.requested = next.overrides.clone();
        expected.effective.shake_threshold = threshold;
        expected.effective.compaction_threshold = CompactionThreshold::ModelDefault;
        expected.compaction_scope_token_limit = Some(8000);
        expected.shake_threshold_tokens = match threshold {
            ShakeThreshold::Off => None,
            ShakeThreshold::Percent { percent } => Some(10000 * percent / 100),
            ShakeThreshold::Tokens { tokens } => Some(tokens),
        };
        assert_eq!(view, expected);
        assert_eq!(resolved_model.auto_compact_token_limit, Some(8000));
        let reset = {
            let mut next = next.clone();
            next.overrides = ContextSettingsOverrides::default();
            next
        };
        assert_eq!(
            resolve(&config, &reset, &model, ContextSettingsTarget::Root).1,
            baseline
        );
    }
    model.context_window = None;
    model.max_context_window = None;
    let percent_state = {
        let mut next = state;
        merge(
            &mut next.overrides,
            &ContextSettingsOverrides {
                shake_threshold: Some(ShakeThreshold::Percent { percent: 61 }),
                ..Default::default()
            },
        );
        next
    };
    assert_eq!(
        resolve(&config, &percent_state, &model, ContextSettingsTarget::Root)
            .1
            .shake_threshold_tokens,
        None
    );
}

#[tokio::test]
async fn compaction_scope_keeps_body_limit_distinct_from_full_admission() {
    use codex_protocol::config_types::AutoCompactTokenLimitScope;
    let (_, turn) = make_session_and_context().await;
    let config = &turn.config;
    let mut model = (**turn.model_info()).clone();
    model.context_window = Some(10000);
    model.max_context_window = Some(10000);
    model.effective_context_window_percent = 95;
    let mut state = ContextSettingsState::default();
    for (scope, limit) in [
        (AutoCompactTokenLimitScope::Total, 9000),
        (AutoCompactTokenLimitScope::BodyAfterPrefix, 12000),
    ] {
        state = {
            let mut next = state.clone();
            merge(
                &mut next.overrides,
                &ContextSettingsOverrides {
                    compaction_threshold: Some(CompactionThreshold::Tokens { tokens: 12000 }),
                    compaction_scope: Some(scope),
                    ..Default::default()
                },
            );
            next
        };
        let (resolved, view) = resolve(config, &state, &model, ContextSettingsTarget::Root);
        assert_eq!(
            (
                view.compaction_scope_token_limit,
                view.usable_context_window
            ),
            (Some(limit), Some(9500))
        );
        assert_eq!(resolved.usable_context_window(), view.usable_context_window);
    }
}

#[tokio::test]
async fn future_turn_projects_context_choices_while_captured_turn_stays_immutable() {
    let (session, _) = make_session_and_context().await;
    let initial = session.new_default_turn().await;
    let before = initial.initial_settings.context_settings.clone();
    session
        .update_settings(super::super::session::SessionSettingsUpdate {
            restored_context_settings: Some(ContextSettingsState {
                overrides: ContextSettingsOverrides {
                    shake_threshold: Some(ShakeThreshold::Off),
                    compaction_threshold: Some(CompactionThreshold::Tokens { tokens: 1111 }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let next = session.new_default_turn().await;
    assert_eq!(initial.initial_settings.context_settings, before);
    assert_eq!(
        next.config
            .auto_shake
            .settings_for_model(&next.model_info().slug)
            .enabled,
        false
    );
    assert_eq!(next.model_info().auto_compact_token_limit(), Some(1111));
    assert_eq!(
        next.initial_settings
            .context_settings
            .as_ref()
            .unwrap()
            .model,
        next.model_info().slug
    );
    session
        .update_settings(super::super::session::SessionSettingsUpdate {
            restored_context_settings: Some(ContextSettingsState::default()),
            step_settings: crate::session::step_settings::StepSettingsUpdate {
                model: Some("gpt-6-astra".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let switched = session.new_default_turn().await;
    let view = switched.initial_settings.context_settings.as_ref().unwrap();
    assert_eq!(
        (view.model.as_str(), view.effective.shake_threshold),
        ("gpt-6-astra", ShakeThreshold::Percent { percent: 40 })
    );
    assert_eq!(
        view.usable_context_window,
        switched.model_info().usable_context_window()
    );
    assert_eq!(initial.initial_settings.context_settings, before);
}

#[test]
fn sparse_numeric_validation_rejects_out_of_range_fields() {
    for overrides in [
        ContextSettingsOverrides {
            shake_threshold: Some(ShakeThreshold::Percent { percent: 101 }),
            ..Default::default()
        },
        ContextSettingsOverrides {
            shake_threshold: Some(ShakeThreshold::Tokens { tokens: 0 }),
            ..Default::default()
        },
        ContextSettingsOverrides {
            shake_min_elidable_percent: Some(-1),
            ..Default::default()
        },
        ContextSettingsOverrides {
            shake_min_savings_tokens: Some(-1),
            ..Default::default()
        },
        ContextSettingsOverrides {
            compaction_threshold: Some(CompactionThreshold::Tokens { tokens: 0 }),
            ..Default::default()
        },
        ContextSettingsOverrides {
            child_reduction_threshold_tokens: Some(0),
            ..Default::default()
        },
        ContextSettingsOverrides {
            post_turn_compaction_percent: Some(101),
            ..Default::default()
        },
    ] {
        assert!(
            validate_state(&ContextSettingsState {
                overrides,
                ..Default::default()
            })
            .is_err()
        );
    }
}

#[tokio::test]
async fn retained_child_inputs_survive_nested_projection_and_changed_reload_config() {
    let (_, turn) = make_session_and_context().await;
    for threshold in [
        None,
        Some(codex_config::config_toml::AutoShakeThresholdToml::Off),
        Some(codex_config::config_toml::AutoShakeThresholdToml::Percent(
            70,
        )),
        Some(codex_config::config_toml::AutoShakeThresholdToml::Tokens(
            4000,
        )),
    ] {
        let mut config = (*turn.config).clone();
        config.model_auto_compact_token_limit = Some(9000);
        config.auto_shake.threshold = threshold;
        config.auto_shake.max_threshold_tokens = Some(8500);
        config.subagent_context_reduction.threshold_tokens = 3000;
        inherit(&mut config, &ContextSettingsState::default());
        let saved = config.context_settings.clone().unwrap();
        let mut encoded = serde_json::to_value(&saved).unwrap();
        encoded.as_object_mut().unwrap().remove("overrides");
        let basis = encoded["inherited"].as_object_mut().unwrap();
        basis.remove("overrides");
        if threshold.is_none() {
            basis.remove("shake_threshold");
        }
        let reloaded: ContextSettingsState = serde_json::from_value(encoded).unwrap();
        assert_eq!(reloaded, saved);
        let mut reload = (*turn.config).clone();
        reload.model_auto_compact_token_limit = Some(1000);
        reload.auto_shake.threshold = Some(
            codex_config::config_toml::AutoShakeThresholdToml::Percent(33),
        );
        project(&mut reload, &reloaded, ContextSettingsTarget::Child);
        assert_eq!(
            (
                reload.model_auto_compact_token_limit,
                reload.auto_shake.max_threshold_tokens,
                reload.auto_shake.threshold,
            ),
            (Some(3000), Some(3000), threshold)
        );
        let mut raised = reloaded;
        raised
            .inherited
            .as_mut()
            .unwrap()
            .child_reduction_threshold_tokens = 7000;
        inherit(&mut reload, &raised);
        let mut nested = reload.context_settings.clone().unwrap();
        project(&mut reload, &nested, ContextSettingsTarget::Child);
        assert_eq!(reload.model_auto_compact_token_limit, Some(7000));
        nested.inherited.as_mut().unwrap().child_reduction_enabled = false;
        project(&mut reload, &nested, ContextSettingsTarget::Child);
        assert_eq!(
            (
                reload.model_auto_compact_token_limit,
                reload.auto_shake.max_threshold_tokens,
                reload.auto_shake.threshold,
            ),
            (Some(9000), Some(8500), threshold)
        );
        project(&mut reload, &saved, ContextSettingsTarget::Root);
        assert_eq!(reload.model_auto_compact_token_limit, Some(9000));
    }
}
