use anyhow::Context;
use codex_core::config::SubagentContextReductionConfig;
use codex_protocol::context_usage::ContextReductionOutcome;
use codex_protocol::context_usage::ContextTokenBasis;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

fn completed_without_usage(id: &str) -> serde_json::Value {
    serde_json::json!({"type": "response.completed", "response": {"id": id}})
}

fn child_builder(cap: u64) -> core_test_support::test_codex::TestCodexBuilder {
    test_codex()
        .with_model_info_override("actually-selected-step", |model| {
            model.context_window = Some(80_000);
            model.effective_context_window_percent = 95;
        })
        .with_model("gpt-5.6-sol")
        .with_session_source(SessionSource::SubAgent(SubAgentSource::Other(
            "snapshot".to_string(),
        )))
        .with_config(move |config| {
            config.subagent_context_reduction = SubagentContextReductionConfig {
                enabled: true,
                threshold_tokens: cap,
            };
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
}

#[test_case::test_case(None; "unavailable counters")]
#[test_case::test_case(Some(0); "measured zero")]
#[test_case::test_case(Some(40_000); "measured usage")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_model_policy_and_provider_availability(
    tokens: Option<i64>,
) -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let completed = tokens.map_or_else(
        || completed_without_usage("done"),
        |tokens| responses::ev_completed_with_tokens("done", tokens),
    );
    let mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("done", "done"),
            completed,
        ]),
    )
    .await;
    let fixture = child_builder(/*cap*/ 50_000)
        .build_with_auto_env(&server)
        .await?;
    fixture
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Complete this task.".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                model: Some("actually-selected-step".to_string()),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let snapshot = fixture
        .codex
        .context_usage_snapshot()
        .await
        .context("captured observation")?;
    assert_eq!(
        snapshot.selected_model.as_deref(),
        mock.single_request().body_json()["model"].as_str()
    );
    assert_ne!(snapshot.selected_model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(
        (
            snapshot.child_policy_enabled,
            snapshot.child_active_cap_tokens,
            snapshot.model_window_tokens
        ),
        (Some(true), Some(50_000), Some(76_000))
    );
    assert_eq!(
        snapshot.basis,
        if tokens.is_some() {
            ContextTokenBasis::Usage
        } else {
            ContextTokenBasis::Estimate
        }
    );
    assert_eq!(snapshot.provider_usage_at.is_some(), tokens.is_some());
    assert!(snapshot.observed_at.is_some());
    assert_eq!(
        fixture
            .codex
            .token_usage_info()
            .await
            .map(|info| info.total_token_usage.total_tokens),
        tokens
    );
    fixture.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_initial_reduction_emits_metadata_before_terminal_without_usage()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_response_once_match(
        &server,
        |_: &wiremock::Request| true,
        wiremock::ResponseTemplate::new(500),
    )
    .await;
    let fixture = child_builder(/*cap*/ 1_000)
        .build_with_auto_env(&server)
        .await?;
    fixture
        .submit_text_turn("Keep the initial task constraint.")
        .await?;
    let snapshot = fixture
        .codex
        .context_usage_snapshot()
        .await
        .context("failed reduction observation")?;
    let record = snapshot
        .last_reduction
        .as_ref()
        .context("reduction outcome")?;
    assert_eq!(record.outcome, ContextReductionOutcome::Failed);
    assert_eq!(record.after_tokens, None);
    assert!(record.before_tokens > 1_000);
    assert_eq!(snapshot.provider_usage_at, None);
    assert_eq!(fixture.codex.token_usage_info().await, None);
    assert!(
        mock.single_request().body_json()["input"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item["type"] == "compaction_trigger"))
    );
    fixture.codex.shutdown_and_wait().await?;
    let history = fixture
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: fixture.session_configured.thread_id,
            include_archived: true,
        })
        .await?;
    let metadata = history.items.iter().position(|item| matches!(item, codex_rollout::RolloutItem::EventMsg(EventMsg::TokenCount(event)) if event.info.is_none() && event.context_usage.as_ref().and_then(|usage| usage.last_reduction.as_ref()).is_some_and(|record| record.outcome == ContextReductionOutcome::Failed))).context("metadata event")?;
    let terminal = history
        .items
        .iter()
        .position(|item| {
            matches!(
                item,
                codex_rollout::RolloutItem::EventMsg(
                    EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
                )
            )
        })
        .context("terminal event")?;
    assert!(metadata < terminal);
    Ok(())
}
