use std::sync::Arc;
use std::time::Duration;

use codex_analytics::AnalyticsEventsClient;
use codex_core::CodexAppsToolsCache;
use codex_core::StartIfIdleSubmission;
use codex_core::StartThreadOptions;
use codex_core::ThreadManager;
use codex_core::TurnInputRequest;
use codex_core::TurnStartOptions;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_goal_extension::GoalExtensionConfig;
use codex_goal_extension::GoalObjectiveUpdate;
use codex_goal_extension::GoalService;
use codex_goal_extension::GoalSetRequest;
use codex_goal_extension::GoalTokenBudgetUpdate;
use codex_goal_extension::install_with_backend;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[tokio::test]
async fn goal_objective_update_during_queue_turn_registration_reaches_model() -> anyhow::Result<()>
{
    struct TurnRegistrationGate {
        entered: mpsc::Sender<()>,
        release: Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl TurnLifecycleContributor for TurnRegistrationGate {
        fn on_turn_start<'a>(
            &'a self,
            _input: TurnStartInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                let release = self.release.lock().await.take();
                if let Some(release) = release {
                    let _ = self.entered.send(()).await;
                    let _ = release.await;
                }
            })
        }
    }

    let mock = responses::start_mock_server().await;
    let test = test_codex().build_with_auto_env(&mock).await?;
    let state_db = codex_core::init_state_db(&test.config)
        .await
        .ok_or_else(|| anyhow::anyhow!("goal state database must be available"))?;
    let goal_service = Arc::new(GoalService::new());
    let (entered, mut gate_entered) = mpsc::channel(/*buffer*/ 1);
    let (release, gate_release) = oneshot::channel();
    let gate = Arc::new(TurnRegistrationGate {
        entered,
        release: Mutex::new(Some(gate_release)),
    });
    let manager = Arc::new_cyclic(|manager| {
        let mut extensions = ExtensionRegistryBuilder::new();
        install_with_backend(
            &mut extensions,
            Arc::clone(&state_db),
            AnalyticsEventsClient::disabled(),
            /*metrics_client*/ None,
            manager.clone(),
            Arc::clone(&goal_service),
            |_| GoalExtensionConfig {
                enabled: true,
                ..Default::default()
            },
        );
        extensions.turn_lifecycle_contributor(gate.clone());
        ThreadManager::new(
            &test.config,
            test.thread_manager.auth_manager(),
            test.thread_manager.get_models_manager(),
            CodexAppsToolsCache::default(),
            SessionSource::Exec,
            test.thread_manager.environment_manager(),
            Arc::new(extensions.build()),
            Arc::new(codex_core::test_support::EmptyUserInstructionsProvider),
            /*analytics_events_client*/ None,
            test.thread_manager.image_store(),
            Arc::clone(&test.thread_store),
            codex_core::local_agent_graph_store_from_state_db(Some(&state_db)),
            "goal-injection-test".to_string(),
            /*attestation_provider*/ None,
            /*external_time_provider*/ None,
        )
    });
    let (release_response, response_gate) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(response_gate),
        body: responses::sse_completed("queue-turn"),
    }]])
    .await;
    let mut config = test.config.clone();
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    let thread = manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![test.executor_environment().request()]),
            ..StartThreadOptions::new(config)
        })
        .await?
        .thread;
    let thread_id = thread.startup_metadata().thread_id;
    // Only runtime steering should add goal context; keep instruction recording inert.
    goal_service
        .set_thread_goal(
            &state_db,
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Set("original objective"),
                status: Some(ThreadGoalStatus::Active),
                token_budget: GoalTokenBudgetUpdate::Keep,
                max_goal_token_budget: None,
            },
            std::future::ready(Ok(())),
        )
        .await?;

    // Match the automatic queue dispatch request, which does not take the goal permit.
    let submission = tokio::spawn({
        let thread = Arc::clone(&thread);
        async move {
            thread
                .start_turn_if_idle(
                    TurnInputRequest::user_input(vec![UserInput::Text {
                        text: "queued prompt".to_string(),
                        text_elements: Vec::new(),
                    }])
                    .on_start(TurnStartOptions {
                        turn_trigger: Some("queue".to_string()),
                        ..Default::default()
                    }),
                )
                .await
        }
    });
    timeout(Duration::from_secs(/*secs*/ 30), gate_entered.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("turn registration gate should be entered"))?;
    let outcome = goal_service
        .set_thread_goal(
            &state_db,
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Set("updated goal during queue startup"),
                status: Some(ThreadGoalStatus::Active),
                token_budget: GoalTokenBudgetUpdate::Keep,
                max_goal_token_budget: None,
            },
            std::future::ready(Ok(())),
        )
        .await?;
    let effects = tokio::spawn({
        let goal_service = Arc::clone(&goal_service);
        async move { outcome.apply_runtime_effects(&goal_service).await }
    });
    // History recording emits this event before idle continuation waits on Core routing.
    let recorded_steering = timeout(
        Duration::from_secs(/*secs*/ 20),
        // The outer timeout must return before the helper can panic, so the gate is released.
        wait_for_event_with_timeout(
            &thread,
            |event| {
                matches!(event, EventMsg::RawResponseItem(event)
                    if matches!(&event.item, ResponseItem::Message { content, .. }
                        if content.iter().any(|item| matches!(item, ContentItem::InputText { text }
                            if text.contains("updated goal during queue startup")))))
            },
            Duration::from_secs(/*secs*/ 30),
        ),
    )
    .await;
    // Release registration before assertions or further fallible work.
    let _ = release.send(());
    let submission = submission.await??;
    effects.await?;
    timeout(
        Duration::from_secs(/*secs*/ 30),
        server.wait_for_request_count(/*count*/ 1),
    )
    .await?;

    // Stop idle goal continuations after the request is captured, before the response ends.
    let paused = goal_service
        .set_thread_goal(
            &state_db,
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Keep,
                status: Some(ThreadGoalStatus::Paused),
                token_budget: GoalTokenBudgetUpdate::Keep,
                max_goal_token_budget: None,
            },
            std::future::ready(Ok(())),
        )
        .await?;
    paused.apply_runtime_effects(&goal_service).await;
    let _ = release_response.send(());
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let requests = server.requests().await;
    server.shutdown().await;

    assert!(matches!(submission, StartIfIdleSubmission::Started { .. }));
    assert_eq!(requests.len(), 1);
    let request: serde_json::Value = serde_json::from_slice(&requests[0])?;
    let input = request["input"].to_string();
    assert!(input.contains("queued prompt"));
    assert!(input.contains("The active thread goal objective was edited by the user."));
    assert!(input.contains("updated goal during queue startup"));
    assert!(recorded_steering.is_ok());
    Ok(())
}
