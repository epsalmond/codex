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
