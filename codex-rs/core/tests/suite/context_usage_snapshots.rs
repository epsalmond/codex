use anyhow::Context;
use codex_core::config::SubagentContextReductionConfig;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_protocol::context_usage::ContextReductionOutcome;
use codex_protocol::context_usage::ContextTokenBasis;
use codex_protocol::mcp::ClientMcpExtensions;
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
use std::sync::Arc;

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

#[test_case::test_case(Some(0), Some(0); "verified unsealed boundary")]
#[test_case::test_case(Some(17), None; "mismatched saved boundary")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_preserves_captured_observation_and_provider_times(
    saved_watermark: Option<u64>,
    restored_watermark: Option<u64>,
) -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("seed", "done"),
            responses::ev_completed_with_tokens("seed", /*total_tokens*/ 40_000),
        ]),
    )
    .await;
    let initial = child_builder(/*cap*/ 50_000)
        .build_with_auto_env(&server)
        .await?;
    initial.submit_text_turn("Remember this task.").await?;
    let mut expected = initial
        .codex
        .context_usage_snapshot()
        .await
        .context("initial observation")?;
    expected.observed_at = Some(1_700_000_010);
    expected.provider_usage_at = Some(1_700_000_000);
    expected.shake_watermark = saved_watermark;
    initial.codex.shutdown_and_wait().await?;
    let mut history = initial
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: initial.session_configured.thread_id,
            include_archived: true,
        })
        .await?;
    for item in &mut history.items {
        if let codex_rollout::RolloutItem::EventMsg(EventMsg::TokenCount(event)) = item
            && event.context_usage.is_some()
        {
            event.context_usage = Some(expected.clone());
        }
    }
    let resumed = initial
        .thread_manager
        .resume_thread_with_history(
            initial.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: history.thread_id,
                history: Arc::new(history.items),
                rollout_path: initial.session_configured.rollout_path.clone(),
                last_activity_at: None,
            }),
            initial.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?;
    expected.shake_watermark = restored_watermark;
    assert_eq!(
        resumed.thread.context_usage_snapshot().await,
        Some(expected)
    );
    resumed.thread.shutdown_and_wait().await?;
    Ok(())
}

#[test_case::test_case(false, false; "live")]
#[test_case::test_case(true, false; "resumed after completion without usage")]
#[test_case::test_case(true, true; "resumed after exhausted early close")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_without_usage_keeps_provider_measurement_and_estimated_newer_context(
    resume: bool,
    early_close: bool,
) -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("seed", "done"),
            responses::ev_completed_with_tokens("seed", /*total_tokens*/ 48_000),
        ]),
    )
    .await;
    let mut fixture = child_builder(/*cap*/ 100_000)
        .build_with_auto_env(&server)
        .await?;
    fixture.submit_text_turn("Complete this task.").await?;
    let measured = fixture
        .codex
        .context_usage_snapshot()
        .await
        .context("measured observation")?;
    let mut unmeasured = vec![responses::ev_assistant_message(
        "unmeasured",
        &"x".repeat(/*n*/ 196_000),
    )];
    if !early_close {
        unmeasured.push(completed_without_usage("unmeasured"));
    }
    let unmeasured_request = responses::mount_sse_once(&server, responses::sse(unmeasured)).await;
    fixture.submit_text_turn("Continue the task.").await?;
    let newer = fixture
        .codex
        .context_usage_snapshot()
        .await
        .context("newer observation")?;
    assert_eq!(newer.provider_usage_at, measured.provider_usage_at);
    assert_eq!(newer.basis, ContextTokenBasis::Usage);
    assert!(
        (95_000..100_000).contains(&newer.active_tokens),
        "saved floor must remain below the cap before resumed input: {newer:?}"
    );
    // Even the ordinary request plus retained output and new input fits locally.
    // The provider-based floor, rather than the local estimate, must reject dispatch.
    assert!(
        unmeasured_request
            .single_request()
            .body_json()
            .to_string()
            .len()
            + 196_000
            + 16_000
            < 100_000 * 4
    );
    if resume {
        fixture.codex.shutdown_and_wait().await?;
        let history = fixture
            .thread_store
            .load_latest_model_context(LoadThreadHistoryParams {
                thread_id: fixture.session_configured.thread_id,
                include_archived: true,
            })
            .await?;
        let resumed = fixture
            .thread_manager
            .resume_thread_with_history(
                fixture.config.clone(),
                InitialHistory::Resumed(ResumedHistory {
                    conversation_id: history.thread_id,
                    history: Arc::new(history.items),
                    rollout_path: fixture.session_configured.rollout_path.clone(),
                    last_activity_at: None,
                }),
                fixture.thread_manager.auth_manager(),
                /*parent_trace*/ None,
                ClientMcpExtensions::default(),
            )
            .await?;
        fixture.codex = resumed.thread;
        assert_eq!(fixture.codex.context_usage_snapshot().await, Some(newer));
    }
    let compact = responses::mount_sse_once(&server, responses::sse(vec![
        serde_json::json!({"type": "response.output_item.done", "item": {"type": "compaction", "encrypted_content": "newer-output-summary"}}),
        responses::ev_completed("compact"),
    ])).await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body["input"].to_string().contains("newer-output-summary"))
        },
        responses::sse(vec![
            responses::ev_assistant_message("final", "done"),
            responses::ev_completed("final"),
        ]),
    )
    .await;
    fixture.submit_text_turn(&"y".repeat(/*n*/ 16_000)).await?;
    assert!(
        compact.single_request().body_json()["input"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item["type"] == "compaction_trigger"))
    );
    let reduced = fixture
        .codex
        .context_usage_snapshot()
        .await
        .context("reduced observation")?;
    assert_eq!(
        reduced.last_reduction.as_ref().map(|record| record.outcome),
        Some(ContextReductionOutcome::Compacted)
    );
    assert!(reduced.active_tokens < 50_000);
    fixture.codex.shutdown_and_wait().await?;
    Ok(())
}
