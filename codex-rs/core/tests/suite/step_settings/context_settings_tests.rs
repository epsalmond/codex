use super::*;
use codex_config::config_toml::AutoShakeThresholdToml;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use core_test_support::responses::ev_completed_with_tokens;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_child_keeps_saved_compaction_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mut initial = step_settings_test()
        .with_session_source(SessionSource::SubAgent(SubAgentSource::Other(
            "context-settings-worker".into(),
        )))
        .with_config(|config| {
            config.model_context_window = Some(100_000);
            config.model_auto_compact_token_limit = Some(30_000);
            config.model_auto_compact_token_limit_scope = AutoCompactTokenLimitScope::Total;
            config.subagent_context_reduction.enabled = false;
            config.auto_shake.threshold = Some(AutoShakeThresholdToml::Off);
        })
        .build_with_auto_env(&server)
        .await?;
    let started = initial
        .thread_manager
        .start_thread(StartThreadOptions {
            history_mode: Some(ThreadHistoryMode::Legacy),
            dynamic_tools: vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: "pause_context_test".into(),
                description: "Wait for a context settings update.".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
                defer_loading: false,
            })],
            ..StartThreadOptions::new(initial.config.clone())
        })
        .await?;
    initial.codex.shutdown_and_wait().await?;
    initial.codex = started.thread;
    initial.session_configured = started.session_configured;
    initial.codex.checkpoint_thread_settings().await?;
    let resumed = step_settings_test()
        .with_config(|config| {
            config.model_context_window = Some(100_000);
            config.model_auto_compact_token_limit = Some(80_000);
            config.model_auto_compact_token_limit_scope = AutoCompactTokenLimitScope::Total;
            config.subagent_context_reduction.enabled = false;
            config.auto_shake.threshold = Some(AutoShakeThresholdToml::Off);
        })
        .restart_with_auto_env(&server, &initial)
        .await?;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("pause-response"),
                ev_function_call("pause-call", "pause_context_test", "{}"),
                ev_completed_with_tokens("pause-response", /*total_tokens*/ 1_000),
            ]),
            sse(vec![
                ev_response_created("switched-response"),
                ev_function_call("switched-call", "pause_context_test", "{}"),
                ev_completed_with_tokens("switched-response", /*total_tokens*/ 40_000),
            ]),
            sse(vec![
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {"type": "compaction", "encrypted_content": "Saved context summary"}
                }),
                ev_completed_with_tokens("compact-summary", /*total_tokens*/ 1_000),
            ]),
            sse_completed("done-response"),
        ],
    )
    .await;
    resumed
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "pause before continuing".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    for step in 0..2 {
        let paused = wait_for_event_match(&resumed.codex, |event| match event {
            EventMsg::DynamicToolCallRequest(request) => Some(request.clone()),
            _ => None,
        })
        .await;
        if step == 0 {
            apply_turn_settings(
                &resumed.codex,
                &paused.turn_id,
                TurnSettingsUpdate {
                    model: Some(MODEL_B.to_string()),
                    ..Default::default()
                },
            )
            .await?;
        }
        resumed
            .codex
            .submit(Op::DynamicToolResponse {
                id: paused.call_id,
                response: DynamicToolResponse {
                    content_items: vec![DynamicToolCallOutputContentItem::InputText {
                        text: "continue".into(),
                    }],
                    success: true,
                },
            })
            .await?;
    }
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = requests.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| (
                request.body_json()["model"].as_str().unwrap().to_string(),
                request.inputs_of_type("compaction_trigger").len(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (MODEL_A.to_string(), 0),
            (MODEL_B.to_string(), 0),
            (MODEL_A.to_string(), 1),
            (MODEL_B.to_string(), 0),
        ]
    );
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}
