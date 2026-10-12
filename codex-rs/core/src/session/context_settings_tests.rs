use super::*;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;
use std::sync::Arc;

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

#[tokio::test]
async fn review_retains_captured_context_selection_and_model_limit() {
    let (session, mut parent) = make_session_and_context().await;
    let mut reload_config = (*parent.config).clone();
    reload_config.model_auto_compact_token_limit = Some(80_000);
    let mut captured_config = reload_config.clone();
    captured_config.model_auto_compact_token_limit = Some(30_000);
    captured_config.model_auto_compact_token_limit_scope =
        codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
    let selection = ContextSettingsState {
        inherited: Some(configured_baseline(&captured_config)),
    };
    Arc::make_mut(&mut parent.initial_settings).context_selection = selection.clone();
    parent.config = Arc::new(captured_config);
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
        (selection, Some(30_000), Some(30_000))
    );
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
}
