use super::*;
use crate::function_tool::FunctionCallError;
use crate::tools::handlers::multi_agents_common::collab_v2_agent_error;
use pretty_assertions::assert_eq;

fn followup(caller: ThreadId, target: AgentTarget, config: &Config) -> SendRequest {
    SendRequest {
        caller,
        target,
        resume_config: config.clone(),
        input: AgentInput::Message {
            message: AgentMessage::Plaintext("more work".to_string()),
            mode: MessageDeliveryMode::TriggerTurn,
        },
        start_options: Default::default(),
    }
}

fn model_visible_error(thread_id: ThreadId, err: CodexErr) -> String {
    match collab_v2_agent_error(thread_id, err) {
        FunctionCallError::RespondToModel(message) => message,
        other => panic!("expected a model-visible error, got {other:?}"),
    }
}

#[tokio::test]
async fn archive_close_cascades_and_rejects_later_followups() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    config.multi_agent_v2.agent_polling = AgentPolling::Enabled;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let worker =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "worker").await;
    let worker_thread = harness.manager.get_thread(worker.thread_id).await.unwrap();
    let helper =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &worker_thread, "helper")
            .await;
    let sibling =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "sibling").await;
    drop(worker_thread);

    // The app-server archive and delete paths close through the thread manager.
    assert!(
        harness
            .manager
            .close_multi_agent_v2_agent(worker.thread_id)
            .await
            .expect("close worker subtree")
    );

    for closed in [worker.thread_id, helper.thread_id] {
        assert_thread_not_loaded(&harness.manager, closed).await;
        assert!(control.get_agent_metadata(closed).is_none());
    }
    assert!(harness.manager.get_thread(sibling.thread_id).await.is_ok());
    let listed = control
        .list_agents(&root.session_source, /*path_prefix*/ None)
        .await
        .expect("list agents")
        .into_iter()
        .map(|agent| agent.thread_id)
        .collect::<Vec<_>>();
    assert!(listed.contains(&sibling.thread_id), "{listed:?}");
    assert!(!listed.contains(&worker.thread_id), "{listed:?}");
    assert!(!listed.contains(&helper.thread_id), "{listed:?}");

    let graph_store = control
        .runtime
        .upgrade()
        .unwrap()
        .agent_graph_store()
        .expect("sqlite agent graph store");
    for (parent, child) in [
        (root_id, worker.thread_id),
        (worker.thread_id, helper.thread_id),
    ] {
        let closed_children = graph_store
            .list_thread_spawn_children(
                parent,
                Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed),
            )
            .await
            .expect("closed children");
        assert!(closed_children.contains(&child));
    }

    let by_id = control
        .send(followup(
            root_id,
            AgentTarget::Id(worker.thread_id),
            &harness.config,
        ))
        .await
        .err()
        .expect("follow-up to a closed agent must fail");
    assert_eq!(
        model_visible_error(worker.thread_id, by_id),
        "agent `/root/worker` is closed; spawn a fresh agent with a self-contained brief"
    );
    let by_path = control
        .send(followup(
            root_id,
            AgentTarget::Reference("/root/worker/helper".to_string()),
            &harness.config,
        ))
        .await
        .err()
        .expect("follow-up to a closed grandchild must fail");
    assert_eq!(
        model_visible_error(helper.thread_id, by_path),
        "agent `/root/worker/helper` is closed; spawn a fresh agent with a self-contained brief"
    );
    let reload = control
        .ensure_v2_agent_loaded(
            harness.config.clone(),
            helper.thread_id,
            /*parent*/ None,
        )
        .await
        .err()
        .expect("closed agents must not reload");
    assert!(reload.to_string().contains("is closed"), "{reload}");
    assert_thread_not_loaded(&harness.manager, helper.thread_id).await;

    // Closing again is a no-op, and the path can be reused by a fresh agent.
    control
        .close_v2_agent(worker.thread_id)
        .await
        .expect("repeat close");
    let fresh = spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "worker").await;
    assert_ne!(fresh.thread_id, worker.thread_id);
    let resolved = control
        .resolve_target(root_id, &AgentTarget::Reference("worker".to_string()))
        .expect("fresh agent resolves");
    assert_eq!(resolved, fresh.thread_id);
}

#[tokio::test]
async fn close_v2_agent_rejects_the_root() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.agent_polling = AgentPolling::Enabled;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let _worker =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "worker").await;

    assert!(control.close_v2_agent(root_id).await.is_err());
    assert!(
        !harness
            .manager
            .close_multi_agent_v2_agent(root_id)
            .await
            .expect("root archive leaves agent state alone")
    );
    assert!(harness.manager.get_thread(root_id).await.is_ok());
}

/// Archiving a root archives its subtree; after the root is unarchived, its still-open
/// descendants must rejoin the tree even though their own rollouts stay archived.
#[tokio::test]
async fn archived_descendant_with_open_edge_is_restored_and_reloadable() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    config.multi_agent_v2.agent_polling = AgentPolling::Enabled;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let worker =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "worker").await;
    let worker_thread = harness.manager.get_thread(worker.thread_id).await.unwrap();
    persist_thread_for_tree_resume(&worker_thread, "worker persisted").await;
    drop(worker_thread);
    // Unload without closing the spawn edge, then archive the rollout as a root archive would.
    let _ = control.shutdown_live_agent(worker.thread_id).await;
    LocalThreadStore::new(
        LocalThreadStoreConfig::from_config(&harness.config),
        harness.state_db.clone(),
    )
    .archive_thread(ArchiveThreadParams {
        thread_id: worker.thread_id,
    })
    .await
    .expect("archive worker rollout");
    assert!(control.get_agent_metadata(worker.thread_id).is_none());

    control
        .restore_v2_agent_metadata(&harness.config, root_id)
        .await;
    assert!(
        control.get_agent_metadata(worker.thread_id).is_some(),
        "an archived descendant with an open spawn edge must be restored"
    );
    control
        .ensure_v2_agent_loaded(
            harness.config.clone(),
            worker.thread_id,
            /*parent*/ None,
        )
        .await
        .expect("an archived descendant with an open spawn edge must reload");
}

/// Starts a root with an idle `/root/worker`, as a task agent is once its final turn ends.
async fn idle_worker_after_final(
    agent_polling: AgentPolling,
) -> (
    AgentControlHarness,
    ThreadId,
    LocalAgentControl,
    LiveAgent,
    Arc<CodexThread>,
) {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    config.multi_agent_v2.agent_polling = agent_polling;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let worker =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "worker").await;
    let thread = harness.manager.get_thread(worker.thread_id).await.unwrap();
    // The fixture has no task runner to finish the worker's first turn.
    thread.session.mark_interrupted();
    *thread.session.active_turn.lock().await = None;
    (harness, root_id, control, worker, thread)
}

/// #213: a follow-up admitted before the close holds its residency pin until the worker handles
/// it. The close waits for that delivery, finds the follow-up, and keeps the worker for the turn
/// it starts instead of dropping it.
#[tokio::test]
async fn close_after_final_keeps_worker_for_in_flight_follow_up() {
    let (harness, _root_id, control, worker, thread) =
        idle_worker_after_final(AgentPolling::Enabled).await;

    let in_flight = Arc::clone(&thread.residency_gate).read_owned().await;
    let mut close = Box::pin(control.close_task_agent_after_final(worker.thread_id));
    poll_fn(|cx| {
        assert!(
            close.as_mut().poll(cx).is_pending(),
            "the close must wait for the in-flight follow-up"
        );
        Poll::Ready(())
    })
    .await;
    thread
        .session
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::root(),
                worker.metadata.agent_path.clone().unwrap(),
                Vec::new(),
                "more work".to_string(),
                /*trigger_turn*/ true,
            ),
            Default::default(),
        )
        .await;
    drop(in_flight);

    assert!(!close.await.expect("close after final"));
    assert!(harness.manager.get_thread(worker.thread_id).await.is_ok());
    assert_eq!(
        control.runtime.registry.closed_agent_path(worker.thread_id),
        None
    );
    assert_eq!(
        thread
            .session
            .input_queue
            .trigger_turn_mailbox_count()
            .await,
        1
    );
}

/// A follow-up that reaches the worker after the close decided gets the closed-agent error, and
/// the spawn edge ends closed.
#[tokio::test]
async fn close_after_final_rejects_follow_up_queued_behind_it() {
    let (harness, root_id, control, worker, thread) =
        idle_worker_after_final(AgentPolling::Enabled).await;

    let in_flight = Arc::clone(&thread.residency_gate).read_owned().await;
    let mut close = Box::pin(control.close_task_agent_after_final(worker.thread_id));
    let mut late = Box::pin(control.send(followup(
        root_id,
        AgentTarget::Id(worker.thread_id),
        &harness.config,
    )));
    poll_fn(|cx| {
        assert!(close.as_mut().poll(cx).is_pending());
        assert!(late.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(thread);
    drop(in_flight);

    assert!(close.await.expect("close after final"));
    let err = late
        .await
        .err()
        .expect("a follow-up behind the close must fail");
    assert_eq!(
        model_visible_error(worker.thread_id, err),
        "agent `/root/worker` is closed; spawn a fresh agent with a self-contained brief"
    );
    assert_thread_not_loaded(&harness.manager, worker.thread_id).await;
    let closed_children = control
        .runtime
        .upgrade()
        .unwrap()
        .agent_graph_store()
        .expect("sqlite agent graph store")
        .list_thread_spawn_children(
            root_id,
            Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed),
        )
        .await
        .expect("closed children");
    assert_eq!(closed_children, vec![worker.thread_id]);
}
