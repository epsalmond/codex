use crate::session::multi_agents::ChildReportMode;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_session_source_and_rx;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;

#[test_case::test_case(false, false; "ordinary path")]
#[test_case::test_case(true, false; "oversized valid path")]
#[test_case::test_case(false, true; "durable sleep queue-only mail")]
#[tokio::test]
async fn failed_automatic_binding_retains_accepted_followup_until_explicit_resume(
    oversized_path: bool,
    wake_from_sleep: bool,
) {
    let server = core_test_support::responses::start_mock_server().await;
    let parent_thread_id = ThreadId::new();
    let worker_path = AgentPath::try_from(if oversized_path {
        format!("/root/{}", "w".repeat(12_000))
    } else {
        "/root/worker".to_string()
    })
    .expect("valid worker path");
    let source = SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: Some(worker_path.clone()),
        agent_nickname: None,
        agent_role: None,
    });
    let (mut session, _, rx) =
        make_session_and_context_with_auth_and_config_and_session_source_and_rx(
            CodexAuth::from_api_key("test-api-key"),
            Vec::new(),
            source,
            |config| {
                config
                    .features
                    .enable(Feature::Collab)
                    .expect("enable collab");
                config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("enable v2");
                config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Disabled;
                config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                config.model_provider.request_max_retries = Some(0);
                config.model_provider.stream_max_retries = Some(0);
                config.model_provider.supports_websockets = false;
            },
        )
        .await;
    let responses = core_test_support::responses::mount_sse_sequence(
        &server,
        vec![
            core_test_support::responses::sse(vec![
                core_test_support::responses::ev_response_created("root-initial"),
                core_test_support::responses::ev_completed("root-initial"),
            ]),
            core_test_support::responses::sse(vec![
                core_test_support::responses::ev_response_created("root-startup-rejection"),
                core_test_support::responses::ev_completed("root-startup-rejection"),
            ]),
            core_test_support::responses::sse(vec![
                core_test_support::responses::ev_response_created("worker-explicit-recovery"),
                core_test_support::responses::ev_completed("worker-explicit-recovery"),
            ]),
        ],
    )
    .await;
    let mut root_config = (*session.get_config().await).clone();
    root_config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    root_config.model_provider.request_max_retries = Some(0);
    root_config.model_provider.stream_max_retries = Some(0);
    root_config.model_provider.supports_websockets = false;
    let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("test-api-key"),
        root_config.model_provider.clone(),
        root_config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(crate::StartThreadOptions {
            session_source: Some(SessionSource::Cli),
            ..crate::StartThreadOptions::new(root_config)
        })
        .await
        .expect("root thread");
    root.thread
        .start_or_steer_turn(crate::TurnInputRequest::user_input(vec![
            codex_protocol::user_input::UserInput::Text {
                text: "initialize root".to_string(),
                text_elements: Vec::new(),
            },
        ]))
        .await
        .expect("initial root turn");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = root.thread.next_event().await.expect("root event");
            assert!(
                !matches!(event.msg, EventMsg::Error(_)),
                "root initialization failed: {:?}",
                event.msg
            );
            if matches!(event.msg, EventMsg::TurnComplete(_)) {
                break;
            }
        }
    })
    .await
    .expect("initial root turn completes");
    assert_eq!(
        root.thread.session.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    // Real spawned children have a registered root. This isolated rejected-start fixture has
    // no spawn operation, so register the same root identity before sharing its runtime.
    root.thread
        .session
        .services
        .local_agent_runtime
        .register_session_root(
            root.thread.session.thread_id,
            /*current_parent_thread_id*/ None,
        );
    assert_eq!(
        root.thread.session.child_report_mode().await,
        Some(ChildReportMode::WakeOnReport)
    );
    let mutable = Arc::get_mut(&mut session).expect("session has no active task");
    mutable.services.local_agent_runtime = root.thread.session.services.local_agent_runtime.clone();
    mutable.services.agent_control = Arc::clone(&root.thread.session.services.agent_control);
    session.services.local_agent_runtime.enable_wake_mode();
    if wake_from_sleep {
        session
            .services
            .thread_extension_data
            .insert(codex_extension_items::sleep::SleepItem {
                id: "held-durable-sleep".to_string(),
                duration_ms: 60_000,
            });
    }
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        worker_path.clone(),
        Vec::new(),
        "accepted followup".to_string(),
        /*trigger_turn*/ !wake_from_sleep,
    );
    let options = crate::TurnStartOptions {
        parent_turn_id: Some("requester-turn".to_string()),
        root_turn_id: Some("root-turn".to_string()),
        turn_trigger: Some("composer".to_string()),
        ..Default::default()
    };
    session
        .input_queue
        .enqueue_mailbox_communication(communication.clone(), options.clone())
        .await;
    assert_eq!(
        session
            .maybe_start_turn_for_pending_work_with_sub_id("rejected-followup".to_string())
            .await,
        crate::tasks::PendingWorkStartResult::Stale,
    );
    assert!(session.active_turn.lock().await.is_none());
    assert!(session.input_queue.wakeups_paused());
    assert_eq!(
        session.input_queue.trigger_turn_mailbox_count().await,
        usize::from(!wake_from_sleep)
    );
    assert_eq!(
        session.maybe_start_turn_for_pending_work().await,
        crate::tasks::PendingWorkStartResult::Stale,
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = root
                .thread
                .next_event()
                .await
                .expect("root rejection event");
            assert!(
                !matches!(event.msg, EventMsg::Error(_)),
                "root rejection turn failed: {:?}",
                event.msg
            );
            if matches!(event.msg, EventMsg::TurnComplete(_)) {
                break;
            }
        }
    })
    .await
    .expect("root processes startup rejection");
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body_contains_text("Automatic turn did not start"));
    assert!(requests[1].body_contains_text("Accepted messages remain queued"));
    assert!(!requests[1].body_contains_text("accepted followup"));
    let rejection = requests[1]
        .input()
        .into_iter()
        .find(|item| {
            item["type"] == "agent_message"
                && item["content"][0]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Automatic turn did not start"))
        })
        .expect("rendered rejection fragment");
    assert!(
        codex_utils_output_truncation::approx_token_count(
            rejection["content"][0]["text"]
                .as_str()
                .expect("rejection text")
        ) <= 1_000
    );
    let mut warning = None;
    let mut startup_warning_count = 0;
    while let Ok(event) = rx.try_recv() {
        if let EventMsg::Warning(event) = event.msg
            && event.message.starts_with("Automatic turn did not start")
        {
            startup_warning_count += 1;
            warning = Some(event.message);
        }
    }
    assert_eq!(startup_warning_count, 1);
    let warning = warning.expect("startup failure is visible");
    insta::assert_snapshot!(warning, @"Automatic turn did not start: parent assignment is not active for this turn. Accepted messages remain queued; automatic wakeups are paused until an explicit follow-up or user turn.");
    if !wake_from_sleep {
        let (input, retained) = session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await;
        assert_eq!(input, Vec::new());
        assert_eq!(retained.parent_turn_id, None);
    }
    // Repair the missing owner, then recover through the actual explicit-followup handler.
    session
        .services
        .local_agent_runtime
        .begin_wake_assignment_for_turn(
            parent_thread_id,
            &SessionSource::Cli,
            "restored-owner",
            /*allow_new_generation*/ true,
        )
        .expect("restored parent assignment");
    super::handlers::inter_agent_communication(
        &session,
        "explicit-recovery".to_string(),
        InterAgentCommunication::new(
            AgentPath::root(),
            worker_path,
            Vec::new(),
            "explicit recovery".to_string(),
            /*trigger_turn*/ true,
        ),
        options.clone(),
    )
    .await;
    assert!(!session.input_queue.wakeups_paused());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("recovery event");
            assert!(
                !matches!(event.msg, EventMsg::Error(_)),
                "recovery failed: {:?}",
                event.msg
            );
            if matches!(event.msg, EventMsg::TurnComplete(_)) {
                break;
            }
        }
    })
    .await
    .expect("explicit recovery turn completes");
    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].body_contains_text("explicit recovery"));
    assert_eq!(
        requests[2]
            .input()
            .iter()
            .filter(|item| item["type"] == "agent_message"
                && item["content"][0]["text"] == "accepted followup")
            .count(),
        1
    );
    assert_eq!(session.input_queue.trigger_turn_mailbox_count().await, 0);
    assert_eq!(
        requests[2].body_json()["client_metadata"]["parent_turn_id"],
        serde_json::json!(options.parent_turn_id),
    );
    assert_eq!(
        requests[2].body_json()["client_metadata"]["root_turn_id"],
        serde_json::json!(options.root_turn_id),
    );
}
