use super::*;
use crate::agent::types::ThreadSpawnEdgeStatus;
use pretty_assertions::assert_eq;

/// Open agents as `list_agents` sees them, keyed by thread id with their loaded state.
async fn list_agents_view(
    control: &LocalAgentControl,
    source: &SessionSource,
) -> Vec<(ThreadId, bool)> {
    let mut view = control
        .list_agents(source, /*path_prefix*/ None)
        .await
        .expect("list agents")
        .into_iter()
        .filter(|agent| {
            !agent
                .metadata
                .agent_path
                .as_ref()
                .is_some_and(AgentPath::is_root)
        })
        .map(|agent| (agent.thread_id, agent.live_status.is_some()))
        .collect::<Vec<_>>();
    view.sort_by_key(|(thread_id, _)| thread_id.to_string());
    view
}

/// Open agents as the app-server's `thread/list` overlay sees them.
async fn client_view(manager: &ThreadManager, root: ThreadId) -> Vec<(ThreadId, bool)> {
    let mut view = manager
        .list_spawned_descendants(root)
        .await
        .expect("list spawned descendants")
        .into_iter()
        .filter(|descendant| descendant.edge_status == ThreadSpawnEdgeStatus::Open)
        .map(|descendant| (descendant.thread_id, descendant.live_status.is_some()))
        .collect::<Vec<_>>();
    view.sort_by_key(|(thread_id, _)| thread_id.to_string());
    view
}

fn sorted(mut view: Vec<(ThreadId, bool)>) -> Vec<(ThreadId, bool)> {
    view.sort_by_key(|(thread_id, _)| thread_id.to_string());
    view
}

#[tokio::test]
async fn list_agents_and_client_view_agree_when_an_open_child_is_unloaded() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::Sqlite).unwrap();
    // Spawning outside a parent turn needs polling mode.
    config.multi_agent_v2.agent_polling = AgentPolling::Enabled;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_paginated_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let idle = spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "idle").await;
    let resident =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "resident").await;

    // Residency unloads the idle child; its spawn edge stays open.
    let idle_thread = harness.manager.get_thread(idle.thread_id).await.unwrap();
    idle_thread
        .inject_response_items(vec![assistant_message(
            "idle child persisted",
            Some(MessagePhase::FinalAnswer),
        )])
        .await
        .unwrap();
    idle_thread.session.mark_interrupted();
    *idle_thread.session.active_turn.lock().await = None;
    assert_eq!(
        harness
            .manager
            .try_evict_v2_thread(idle_thread)
            .await
            .unwrap(),
        ThreadEvictionOutcome::Evicted,
    );
    assert_thread_not_loaded(&harness.manager, idle.thread_id).await;
    assert!(
        control
            .runtime
            .agent_is_idle_for_close(idle.thread_id)
            .await
    );
    assert_eq!(
        control
            .runtime
            .agent_pending_work(idle.thread_id)
            .await
            .unwrap()
            .count,
        0,
    );

    let expected = sorted(vec![(idle.thread_id, false), (resident.thread_id, true)]);
    assert_eq!(
        list_agents_view(&control, &root.session_source).await,
        expected
    );
    assert_eq!(client_view(&harness.manager, root_id).await, expected);

    // Closing the unloaded child closes its edge in both views.
    control
        .close_v2_agent(idle.thread_id)
        .await
        .expect("close unloaded child");
    let expected = vec![(resident.thread_id, true)];
    assert_eq!(
        list_agents_view(&control, &root.session_source).await,
        expected
    );
    assert_eq!(client_view(&harness.manager, root_id).await, expected);
    let closed = harness
        .manager
        .list_spawned_descendants(root_id)
        .await
        .unwrap()
        .into_iter()
        .find(|descendant| descendant.thread_id == idle.thread_id)
        .expect("closed child keeps its edge");
    assert_eq!(
        (closed.edge_status, closed.live_status),
        (ThreadSpawnEdgeStatus::Closed, None)
    );
}

#[tokio::test]
async fn pending_work_inspection_preserves_finite_records_until_acceptance() {
    use crate::session::async_completion::CompletionStatus;
    use crate::session::tests::make_session_and_context;

    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.multi_agent_v2.agent_polling = AgentPolling::Enabled;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (_, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let parent =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "parent").await;
    let parent_thread = harness.manager.get_thread(parent.thread_id).await.unwrap();
    parent_thread.session.mark_interrupted();
    *parent_thread.session.active_turn.lock().await = None;
    let child =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &parent_thread, "finite")
            .await;
    let thread = harness.manager.get_thread(child.thread_id).await.unwrap();
    thread.session.mark_interrupted();
    *thread.session.active_turn.lock().await = None;
    let (_, turn) = make_session_and_context().await;
    let store = &thread.session.services.async_completions;
    assert!(
        control
            .runtime
            .agent_is_idle_for_close(child.thread_id)
            .await
    );

    let mut reservation = store
        .reserve(child.thread_id, &turn, "job", 27, Some("cell"))
        .unwrap();
    reservation.set_title(Some("Compile focused tests"));
    reservation.set_command("cargo test -p codex-core");
    let publisher = reservation.register();
    // Inline records are absent from the display summary but still block an idle close.
    assert_eq!(
        control
            .runtime
            .agent_pending_work(child.thread_id)
            .await
            .unwrap()
            .count,
        0
    );
    assert!(
        !control
            .runtime
            .agent_is_idle_for_close(child.thread_id)
            .await
    );
    drop(reservation);
    let summary = control
        .runtime
        .agent_pending_work(child.thread_id)
        .await
        .unwrap();
    assert_eq!(summary.count, 1);
    let job = &summary.jobs[0];
    assert_eq!(job.process_id.as_deref(), Some("27"));
    assert_eq!(job.title.as_deref(), Some("Compile focused tests"));
    assert_eq!(job.command.as_deref(), Some("cargo test -p codex-core"));
    assert_eq!(job.cell_id.as_deref(), Some("cell"));
    assert!(!job.ready);
    assert!(
        !control
            .runtime
            .agent_is_idle_for_close(child.thread_id)
            .await
    );

    let closed = close_idle(&root).await;
    assert_eq!(closed["closed"], serde_json::json!([]));
    assert!(harness.manager.get_thread(parent.thread_id).await.is_ok());
    assert!(harness.manager.get_thread(child.thread_id).await.is_ok());

    publisher.publish(CompletionStatus::Exited(0), b"finished", 0);
    let summary = control
        .runtime
        .agent_pending_work(child.thread_id)
        .await
        .unwrap();
    assert_eq!(summary.count, 1);
    assert!(summary.jobs[0].ready);
    assert!(
        !control
            .runtime
            .agent_is_idle_for_close(child.thread_id)
            .await
    );
    assert_eq!(thread.agent_status().await, AgentStatus::Interrupted);
    let closed = close_idle(&root).await;
    assert_eq!(closed["closed"], serde_json::json!([]));
    assert!(harness.manager.get_thread(parent.thread_id).await.is_ok());
    assert!(harness.manager.get_thread(child.thread_id).await.is_ok());
    store.accept_for_test(&[ResponseItemId::from_server(summary.jobs[0].id.clone())]);
    assert_eq!(
        control
            .runtime
            .agent_pending_work(child.thread_id)
            .await
            .unwrap()
            .count,
        0
    );
    assert!(
        control
            .runtime
            .agent_is_idle_for_close(child.thread_id)
            .await
    );
    assert!(
        !control
            .runtime
            .agent_is_idle_for_close(ThreadId::new())
            .await
    );
    assert!(
        control
            .runtime
            .agent_pending_work(ThreadId::new())
            .await
            .is_none()
    );
    let closed = close_idle(&root).await;
    assert_eq!(closed["closed"][0]["agent_name"], "/root/parent");
    assert_eq!(closed["failed"], serde_json::json!([]));
    assert_thread_not_loaded(&harness.manager, child.thread_id).await;
}

#[test_case::test_case(false; "running_service")]
#[test_case::test_case(true; "ready_service")]
#[tokio::test]
async fn pending_work_inspection_excludes_service_records(ready: bool) {
    use crate::agent::control::WorkKind;
    use crate::agent::control::coordinator::AgentWakeCoordinator;
    use crate::agent::control::work_lifecycle::WorkCoordinator;
    use crate::session::async_completion::CompletionStatus;
    use crate::session::tests::make_session_and_context;

    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.multi_agent_v2.agent_polling = AgentPolling::Enabled;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (_, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let child =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "service").await;
    let thread = harness.manager.get_thread(child.thread_id).await.unwrap();
    thread.session.mark_interrupted();
    *thread.session.active_turn.lock().await = None;
    let (_, turn) = make_session_and_context().await;
    let lifecycle = Arc::new(WorkCoordinator::new(Arc::new(
        AgentWakeCoordinator::default(),
    )));
    let service = lifecycle
        .reserve_producer(
            child.thread_id,
            &turn.sub_id,
            "service",
            None,
            WorkKind::Service,
            None,
        )
        .unwrap();
    let store = &thread.session.services.async_completions;
    let mut reservation = store
        .reserve_with_lifecycle(child.thread_id, &turn, "service", 30, None, &service)
        .unwrap();
    let publisher = reservation.register();
    drop(reservation);
    if ready {
        publisher.publish(CompletionStatus::Exited(0), b"service ended", 0);
    }
    let summary = control
        .runtime
        .agent_pending_work(child.thread_id)
        .await
        .unwrap();
    assert_eq!(summary.count, 0);
    assert!(summary.jobs.is_empty());
    assert!(
        control
            .runtime
            .agent_is_idle_for_close(child.thread_id)
            .await
    );
    let closed = close_idle(&root).await;
    assert_eq!(closed["closed"][0]["agent_name"], "/root/service");
    assert_eq!(closed["failed"], serde_json::json!([]));
    assert_thread_not_loaded(&harness.manager, child.thread_id).await;
    store.retire();
}

async fn close_idle(root: &CodexThread) -> serde_json::Value {
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::multi_agents_v2::CloseAgentsHandler;
    use crate::tools::registry::ToolExecutor;
    use crate::turn_diff_tracker::TurnDiffTracker;

    let turn = root.session.new_default_turn().await;
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({"targets": ["idle"]}).to_string(),
    };
    CloseAgentsHandler
        .handle(ToolInvocation {
            session: Arc::clone(&root.session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "close-idle".to_string(),
            tool_name: codex_tools::ToolName::plain("close_agents"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
            native_call: None,
            environments: Default::default(),
        })
        .await
        .expect("close idle agents")
        .code_mode_result(&payload)
}
