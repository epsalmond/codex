use super::V2Residency;
use super::V2ResidencySlot;
use crate::StartThreadOptions;
use crate::ThreadManager;
use crate::agent::LocalAgentControl;
use crate::agent::control::AssignmentPhase;
use crate::agent::control::TurnEndDisposition;
use crate::agent::control::coordinator::AgentWakeCoordinator;
use crate::agent::types::AgentMetadata;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::config::test_config;
use crate::thread_manager::ThreadManagerState;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn residency_slot_reservation_unloads_oldest_idle_v2_agent() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control
        .runtime
        .upgrade()
        .expect("thread manager should be live");
    let membership = control.runtime.admit_start().expect("admit residency work");

    let first_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .expect("second resident slot should evict the first idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let second = spawn_v2_subagent(&control, &state, config, root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(second.thread_id).await.is_ok());
}

#[tokio::test]
async fn interrupted_v2_agent_is_lost_after_residency_eviction() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control
        .runtime
        .upgrade()
        .expect("thread manager should be live");
    let membership = control.runtime.admit_start().expect("admit residency work");

    let first_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    mark_thread_interrupted(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .expect("second resident slot should evict the first interrupted idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let second =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);
    mark_thread_completed(second.thread.as_ref()).await;

    let err = control
        .ensure_v2_agent_loaded(config, first.thread_id, /*parent*/ None)
        .await
        .expect_err("evicted interrupted agent should stay lost");
    match err.details() {
        CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
        _ => panic!("expected ThreadNotFound, got {err:?}"),
    }

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(second.thread_id).await.is_ok());
    match manager.get_thread(first.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
}

#[tokio::test]
async fn evicted_waiting_wake_agent_reports_interruption_once() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control
        .runtime
        .upgrade()
        .expect("thread manager should be live");
    control.runtime.enable_wake_mode();

    let coordinator = &control.runtime.wake_coordinator;
    let root_assignment = coordinator
        .begin_or_continue_assignment(
            root.thread_id,
            None,
            "evicted-interrupt-root-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment starts");
    let first_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &control.runtime.admit_start().expect("admit residency work"),
            /*protected_thread_id*/ None,
        )
        .await
        .expect("first resident slot");
    let first_agent_path = AgentPath::try_from("/root/worker_1").expect("agent path");
    let first_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(first_agent_path.clone()),
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let first = spawn_v2_thread_spawn_agent(
        &control,
        &state,
        config.clone(),
        root.thread_id,
        first_source.clone(),
    )
    .await;
    first_slot.commit(first.thread_id);
    control
        .runtime
        .registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("register evicted agent identity")
        .commit(AgentMetadata {
            agent_id: Some(first.thread_id),
            agent_path: Some(first_agent_path),
            agent_role: Some("worker".to_string()),
            ..Default::default()
        });

    let turn = first.thread.session.new_default_turn().await;
    let first_assignment = coordinator
        .reserve_child_assignment(root_assignment.clone(), first.thread_id)
        .expect("first child assignment is admitted")
        .commit()
        .expect("first child assignment commits");
    coordinator
        .begin_or_continue_assignment(
            first.thread_id,
            Some(root_assignment.clone()),
            &turn.sub_id,
            /*allow_new_generation*/ false,
        )
        .expect("first child turn starts");
    turn.agent_assignment
        .set(first_assignment.clone())
        .expect("turn binds to its assignment");
    let _leaf_assignment = coordinator
        .reserve_child_assignment(first_assignment.clone(), ThreadId::new())
        .expect("waiting child has unresolved descendant work")
        .commit()
        .expect("leaf reservation commits");
    assert_eq!(
        control
            .runtime
            .classify_wake_turn_end(
                &first_assignment,
                &turn.sub_id,
                TurnEndDisposition::Succeeded,
            )
            .await,
        Ok(AssignmentPhase::Waiting)
    );
    turn.agent_assignment_waiting
        .set(true)
        .expect("turn is marked as yielding");
    first
        .thread
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("waiting".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    clear_active_turn(first.thread.as_ref()).await;
    assert_eq!(first.thread.agent_status().await, AgentStatus::Waiting);

    let second_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &control.runtime.admit_start().expect("admit residency work"),
            /*protected_thread_id*/ None,
        )
        .await
        .expect("second resident slot evicts the waiting agent");
    assert!(manager.get_thread(first.thread_id).await.is_err());
    let second =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);

    control
        .interrupt_spawned_agent(root.thread_id, first.thread_id)
        .await
        .expect("evicted wake assignment accepts interruption");
    assert_eq!(
        control.runtime.wake_assignment_status(first.thread_id),
        Some(AgentStatus::Interrupted)
    );
    let report = control
        .runtime
        .claim_next_report_for_delivery(&root_assignment)
        .expect("evicted interruption report can be claimed through delivery");
    assert!(
        report
            .communication
            .content
            .to_lowercase()
            .contains("interrupted")
    );
    let report_id = report.id.clone();
    assert!(report.mark_enqueued());
    assert!(
        control
            .runtime
            .acknowledge_terminal_report_mailbox_delivery(&report_id, root_assignment.thread_id)
    );
    control
        .interrupt_spawned_agent(root.thread_id, first.thread_id)
        .await
        .expect("duplicate evicted interruption remains idempotent");
    assert!(
        control
            .runtime
            .claim_next_report_for_delivery(&root_assignment)
            .is_none()
    );
}

#[tokio::test]
async fn closing_interrupted_parent_cancels_and_releases_evicted_grandchild() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.runtime.upgrade().expect("thread manager is live");
    control
        .runtime
        .registry
        .register_root_thread(root.thread_id);

    let child_path = AgentPath::try_from("/root/worker").expect("child path");
    let grandchild_path = AgentPath::try_from("/root/worker/helper").expect("grandchild path");
    let child_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &control.runtime.admit_start().expect("admit residency work"),
            /*protected_thread_id*/ None,
        )
        .await
        .expect("reserve child residency");
    let child = spawn_v2_thread_spawn_agent(
        &control,
        &state,
        config.clone(),
        root.thread_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root.thread_id,
            depth: 1,
            agent_path: Some(child_path.clone()),
            agent_nickname: None,
            agent_role: Some("worker".to_string()),
        }),
    )
    .await;
    child_slot.commit(child.thread_id);
    control
        .runtime
        .registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("register child identity")
        .commit(AgentMetadata {
            agent_id: Some(child.thread_id),
            agent_path: Some(child_path),
            agent_role: Some("worker".to_string()),
            ..Default::default()
        });

    let grandchild_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &control.runtime.admit_start().expect("admit residency work"),
            /*protected_thread_id*/ None,
        )
        .await
        .expect("reserve grandchild residency");
    let grandchild = spawn_v2_thread_spawn_agent(
        &control,
        &state,
        config.clone(),
        child.thread_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: child.thread_id,
            depth: 2,
            agent_path: Some(grandchild_path.clone()),
            agent_nickname: None,
            agent_role: Some("worker".to_string()),
        }),
    )
    .await;
    grandchild_slot.commit(grandchild.thread_id);
    control
        .runtime
        .registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("register grandchild identity")
        .commit(AgentMetadata {
            agent_id: Some(grandchild.thread_id),
            agent_path: Some(grandchild_path),
            agent_role: Some("worker".to_string()),
            ..Default::default()
        });
    mark_thread_completed(child.thread.as_ref()).await;
    mark_thread_completed(grandchild.thread.as_ref()).await;
    control
        .touch_loaded_v2_residency(&state, child.thread_id)
        .await;

    let eviction_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &control.runtime.admit_start().expect("admit residency work"),
            /*protected_thread_id*/ None,
        )
        .await
        .expect("third reservation evicts the least-recently-used grandchild");
    drop(eviction_slot);
    assert!(manager.get_thread(grandchild.thread_id).await.is_err());
    assert!(
        control
            .runtime
            .registry
            .agent_metadata_for_thread(grandchild.thread_id)
            .is_some()
    );

    let coordinator = &control.runtime.wake_coordinator;
    let root_assignment = coordinator
        .begin_or_continue_assignment(
            root.thread_id,
            None,
            "close-tree-root-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment starts");
    let child_assignment = coordinator
        .reserve_child_assignment(root_assignment.clone(), child.thread_id)
        .expect("child assignment is admitted")
        .commit()
        .expect("child assignment commits");
    coordinator
        .begin_or_continue_assignment(
            child.thread_id,
            Some(root_assignment),
            "close-tree-child-turn",
            /*allow_new_generation*/ false,
        )
        .expect("child turn starts");
    let grandchild_assignment = coordinator
        .reserve_child_assignment(child_assignment.clone(), grandchild.thread_id)
        .expect("grandchild assignment is admitted")
        .commit()
        .expect("grandchild assignment commits");
    coordinator
        .begin_or_continue_assignment(
            grandchild.thread_id,
            Some(child_assignment.clone()),
            "close-tree-grandchild-turn",
            /*allow_new_generation*/ false,
        )
        .expect("grandchild turn starts");
    assert_eq!(
        coordinator.classify_turn_end(
            &child_assignment,
            "close-tree-child-turn",
            TurnEndDisposition::Succeeded,
        ),
        Ok(AssignmentPhase::Waiting)
    );
    assert!(
        coordinator
            .interrupt_idle_assignment(&child_assignment, "close-tree-interrupt")
            .expect("interrupted child is still current")
    );

    let _ = control
        .close_agent(child.thread_id)
        .await
        .expect("close cancels the full registered subtree");

    assert!(manager.get_thread(grandchild.thread_id).await.is_err());
    assert!(
        control
            .runtime
            .registry
            .agent_metadata_for_thread(grandchild.thread_id)
            .is_none()
    );
    assert!(
        coordinator
            .current_assignment(grandchild.thread_id)
            .is_none()
    );
    assert_eq!(
        coordinator.assignment_status(child.thread_id),
        None,
        "closed assignment state is retired"
    );
    let _ = control.shutdown_live_agent(root.thread_id).await;
    let _ = grandchild_assignment;
}

#[tokio::test]
async fn internal_agent_died_interrupt_publishes_one_upward_report() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.runtime.upgrade().expect("thread manager is live");
    control.runtime.enable_wake_mode();
    control
        .runtime
        .registry
        .register_root_thread(root.thread_id);

    let child_path = AgentPath::try_from("/root/worker").expect("child path");
    let child_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            &control.runtime.admit_start().expect("admit residency work"),
            /*protected_thread_id*/ None,
        )
        .await
        .expect("reserve child residency");
    let child = spawn_v2_thread_spawn_agent(
        &control,
        &state,
        config,
        root.thread_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root.thread_id,
            depth: 1,
            agent_path: Some(child_path.clone()),
            agent_nickname: None,
            agent_role: Some("worker".to_string()),
        }),
    )
    .await;
    child_slot.commit(child.thread_id);
    control
        .runtime
        .registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("register child identity")
        .commit(AgentMetadata {
            agent_id: Some(child.thread_id),
            agent_path: Some(child_path.clone()),
            agent_role: Some("worker".to_string()),
            ..Default::default()
        });

    let coordinator = &control.runtime.wake_coordinator;
    let root_assignment = coordinator
        .begin_or_continue_assignment(
            root.thread_id,
            None,
            "dead-child-root-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment starts");
    let child_assignment = coordinator
        .reserve_child_assignment(root_assignment.clone(), child.thread_id)
        .expect("child assignment is admitted")
        .commit()
        .expect("child assignment commits");
    coordinator
        .begin_or_continue_assignment(
            child.thread_id,
            Some(root_assignment.clone()),
            "dead-child-turn",
            /*allow_new_generation*/ false,
        )
        .expect("child turn starts");

    child.thread.io.tx_sub.close();
    control
        .interrupt_spawned_agent(root.thread_id, child.thread_id)
        .await
        .expect("dead child interruption is reported as successful");

    assert!(manager.get_thread(child.thread_id).await.is_err());
    assert_eq!(
        coordinator.assignment_status(child.thread_id),
        Some(AgentStatus::Interrupted)
    );
    let report = control
        .runtime
        .claim_next_report_for_delivery(&root_assignment)
        .expect("interruption is reported to the parent");
    assert_eq!(report.sender_thread_id, child.thread_id);
    assert_eq!(report.communication.author, child_path);
    assert_eq!(report.communication.recipient, AgentPath::root());
    assert!(
        report
            .communication
            .content
            .to_lowercase()
            .contains("interrupted")
    );
    assert!(
        control
            .runtime
            .claim_next_report_for_delivery(&root_assignment)
            .is_none()
    );
    drop(report);

    let _ = control.shutdown_live_agent(root.thread_id).await;
    let _ = child_assignment;
}

#[test]
fn residency_slot_and_removal_release_wake_events() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let residency = Arc::new(V2Residency::new(Arc::clone(&coordinator)));
    assert!(residency.try_reserve_pending_slot(1));
    let slot = V2ResidencySlot {
        residency: Arc::clone(&residency),
        active: true,
    };
    let before_slot_release = coordinator.wake_event_epoch();
    drop(slot);
    assert!(coordinator.wake_event_epoch() > before_slot_release);

    let thread_id = ThreadId::new();
    residency.commit_slot(thread_id);
    let before_resident_removal = coordinator.wake_event_epoch();
    residency.remove(thread_id);
    assert!(coordinator.wake_event_epoch() > before_resident_removal);
}

async fn spawn_v2_thread_spawn_agent(
    control: &LocalAgentControl,
    state: &Arc<ThreadManagerState>,
    config: Config,
    parent_thread_id: ThreadId,
    source: SessionSource,
) -> crate::thread_manager::NewThread {
    state
        .spawn_new_thread_with_source(
            config,
            control.clone(),
            source,
            /*history_mode*/ None,
            Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
            /*reserved_thread_id*/ None,
        )
        .await
        .expect("spawn v2 thread-spawn agent")
}

async fn spawn_v2_subagent(
    control: &LocalAgentControl,
    state: &Arc<ThreadManagerState>,
    config: Config,
    parent_thread_id: ThreadId,
    label: &str,
) -> crate::thread_manager::NewThread {
    state
        .spawn_new_thread_with_source(
            config,
            control.clone(),
            SessionSource::SubAgent(SubAgentSource::Other(label.to_string())),
            /*history_mode*/ None,
            Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
            /*reserved_thread_id*/ None,
        )
        .await
        .expect("spawn v2 subagent")
}

async fn mark_thread_completed(thread: &CodexThread) {
    let turn = thread.session.new_default_turn().await;
    thread
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("done".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    clear_active_turn(thread).await;
}

async fn mark_thread_interrupted(thread: &CodexThread) {
    let turn = thread.session.new_default_turn().await;
    thread
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn.sub_id.clone()),
                started_at: None,
                reason: TurnAbortReason::Interrupted,
                error: None,
                completed_at: None,
                duration_ms: None,
            }),
        )
        .await;
    clear_active_turn(thread).await;
}

async fn clear_active_turn(thread: &CodexThread) {
    // The fixture has no task runner to clear the turn after the terminal event.
    *thread.session.active_turn.lock().await = None;
}
