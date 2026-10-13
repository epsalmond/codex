use super::*;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;
use std::sync::Arc;

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
        let next = apply(
            &state,
            &ContextSettingsUpdate::Patch {
                overrides: ContextSettingsOverrides {
                    shake_threshold: Some(threshold),
                    compaction_threshold: Some(CompactionThreshold::ModelDefault),
                    ..Default::default()
                },
            },
        )
        .unwrap();
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
        let reset = apply(&next, &ContextSettingsUpdate::Reset).unwrap();
        assert_eq!(
            resolve(&config, &reset, &model, ContextSettingsTarget::Root).1,
            baseline
        );
    }
    model.context_window = None;
    model.max_context_window = None;
    let percent_state = apply(
        &state,
        &ContextSettingsUpdate::Patch {
            overrides: ContextSettingsOverrides {
                shake_threshold: Some(ShakeThreshold::Percent { percent: 61 }),
                ..Default::default()
            },
        },
    )
    .unwrap();
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
        state = apply(
            &state,
            &ContextSettingsUpdate::Patch {
                overrides: ContextSettingsOverrides {
                    compaction_threshold: Some(CompactionThreshold::Tokens { tokens: 12000 }),
                    compaction_scope: Some(scope),
                    ..Default::default()
                },
            },
        )
        .unwrap();
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
async fn child_clamp_can_raise_disable_reset_and_reload_without_becoming_ancestor_limit() {
    let (_, turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.model_auto_compact_token_limit = Some(9000);
    config.auto_shake.max_threshold_tokens = Some(8500);
    config.subagent_context_reduction.threshold_tokens = 3000;
    let mut model = (**turn.model_info()).clone();
    model.context_window = Some(20000);
    model.max_context_window = Some(20000);
    inherit(&mut config, &ContextSettingsState::default());
    let child = config.context_settings.clone().unwrap();
    // A root fork keeps the saved descendant default without inheriting an active cap.
    let mut root_fork = config.clone();
    project(&mut root_fork, &child, ContextSettingsTarget::Root);
    let view = resolve(&root_fork, &child, &model, ContextSettingsTarget::Root).1;
    assert_eq!(
        (
            root_fork.model_auto_compact_token_limit,
            view.compaction_scope_token_limit,
            view.inherited_child_default_tokens,
            view.inherited_child_active_cap,
        ),
        (Some(9000), Some(9000), None, None)
    );

    assert_eq!(
        resolve(&config, &child, &model, ContextSettingsTarget::Child)
            .1
            .compaction_scope_token_limit,
        Some(3000)
    );
    model.auto_compact_token_limit = Some(2000);
    let model_default = apply(
        &child,
        &ContextSettingsUpdate::Patch {
            overrides: ContextSettingsOverrides {
                compaction_threshold: Some(CompactionThreshold::ModelDefault),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(
        resolve(
            &config,
            &model_default,
            &model,
            ContextSettingsTarget::Child
        )
        .1
        .compaction_scope_token_limit,
        Some(2000)
    );
    let raised = apply(
        &child,
        &ContextSettingsUpdate::Patch {
            overrides: ContextSettingsOverrides {
                child_reduction_threshold_tokens: Some(7000),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(
        resolve(&config, &raised, &model, ContextSettingsTarget::Child)
            .1
            .compaction_scope_token_limit,
        Some(7000)
    );
    // Nesting preserves the original compaction and internal Shake caps.
    inherit(&mut config, &raised);
    let nested = config.context_settings.clone().unwrap();
    let disabled = apply(
        &nested,
        &ContextSettingsUpdate::Patch {
            overrides: ContextSettingsOverrides {
                child_reduction_enabled: Some(false),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let reloaded: ContextSettingsState =
        serde_json::from_value(serde_json::to_value(&disabled).unwrap()).unwrap();
    let view = resolve(&config, &reloaded, &model, ContextSettingsTarget::Child).1;
    assert_eq!(
        (
            view.compaction_scope_token_limit,
            view.inherited_child_active_cap
        ),
        (Some(9000), None)
    );
    let reset = apply(&disabled, &ContextSettingsUpdate::Reset).unwrap();
    assert_eq!(
        resolve(&config, &reset, &model, ContextSettingsTarget::Child)
            .1
            .compaction_scope_token_limit,
        Some(7000)
    );
    model.context_window = Some(5000);
    model.max_context_window = Some(5000);
    assert_eq!(
        resolve(&config, &reset, &model, ContextSettingsTarget::Child)
            .1
            .inherited_child_active_cap,
        model.usable_context_window()
    );
}

#[tokio::test]
async fn future_turn_projects_context_choices_while_captured_turn_stays_immutable() {
    let (session, _) = make_session_and_context().await;
    {
        let mut state = session.state.lock().await;
        let config = Arc::make_mut(&mut state.session_configuration.original_config_do_not_use);
        config.model_auto_compact_token_limit = Some(777);
    }
    let initial = session.new_default_turn().await;
    let before = initial.initial_settings.context_settings.clone();
    session
        .update_settings(super::super::session::SessionSettingsUpdate {
            context_settings: Some(ContextSettingsUpdate::Patch {
                overrides: ContextSettingsOverrides {
                    shake_threshold: Some(ShakeThreshold::Off),
                    compaction_threshold: Some(CompactionThreshold::Tokens { tokens: 1111 }),
                    ..Default::default()
                },
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
    let destination = next
        .with_model("gpt-6-astra".into(), &session.services.models_manager)
        .await;
    let view = destination
        .initial_settings
        .context_settings
        .as_ref()
        .unwrap();
    assert_eq!(
        view.baseline.compaction_threshold,
        CompactionThreshold::Tokens { tokens: 777 }
    );
    let next_view = next.initial_settings.context_settings.as_ref().unwrap();
    assert_eq!(next_view.model, next.model_info().slug);
    session
        .update_settings(super::super::session::SessionSettingsUpdate {
            context_settings: Some(ContextSettingsUpdate::Reset),
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
    let patches: [fn(&mut ContextSettingsOverrides); 7] = [
        |value| value.shake_threshold = Some(ShakeThreshold::Percent { percent: 101 }),
        |value| value.shake_threshold = Some(ShakeThreshold::Tokens { tokens: 0 }),
        |value| value.shake_min_elidable_percent = Some(-1),
        |value| value.shake_min_savings_tokens = Some(-1),
        |value| value.compaction_threshold = Some(CompactionThreshold::Tokens { tokens: 0 }),
        |value| value.child_reduction_threshold_tokens = Some(0),
        |value| value.post_turn_compaction_percent = Some(101),
    ];
    for patch in patches {
        let mut overrides = ContextSettingsOverrides::default();
        patch(&mut overrides);
        assert!(
            apply(
                &ContextSettingsState::default(),
                &ContextSettingsUpdate::Patch { overrides }
            )
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

#[test_case::test_case(None; "saved_tokens")]
#[test_case::test_case(Some(CompactionThreshold::ModelDefault); "destination_model_default")]
#[tokio::test]
async fn review_retains_captured_context_selection_and_model_limit(
    threshold: Option<CompactionThreshold>,
) {
    let (session, mut parent) = make_session_and_context().await;
    let mut reload_config = (*parent.config).clone();
    reload_config.model_auto_compact_token_limit = Some(80_000);
    reload_config.review_model = Some("gpt-6-astra".into());
    let mut model_config = reload_config.to_models_manager_config();
    model_config.model_auto_compact_token_limit = None;
    let catalog_model = session
        .services
        .models_manager
        .get_model_info("gpt-6-astra", &model_config)
        .await;
    let expected_limit = if threshold.is_some() {
        catalog_model.auto_compact_token_limit
    } else {
        Some(30_000)
    };
    let mut captured_config = reload_config.clone();
    captured_config.model_auto_compact_token_limit = Some(30_000);
    captured_config.model_auto_compact_token_limit_scope =
        codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
    let selection = ContextSettingsState {
        inherited: Some(configured_baseline(&captured_config)),
        overrides: ContextSettingsOverrides {
            compaction_threshold: threshold,
            ..Default::default()
        },
    };
    let expected_view = resolve(
        &captured_config,
        &selection,
        &catalog_model,
        ContextSettingsTarget::Root,
    )
    .1;
    Arc::make_mut(&mut parent.initial_settings).context_selection = selection.clone();
    parent.config = Arc::new(captured_config);
    let switched = parent
        .with_model("gpt-6-astra".into(), &session.services.models_manager)
        .await;
    assert_eq!(
        switched.model_info().auto_compact_token_limit,
        expected_limit
    );
    assert_eq!(
        switched.initial_settings.context_settings,
        Some(Arc::new(expected_view.clone()))
    );
    let session = Arc::new(session);
    crate::session::review::spawn_review_thread(
        Arc::clone(&session),
        Arc::new(reload_config),
        Arc::new(parent),
        "saved-context-review".into(),
        crate::review_prompts::ResolvedReviewRequest {
            target: codex_protocol::protocol::ReviewTarget::Custom {
                instructions: "Inspect saved context.".into(),
            },
            prompt: "Inspect saved context.".into(),
            user_facing_hint: "Inspect saved context.".into(),
        },
    )
    .await;
    let captured = {
        let active = session.active_turn.lock().await;
        let task = active.as_ref().unwrap().task.as_ref().unwrap();
        Arc::clone(&task.turn_context)
    };
    assert_eq!(
        (
            captured.initial_settings.context_selection.clone(),
            captured.config.model_auto_compact_token_limit,
            captured.model_info().auto_compact_token_limit,
        ),
        (
            selection,
            threshold.is_none().then_some(30_000),
            expected_limit
        )
    );
    assert_eq!(
        captured.initial_settings.context_settings,
        Some(Arc::new(expected_view))
    );
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
}
