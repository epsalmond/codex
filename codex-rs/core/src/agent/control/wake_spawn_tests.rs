use super::*;
use crate::agent::control::LocalAgentRuntime;
use crate::agent::control::coordinator::MAX_OUTSTANDING_ASSIGNMENTS;
use crate::thread_manager::default_thread_id_generator;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::protocol::AgentStatus;
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
        control.runtime.wake_coordinator.assignment_status(root_thread_id),
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
