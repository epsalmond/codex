use super::*;
use codex_config::config_toml::AutoShakeThresholdToml;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed_with_tokens;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(false; "live_model_switch")]
#[test_case(true; "review")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_child_keeps_saved_compaction_limit(review: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let initial = step_settings_test()
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
                pause_call("pause-call"),
                ev_completed_with_tokens("pause-response", /*total_tokens*/ 40_000),
            ]),
            sse(vec![
                ev_assistant_message("compact-summary", "Saved context summary"),
                ev_completed("compact-summary"),
            ]),
            sse_completed("done-response"),
        ],
    )
    .await;
    let paused = if review {
        resumed
            .codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: "pause before reviewing".into(),
                    },
                    user_facing_hint: None,
                },
            })
            .await?;
        wait_for_event_match(&resumed.codex, |event| match event {
            EventMsg::RequestUserInput(request) => Some(request.clone()),
            _ => None,
        })
        .await
    } else {
        let paused = start_paused_turn(&resumed.codex).await?;
        apply_turn_settings(
            &resumed.codex,
            &paused.turn_id,
            TurnSettingsUpdate {
                model: Some(MODEL_B.to_string()),
                ..Default::default()
            },
        )
        .await?;
        paused
    };
    answer_paused_turn(&resumed.codex, &paused.turn_id).await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = requests.requests();
    let model = if review { MODEL_A } else { MODEL_B };
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
            (model.to_string(), 1),
            (model.to_string(), 0),
        ]
    );
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}
