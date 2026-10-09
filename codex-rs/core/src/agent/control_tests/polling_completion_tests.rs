use super::*;

#[tokio::test]
async fn polling_completion_cannot_reopen_closed_parent_after_archive_rollback() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    config.multi_agent_v2.agent_polling = AgentPolling::Enabled;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let control = root.session.services.local_agent_runtime.control(root.session.session_id());
    let parent = spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "parent").await;
    let parent_thread = harness.manager.get_thread(parent.thread_id).await.unwrap();
    let child = spawn_v2_reload_test_child(&control, harness.config.clone(), &parent_thread, "child").await;
    let inhibition = harness.manager.inhibit_automatic_agent_completions(&[parent.thread_id]).await;
    tokio::time::timeout(Duration::from_secs(5), control.close_agent(parent.thread_id))
        .await.expect("shutdown callbacks must not deadlock with archive inhibition")
        .expect("close subtree");
    drop(inhibition);
    let communication = InterAgentCommunication::new(
        child.metadata.agent_path.unwrap(), parent.metadata.agent_path.unwrap(),
        vec![], "completed after close".into(), /*trigger_turn*/ false,
    );
    assert!(control.deliver_polling_completion(parent.thread_id, child.thread_id, communication).await.is_err());
    assert_thread_not_loaded(&harness.manager, parent.thread_id).await;
    assert_thread_not_loaded(&harness.manager, child.thread_id).await;
    let _ = control.shutdown_live_agent(root_id).await;
}
