//! A waiting parent whose last child generation is given back unstarted or closed learns of it
//! through the child's Interrupted report, delivered by the real wake dispatcher.

use crate::StartThreadOptions;
use crate::ThreadManager;
use crate::TurnInputRequest;
use crate::agent::LocalAgentControl;
use crate::agent::control::AssignmentPhase;
use crate::agent::control::TurnEndDisposition;
use crate::agent::types::AgentMetadata;
use crate::config::Config;
use crate::config::test_config;
use crate::thread_manager::NewThread;
use crate::thread_manager::ThreadManagerState;
use codex_features::AgentPolling;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use wiremock::MockServer;

struct Harness {
    _home: TempDir,
    _server: MockServer,
    model: responses::ResponseMock,
    config: Config,
    _manager: ThreadManager,
    state: Arc<ThreadManagerState>,
    root: NewThread,
    control: LocalAgentControl,
}

/// Starts a wake-mode root whose model answers `model_turns` turns, including the root's first
/// turn, which settles its multi-agent version. The wake dispatcher runs on the root's runtime.
async fn harness(model_turns: usize, root_waits_for_children: bool) -> Harness {
    let server = responses::start_mock_server().await;
    let model = responses::mount_sse_sequence(
        &server,
        (0..model_turns)
            .map(|turn| {
                let id = format!("response-{turn}");
                responses::sse(vec![
                    responses::ev_response_created(&id),
                    responses::ev_assistant_message(&format!("message-{turn}"), "done"),
                    responses::ev_completed(&id),
                ])
            })
            .collect(),
    )
    .await;
    let home = tempfile::tempdir().expect("create temp home");
    let mut config = test_config().await;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("enable v2");
    config.multi_agent_v2.agent_polling = AgentPolling::Disabled;
    config.codex_home = home.path().to_path_buf().try_into().expect("codex home");
    config.cwd = home.path().to_path_buf().try_into().expect("cwd");
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    config.model_provider.request_max_retries = Some(0);
    config.model_provider.stream_max_retries = Some(0);
    config.model_provider.supports_websockets = false;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions {
            session_source: Some(SessionSource::Cli),
            ..StartThreadOptions::new(config.clone())
        })
        .await
        .expect("start root thread");
    root.thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "initialize root".to_string(),
            text_elements: Vec::new(),
        }]))
        .await
        .expect("initial root turn");
    wait_for_turn_complete(&root).await;
    wait_until_idle(&root).await;

    let runtime = root.thread.session.services.local_agent_runtime.clone();
    runtime.register_session_root(root.thread_id, /*current_parent_thread_id*/ None);
    let control = LocalAgentControl {
        session_id: SessionId::from(root.thread_id),
        runtime,
    };
    control.start_wake_dispatcher(root_waits_for_children);
    let state = control.runtime.upgrade().expect("thread manager is live");
    Harness {
        _home: home,
        _server: server,
        model,
        config,
        _manager: manager,
        state,
        root,
        control,
    }
}

impl Harness {
    async fn spawn_child(&self, parent_thread_id: ThreadId, path: &str, depth: i32) -> NewThread {
        let agent_path = AgentPath::try_from(path).expect("agent path");
        let child = self
            .state
            .spawn_new_thread_with_source(
                self.config.clone(),
                self.control.clone(),
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth,
                    agent_path: Some(agent_path.clone()),
                    agent_nickname: None,
                    agent_role: None,
                }),
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
            .expect("spawn child thread");
        self.control
            .runtime
            .registry
            .reserve_spawn_slot(/*max_threads*/ None)
            .expect("register child identity")
            .commit(AgentMetadata {
                agent_id: Some(child.thread_id),
                agent_path: Some(agent_path),
                ..Default::default()
            });
        child
    }

    /// Drops the child's guard for `turn_id` the way a start that never installs its task does.
    async fn lose_start(&self, child: &NewThread, turn_id: &str) {
        let turn_context = child.thread.session.new_default_turn().await;
        let mut guard = child
            .thread
            .session
            .guard_unstarted_turn_assignment(turn_id);
        guard.record_turn_context(&turn_context);
        drop(guard);
    }
}

async fn wait_for_turn_complete(thread: &NewThread) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = thread.thread.next_event().await.expect("thread event");
            assert!(
                !matches!(event.msg, EventMsg::Error(_)),
                "turn failed: {:?}",
                event.msg
            );
            if let EventMsg::TurnComplete(event) = event.msg {
                assert_eq!(event.error, None);
                break;
            }
        }
    })
    .await
    .expect("turn completes");
}

async fn wait_until_idle(thread: &NewThread) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while thread.thread.session.active_turn.lock().await.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("thread becomes idle");
}

/// The root's wake turn recorded the child's interruption report, consumed it, and its successful
/// end completed the root assignment, which then left the coordinator.
async fn assert_root_woke_and_completed(harness: &Harness, note: &str) {
    wait_for_turn_complete(&harness.root).await;
    let coordinator = &harness.control.runtime.wake_coordinator;
    tokio::time::timeout(Duration::from_secs(10), async {
        while coordinator
            .current_assignment(harness.root.thread_id)
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the root's wake turn completes its assignment");
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body_contains_text("Agent was interrupted."));
    assert!(requests[1].body_contains_text(note));
    let history = harness.root.thread.session.clone_history().await;
    assert!(
        history
            .raw_items()
            .any(|item| { serde_json::to_string(item).is_ok_and(|item| item.contains(note)) })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_parent_wakes_on_the_report_of_a_child_generation_that_never_started() {
    let harness = harness(
        /*model_turns*/ 2, /*root_waits_for_children*/ false,
    )
    .await;
    let child = harness
        .spawn_child(harness.root.thread_id, "/root/worker", /*depth*/ 1)
        .await;
    let coordinator = &harness.control.runtime.wake_coordinator;
    let root = coordinator
        .begin_or_continue_assignment(
            harness.root.thread_id,
            None,
            "root-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment starts");
    // A followup to the idle child creates a fresh generation, and the root's turn ends waiting
    // on it while the child's start is still in flight.
    coordinator
        .begin_or_continue_assignment(
            child.thread_id,
            Some(root.clone()),
            "child-turn",
            /*allow_new_generation*/ true,
        )
        .expect("followup creates a child generation");
    assert_eq!(
        coordinator.classify_turn_end(&root, "root-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );

    harness.lose_start(&child, "child-turn").await;

    assert_root_woke_and_completed(&harness, "did not start").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_parent_wakes_on_the_report_of_its_closed_last_child() {
    let harness = harness(
        /*model_turns*/ 2, /*root_waits_for_children*/ false,
    )
    .await;
    let child = harness
        .spawn_child(harness.root.thread_id, "/root/worker", /*depth*/ 1)
        .await;
    let coordinator = &harness.control.runtime.wake_coordinator;
    let root = coordinator
        .begin_or_continue_assignment(
            harness.root.thread_id,
            None,
            "root-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment starts");
    let child_assignment = coordinator
        .reserve_child_assignment(root.clone(), child.thread_id)
        .expect("child assignment is admitted")
        .commit()
        .expect("child assignment commits");
    coordinator
        .begin_or_continue_assignment(
            child.thread_id,
            Some(root.clone()),
            "child-turn",
            /*allow_new_generation*/ false,
        )
        .expect("child turn starts");
    assert_eq!(
        coordinator.classify_turn_end(&root, "root-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );

    harness
        .control
        .close_agent(child.thread_id)
        .await
        .expect("close the child");

    assert_root_woke_and_completed(&harness, "was closed").await;
    assert!(!coordinator.is_current_open_assignment(&child_assignment));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_non_root_parent_woken_by_an_unstarted_child_reports_to_its_own_parent() {
    // The root's own reports are queue-only, so only the lead's wake turn reaches the model.
    let harness = harness(
        /*model_turns*/ 2, /*root_waits_for_children*/ true,
    )
    .await;
    let lead = harness
        .spawn_child(harness.root.thread_id, "/root/lead", /*depth*/ 1)
        .await;
    let worker = harness
        .spawn_child(lead.thread_id, "/root/lead/worker", /*depth*/ 2)
        .await;
    let coordinator = &harness.control.runtime.wake_coordinator;
    let root = coordinator
        .begin_or_continue_assignment(
            harness.root.thread_id,
            None,
            "root-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment starts");
    let lead_assignment = coordinator
        .reserve_child_assignment(root.clone(), lead.thread_id)
        .expect("lead assignment is admitted")
        .commit()
        .expect("lead assignment commits");
    coordinator
        .begin_or_continue_assignment(
            lead.thread_id,
            Some(root.clone()),
            "lead-turn",
            /*allow_new_generation*/ false,
        )
        .expect("lead turn starts");
    coordinator
        .begin_or_continue_assignment(
            worker.thread_id,
            Some(lead_assignment.clone()),
            "worker-turn",
            /*allow_new_generation*/ true,
        )
        .expect("followup creates a worker generation");
    assert_eq!(
        coordinator.classify_turn_end(&lead_assignment, "lead-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );

    harness.lose_start(&worker, "worker-turn").await;

    wait_for_turn_complete(&lead).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !coordinator.has_pending_reports(&root) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the lead's own report reaches the root");
    assert!(!coordinator.is_current_open_assignment(&lead_assignment));
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body_contains_text("did not start"));
}
