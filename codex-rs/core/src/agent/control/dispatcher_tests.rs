use super::super::coordinator::WakeDispatchResult;
use super::PendingWakeState;
use super::classify_pending_wake_after_start;
use super::validate_coordinator_reload_target;
use crate::tasks::PendingWorkStartResult;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

#[test]
fn coordinator_reload_validates_the_recorded_parent_path_and_version() {
    let parent = ThreadId::new();
    let other_parent = ThreadId::new();
    let path = AgentPath::try_from("/root/worker").expect("valid agent path");
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent,
        depth: 1,
        agent_path: Some(path.clone()),
        agent_nickname: None,
        agent_role: None,
    });

    assert!(
        validate_coordinator_reload_target(&source, Some(MultiAgentVersion::V2), parent, &path,)
            .is_ok()
    );
    assert!(
        validate_coordinator_reload_target(
            &source,
            Some(MultiAgentVersion::V2),
            other_parent,
            &path,
        )
        .is_err()
    );
    assert!(
        validate_coordinator_reload_target(&source, Some(MultiAgentVersion::V1), parent, &path,)
            .is_err()
    );
}

#[test]
fn trigger_mail_is_retained_when_capacity_disappears_after_dispatch_preflight() {
    assert_eq!(
        classify_pending_wake_after_start(
            PendingWorkStartResult::Deferred,
            PendingWakeState {
                trigger_mail_pending: true,
                wakeups_paused: false,
            },
        ),
        WakeDispatchResult::Defer,
    );
    assert_eq!(
        classify_pending_wake_after_start(
            PendingWorkStartResult::AlreadyActive,
            PendingWakeState {
                trigger_mail_pending: true,
                wakeups_paused: false,
            },
        ),
        WakeDispatchResult::Defer,
    );
    assert_eq!(
        classify_pending_wake_after_start(
            PendingWorkStartResult::Stale,
            PendingWakeState {
                trigger_mail_pending: true,
                wakeups_paused: true,
            },
        ),
        WakeDispatchResult::Complete,
    );
}
