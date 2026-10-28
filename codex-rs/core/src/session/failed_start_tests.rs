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

#[test_case::test_case(false, false, false; "ordinary path")]
#[test_case::test_case(true, false, false; "oversized valid path")]
#[test_case::test_case(false, true, false; "durable sleep queue-only mail")]
#[test_case::test_case(false, true, true; "paused durable sleep queue-only mail")]
#[tokio::test]
async fn failed_automatic_binding_retains_accepted_followup_until_explicit_resume(
    oversized_path: bool,
    wake_from_sleep: bool,
    paused_first: bool,
) {
    failed_automatic_binding_recovery(
        oversized_path,
        wake_from_sleep,
        paused_first,
        /*resume_at*/ None,
    )
    .await;
}

#[test_case::test_case(false, false, false, crate::tasks::TurnStartHoldPoint::WakeBeforeBind; "ordinary followup before binding")]
#[test_case::test_case(true, false, false, crate::tasks::TurnStartHoldPoint::WakeBeforeBind; "oversized followup before binding")]
#[test_case::test_case(false, true, true, crate::tasks::TurnStartHoldPoint::WakeBeforeBind; "paused sleep followup before binding")]
#[test_case::test_case(false, false, false, crate::tasks::TurnStartHoldPoint::WakeBeforeSuppress; "followup after binding failure")]
#[tokio::test]
async fn failed_automatic_binding_does_not_stall_concurrent_explicit_resume(
    oversized_path: bool,
    wake_from_sleep: bool,
    paused_first: bool,
    resume_at: crate::tasks::TurnStartHoldPoint,
) {
    failed_automatic_binding_recovery(
        oversized_path,
        wake_from_sleep,
        paused_first,
        Some(resume_at),
    )
    .await;
}

async fn failed_automatic_binding_recovery(
    oversized_path: bool,
    wake_from_sleep: bool,
    paused_first: bool,
    resume_at: Option<crate::tasks::TurnStartHoldPoint>,
) {
    let resume_during_start = resume_at.is_some();
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
        .enqueue_mailbox_communication(communication.clone().into(), options.clone())
        .await;
    // A user pause lets queue-only mail wake the sleep; the rejected start then suppresses starts.
    if paused_first {
        session.input_queue.pause_wakeups();
    }
    let recovery = InterAgentCommunication::new(
        AgentPath::root(),
        worker_path.clone(),
        Vec::new(),
        "explicit recovery".to_string(),
        /*trigger_turn*/ true,
    );
    let pending_start =
        session.maybe_start_turn_for_pending_work_with_sub_id("rejected-followup".to_string());
    let restore_owner = || {
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
    };
    let start_result = if let Some(resume_at) = resume_at {
        let mut hold = crate::tasks::hold_next_turn_start(session.thread_id, resume_at);
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(pending_start, async {
                hold.reached().await;
                assert_eq!(session.input_queue.wakeups_paused(), paused_first);
                // Follow up through the real handler while the failed start owns the slot.
                // Recovery must be recorded even though wakeups are already active.
                super::handlers::inter_agent_communication(
                    &session,
                    "explicit-recovery".to_string(),
                    recovery.clone().into(),
                    options.clone(),
                )
                .await;
                assert_eq!(session.input_queue.trigger_turn_mailbox_count().await, 1);
                if resume_at == crate::tasks::TurnStartHoldPoint::WakeBeforeBind {
                    let mut failed = crate::tasks::hold_next_turn_start(
                        session.thread_id,
                        crate::tasks::TurnStartHoldPoint::WakeBeforeSuppress,
                    );
                    hold.release();
                    failed.reached().await;
                    // Repair only after the original bind has failed. The failed start must
                    // schedule the accepted recovery itself after releasing its reservation.
                    restore_owner();
                    failed.release();
                } else {
                    restore_owner();
                    hold.release();
                }
            })
            .0
        })
        .await
        .expect("parked start fails after explicit resume")
    } else {
        pending_start.await
    };
    assert_eq!(start_result, crate::tasks::PendingWorkStartResult::Stale);
    if !resume_during_start {
        assert!(session.active_turn.lock().await.is_none());
    }
    assert_eq!(session.input_queue.wakeups_paused(), !resume_during_start);
    assert_eq!(
        session.input_queue.automatic_starts_suppressed(),
        !resume_during_start
    );
    if paused_first && !resume_during_start {
        // Fresh queue-only mail would wake a merely paused sleep, but not after a failed start.
        session
            .input_queue
            .enqueue_mailbox_communication(
                InterAgentCommunication::new(
                    AgentPath::root(),
                    worker_path.clone(),
                    Vec::new(),
                    "fresh progress".to_string(),
                    /*trigger_turn*/ false,
                )
                .into(),
                Default::default(),
            )
            .await;
    }
    if !resume_during_start {
        assert_eq!(
            session.input_queue.trigger_turn_mailbox_count().await,
            usize::from(!wake_from_sleep)
        );
        assert_eq!(
            session.maybe_start_turn_for_pending_work().await,
            crate::tasks::PendingWorkStartResult::Stale,
        );
    }
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
    let rejection_request = requests
        .iter()
        .find(|request| request.body_contains_text("Automatic turn did not start"))
        .expect("startup rejection reaches root");
    assert!(rejection_request.body_contains_text("Accepted messages remain queued"));
    assert!(!rejection_request.body_contains_text("accepted followup"));
    let rejection = rejection_request
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
    let mut recovered = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event.msg, EventMsg::TurnComplete(_)) {
            recovered = true;
        }
        if let EventMsg::Warning(event) = event.msg
            && event.message.starts_with("Automatic turn did not start")
        {
            startup_warning_count += 1;
            warning = Some(event.message);
        }
    }
    assert_eq!(startup_warning_count, 1);
    let warning = warning.expect("startup failure is visible");
    if resume_during_start {
        assert_eq!(
            warning,
            "Automatic turn did not start: parent assignment is not active for this turn. Accepted messages remain queued; automatic wakeups have already been resumed by an explicit follow-up or user turn."
        );
    } else {
        assert_eq!(
            warning,
            "Automatic turn did not start: parent assignment is not active for this turn. Accepted messages remain queued; automatic wakeups are paused until an explicit follow-up or user turn."
        );
    }
    if !wake_from_sleep && !resume_during_start {
        let (input, retained) = session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await;
        assert_eq!(input, Vec::new());
        assert_eq!(retained.parent_turn_id, None);
    }
    if !resume_during_start {
        restore_owner();
        super::handlers::inter_agent_communication(
            &session,
            "explicit-recovery".to_string(),
            recovery.into(),
            options.clone(),
        )
        .await;
    }
    assert!(!session.input_queue.wakeups_paused());
    tokio::time::timeout(Duration::from_secs(10), async {
        while !recovered {
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
    let recovery_request = requests
        .iter()
        .find(|request| request.body_contains_text("explicit recovery"))
        .expect("recovery reaches worker");
    assert_eq!(
        recovery_request
            .input()
            .iter()
            .filter(|item| item["type"] == "agent_message"
                && item["content"][0]["text"] == "accepted followup")
            .count(),
        1
    );
    assert_eq!(session.input_queue.trigger_turn_mailbox_count().await, 0);
    assert_eq!(
        recovery_request.body_json()["client_metadata"]["parent_turn_id"],
        serde_json::json!(options.parent_turn_id),
    );
    assert_eq!(
        recovery_request.body_json()["client_metadata"]["root_turn_id"],
        serde_json::json!(options.root_turn_id),
    );
}
