use super::*;
use crate::agent::control::LocalAgentRuntime;
use crate::agent::control::TurnEndDisposition;
use crate::agent::control::coordinator::MAX_OUTSTANDING_ASSIGNMENTS;
use crate::thread_manager::default_thread_id_generator;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::sync::Weak;

#[test]
fn assignment_limit_rejects_spawn_before_any_child_thread_is_created() {
    let root_thread_id = ThreadId::new();
    let runtime = LocalAgentRuntime::new(
        Weak::new(),
        default_thread_id_generator(),
        /*rollout_budget*/ None,
    );
    runtime.enable_wake_mode();
    let control = LocalAgentControl {
        session_id: SessionId::from(root_thread_id),
        runtime,
    };
    let parent_assignment = control
        .runtime
        .wake_coordinator
        .begin_or_continue_assignment(
            root_thread_id,
            None,
            "root-active-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment starts");

    for _ in 0..MAX_OUTSTANDING_ASSIGNMENTS {
        control
            .runtime
            .wake_coordinator
            .reserve_child_assignment(parent_assignment.clone(), ThreadId::new())
            .expect("capacity admits assignments up to the configured bound")
            .commit()
            .expect("admitted assignment commits");
    }

    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root_thread_id,
        depth: 1,
        agent_path: Some(AgentPath::try_from("/root/worker").expect("valid path")),
        agent_nickname: None,
        agent_role: None,
    });
    let error = match control.reserve_wake_assignment(
        MultiAgentVersion::V2,
        codex_features::AgentPolling::Disabled,
        Some(&child_source),
        Some("root-active-turn"),
    ) {
        Ok(_) => panic!("assignment limit should reject the child before session creation"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("delegation was rejected"));
    assert!(
        error
            .to_string()
            .contains("outstanding delegated assignment limit reached")
    );
    assert_eq!(
        control
            .runtime
            .wake_coordinator
            .assignment_status(root_thread_id),
        Some(AgentStatus::Running)
    );
}

#[test]
fn explicit_polling_skips_wake_assignment_reservation() {
    let root_thread_id = ThreadId::new();
    let runtime = LocalAgentRuntime::new(
        Weak::new(),
        default_thread_id_generator(),
        /*rollout_budget*/ None,
    );
    runtime.enable_wake_mode();
    let control = LocalAgentControl {
        session_id: SessionId::from(root_thread_id),
        runtime,
    };
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root_thread_id,
        depth: 1,
        agent_path: Some(AgentPath::try_from("/root/worker").expect("valid path")),
        agent_nickname: None,
        agent_role: None,
    });

    assert!(
        control
            .reserve_wake_assignment(
                MultiAgentVersion::V2,
                codex_features::AgentPolling::Enabled,
                Some(&child_source),
                None,
            )
            .expect("polling mode does not need wake assignment")
            .is_none()
    );
}

#[test_case::test_case(false; "retained report")]
#[test_case::test_case(true; "consumed report")]
fn peer_followup_keeps_structural_parent_ownership(consume_report: bool) {
    let control = LocalAgentControl::default();
    let runtime = &control.runtime;
    runtime.enable_wake_mode();
    let coordinator = &runtime.wake_coordinator;
    let root_thread_id = ThreadId::new();
    let root = coordinator
        .begin_or_continue_assignment(
            root_thread_id,
            /*parent*/ None,
            "root-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment starts");
    let worker_thread_id = ThreadId::new();
    let worker = coordinator
        .reserve_child_assignment(root.clone(), worker_thread_id)
        .expect("worker reservation")
        .commit()
        .expect("worker assignment");
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root_thread_id,
        depth: 1,
        agent_path: Some(AgentPath::try_from("/root/worker").expect("worker path")),
        agent_nickname: None,
        agent_role: None,
    });
    runtime
        .begin_wake_assignment_for_turn(
            worker_thread_id,
            &source,
            "worker-turn",
            /*allow_new_generation*/ false,
        )
        .expect("worker starts");
    coordinator
        .classify_turn_end(&worker, "worker-turn", TurnEndDisposition::Succeeded)
        .expect("worker completes");
    coordinator
        .publish_terminal_report(
            &worker,
            "worker-turn",
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("worker path"),
                AgentPath::root(),
                Vec::new(),
                "initial result".to_string(),
                /*trigger_turn*/ true,
            ),
        )
        .expect("worker publishes report");
    let report = coordinator
        .claim_next_mailbox_report(&root)
        .expect("report is available");
    let report_id = report.id.clone();
    assert!(report.mark_enqueued());
    assert!(coordinator.acknowledge_report_mailbox_delivery(&report_id, root_thread_id));
    if consume_report {
        assert!(coordinator.mark_report_recorded(&report_id));
        assert_eq!(coordinator.accept_reports(&root, &[report_id]), 1);
        assert_eq!(coordinator.current_assignment(worker_thread_id), None);
    }

    let followup = runtime
        .begin_wake_assignment_for_turn(
            worker_thread_id,
            &source,
            "worker-followup",
            /*allow_new_generation*/ true,
        )
        .expect("peer follow-up starts")
        .expect("wake assignment");
    assert_ne!(followup, worker);
    assert_eq!(coordinator.parent_assignment(&followup), Some(root.clone()));
    coordinator
        .classify_turn_end(&followup, "worker-followup", TurnEndDisposition::Succeeded)
        .expect("follow-up completes");
    coordinator.cancel_assignment(&root);
    assert_eq!(
        runtime.begin_wake_assignment_for_turn(
            worker_thread_id,
            &source,
            "orphan-followup",
            /*allow_new_generation*/ true,
        ),
        Err("parent assignment is not active for this turn"),
    );
    assert_eq!(
        coordinator.current_assignment(worker_thread_id),
        Some(followup)
    );
}
