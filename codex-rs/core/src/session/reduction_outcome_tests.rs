use super::*;
use crate::shake::ShakeFailure;
use crate::shake::ShakeOutcome;
use crate::shake::ShakeSkipReason;
use codex_protocol::protocol::ShakeMode;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::task::Context as TaskContext;
use std::task::Waker;
use test_case::test_case;

async fn manual_shake(
    session: &Arc<Session>,
    turn: &TurnContext,
    mode: ShakeMode,
    expected_fingerprint: Option<String>,
) -> ShakeOutcome {
    handlers::apply_shake(
        session,
        turn,
        mode,
        expected_fingerprint,
        handlers::ShakeTrigger::Manual,
    )
    .await
}

#[test_case(ShakeMode::Images; "images")]
#[test_case(ShakeMode::Thinking; "thinking")]
#[tokio::test]
async fn shake_outcome_distinguishes_applied_from_noop(mode: ShakeMode) {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    session.state.lock().await.history.replace(vec![
        serde_json::from_value(json!({
            "type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "keep this"},
                {"type": "input_image", "image_url": "https://example.invalid/image.png"}
            ]
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "type": "reasoning", "summary": [], "encrypted_content": "thinking"
        }))
        .unwrap(),
    ]);
    let outcome = manual_shake(&session, &turn, mode, /*expected_fingerprint*/ None).await;
    let ShakeOutcome::Applied(result) = outcome else {
        panic!("expected applied: {outcome:?}")
    };
    assert_eq!(
        (result.images_dropped, result.thinking_dropped),
        match mode {
            ShakeMode::Images => (1, 0),
            ShakeMode::Thinking => (0, 1),
            ShakeMode::Elide => unreachable!(),
        }
    );
    assert_eq!(
        manual_shake(&session, &turn, mode, /*expected_fingerprint*/ None,).await,
        ShakeOutcome::Noop
    );
}

#[tokio::test]
async fn shake_outcome_distinguishes_stale_invalid_and_ephemeral() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    assert_eq!(
        manual_shake(&session, &turn, ShakeMode::Elide, Some("stale".to_string()),).await,
        ShakeOutcome::Stale
    );
    let (ephemeral, ephemeral_turn, _ephemeral_events) =
        make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("test"),
            Vec::new(),
            |config| config.ephemeral = true,
        )
        .await;
    assert_eq!(
        manual_shake(
            &ephemeral,
            &ephemeral_turn,
            ShakeMode::Elide,
            /*expected_fingerprint*/ None,
        )
        .await,
        ShakeOutcome::Skipped(ShakeSkipReason::EphemeralThread)
    );
    let mut invalid = session.clone_history().await.shake_history_state().clone();
    invalid.sealed_prefix_digest = "invalid".to_string();
    session
        .state
        .lock()
        .await
        .history
        .restore_shake_history_state(Some(&invalid));
    assert_eq!(
        manual_shake(
            &session,
            &turn,
            ShakeMode::Images,
            /*expected_fingerprint*/ None,
        )
        .await,
        ShakeOutcome::Failed(ShakeFailure::InvalidSeal)
    );
}

#[tokio::test]
async fn shake_outcome_reports_a_refused_replacement() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let tail = assistant_message(&"protected tail ".repeat(/*n*/ 2_000));
    let apply = manual_shake(
        &session,
        &turn,
        ShakeMode::Elide,
        /*expected_fingerprint*/ None,
    );
    tokio::pin!(apply);
    // Queue a mutation immediately after the history clone. The FIFO state mutex
    // pauses apply_shake at artifact-store lookup while a different prefix is sealed.
    let mutation = session.state.lock();
    tokio::pin!(mutation);
    {
        let mut held = session.state.lock().await;
        held.history
            .replace(vec![user_message("original"), tail.clone()]);
        let mut context = TaskContext::from_waker(Waker::noop());
        assert!(apply.as_mut().poll(&mut context).is_pending());
        assert!(mutation.as_mut().poll(&mut context).is_pending());
    }
    {
        let mut context = TaskContext::from_waker(Waker::noop());
        assert!(apply.as_mut().poll(&mut context).is_pending());
    }
    let expected = {
        let mut state = mutation.await;
        state.history.replace(vec![user_message("changed"), tail]);
        let changed = state.history.annotated_items().to_vec();
        state
            .history
            .replace_shaken(changed, /*watermark_index*/ 1)
            .unwrap();
        state.history.annotated_items().to_vec()
    };
    assert_eq!(
        apply.await,
        ShakeOutcome::Failed(ShakeFailure::ReplacementRefused)
    );
    assert_eq!(session.clone_history().await.annotated_items(), expected);
}

#[test_case(false, CompactionTrigger::Manual; "local manual")]
#[test_case(false, CompactionTrigger::Auto; "local auto")]
#[test_case(true, CompactionTrigger::Manual; "remote manual")]
#[test_case(true, CompactionTrigger::Auto; "remote auto")]
#[tokio::test]
async fn inline_compaction_forwards_trigger_and_reason(remote: bool, trigger: CompactionTrigger) {
    let server = responses::start_mock_server().await;
    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    if !remote {
        provider.name = "local".to_string();
    }
    let auth = if remote {
        CodexAuth::create_dummy_chatgpt_auth_for_testing()
    } else {
        CodexAuth::from_api_key("test")
    };
    let (session, turn, _events) =
        make_session_and_context_with_auth_and_config_and_rx(auth, Vec::new(), move |config| {
            config.model_provider = provider;
            config.model = Some("gpt-5.2".to_string());
            let _ = config.features.disable(Feature::TokenBudget);
        })
        .await;
    let response = if remote {
        json!({
            "type": "response.output_item.done",
            "item": {"type": "compaction", "encrypted_content": "summary"}
        })
    } else {
        responses::ev_assistant_message("summary", "compacted")
    };
    let requests = responses::mount_sse_once(
        &server,
        responses::sse(vec![response, responses::ev_completed("compact")]),
    )
    .await;
    let step = session
        .capture_step_context(turn, &CancellationToken::new())
        .await
        .unwrap();
    let reason = match trigger {
        CompactionTrigger::Manual => CompactionReason::UserRequested,
        CompactionTrigger::Auto => CompactionReason::ContextLimit,
    };
    super::super::turn::run_inline_compact(
        &session,
        step,
        /*fallback_step_context*/ None,
        &mut session.services.model_client.new_session(),
        InitialContextInjection::DoNotInject,
        CompactionInvocation {
            trigger,
            reason,
            phase: CompactionPhase::MidTurn,
        },
    )
    .await
    .unwrap();
    let metadata: Value = serde_json::from_str(
        &requests
            .single_request()
            .header("x-codex-turn-metadata")
            .expect("compaction metadata"),
    )
    .unwrap();
    assert_eq!(
        metadata["compaction"],
        json!({
            "trigger": trigger, "reason": reason,
            "implementation": if remote { "responses_compaction_v2" } else { "responses" },
            "phase": "mid_turn", "strategy": "memento"
        })
    );
}

#[test_case(CompactionTrigger::Manual; "manual")]
#[test_case(CompactionTrigger::Auto; "auto")]
#[tokio::test]
async fn inline_token_budget_compaction_forwards_hook_trigger(trigger: CompactionTrigger) {
    let home = tempfile::tempdir().unwrap();
    let hook = home.path().join("compact.py");
    let log = home.path().join("trigger.json");
    std::fs::write(&hook, format!(
        "import json, sys\nfrom pathlib import Path\nPath({:?}).write_text(json.dumps(json.load(sys.stdin)))\n",
        log.to_string_lossy()
    )).unwrap();
    let python = if cfg!(windows) { "python" } else { "python3" };
    std::fs::write(
        home.path().join("hooks.json"),
        json!({ "hooks": { "PreCompact": [{
        "hooks": [{ "type": "command", "command": format!("{python} \"{}\"", hook.display()) }]
    }] } })
        .to_string(),
    )
    .unwrap();
    let (session, turn, _events) = make_session_and_context_with_auth_config_home_and_rx(
        CodexAuth::from_api_key("test"),
        Vec::new(),
        home.path(),
        SessionSource::Exec,
        |config| {
            config.features.enable(Feature::TokenBudget).unwrap();
        },
    )
    .await;
    let (hooks, _hook_events) = Hooks::new(
        HooksConfig {
            feature_enabled: true,
            bypass_hook_trust: true,
            config_layer_stack: Some(turn.config.config_layer_stack.clone()),
            ..HooksConfig::default()
        },
        session.thread_id,
        Arc::new(CoreHookMcpExecutor {
            runtime: Arc::clone(&session.services.mcp_runtime),
            thread_id: session.thread_id,
        }),
    )
    .unwrap();
    session.services.hooks.store(Arc::new(hooks));
    let step = session
        .capture_step_context(turn, &CancellationToken::new())
        .await
        .unwrap();
    super::super::turn::run_inline_compact(
        &session,
        step,
        /*fallback_step_context*/ None,
        &mut session.services.model_client.new_session(),
        InitialContextInjection::DoNotInject,
        CompactionInvocation {
            trigger,
            reason: match trigger {
                CompactionTrigger::Manual => CompactionReason::UserRequested,
                CompactionTrigger::Auto => CompactionReason::ContextLimit,
            },
            phase: CompactionPhase::MidTurn,
        },
    )
    .await
    .unwrap();
    let input: Value = serde_json::from_str(&std::fs::read_to_string(log).unwrap()).unwrap();
    assert_eq!(input["trigger"], serde_json::to_value(trigger).unwrap());
}
