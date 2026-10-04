use super::tests::make_session_and_context_with_session_source;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use pretty_assertions::assert_eq;
use std::sync::OnceLock;

#[tokio::test]
async fn interrupted_wake_attempt_reports_to_its_parent_once() {
    let parent_thread_id = ThreadId::new();
    let child_agent_path = AgentPath::try_from("/root/worker").expect("agent path");
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: Some(child_agent_path),
        agent_nickname: None,
        agent_role: None,
    });
    let (mut session, mut turn_context) =
        make_session_and_context_with_session_source(source).await;
    session.multi_agent_version = OnceLock::from(MultiAgentVersion::V2);
    turn_context.multi_agent_version = MultiAgentVersion::V2;
    let runtime = &session.services.local_agent_runtime;
    runtime.enable_wake_mode();

    let root_assignment = runtime
        .begin_wake_assignment_for_turn(
            parent_thread_id,
            &SessionSource::Cli,
            "parent-turn",
            /*allow_new_generation*/ true,
        )
        .expect("parent assignment starts")
        .expect("wake mode binds the parent");
    let child_assignment = runtime
        .begin_wake_assignment_for_turn(
            session.thread_id,
            &turn_context.session_source,
            &turn_context.sub_id,
            /*allow_new_generation*/ true,
        )
        .expect("child assignment starts")
        .expect("wake mode binds the child");
    turn_context
        .agent_assignment
        .set(child_assignment.clone())
        .expect("turn is bound to its assignment");
    assert_eq!(
        runtime.classify_wake_turn_end(
            &child_assignment,
            &turn_context.sub_id,
            crate::agent::control::TurnEndDisposition::Interrupted,
        ),
        Ok(crate::agent::control::AssignmentPhase::Interrupted)
    );

    let interrupted = EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: Some(turn_context.sub_id.clone()),
        reason: TurnAbortReason::Interrupted,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    });
    session
        .maybe_notify_parent_of_terminal_turn(&turn_context, &interrupted)
        .await;
    session
        .maybe_notify_parent_of_terminal_turn(&turn_context, &interrupted)
        .await;

    let report = runtime
        .claim_next_report_for_delivery(&root_assignment)
        .expect("interrupted report can be claimed through the delivery path");
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
        runtime.acknowledge_terminal_report_mailbox_delivery(&report_id, root_assignment.thread_id)
    );
    assert!(
        runtime
            .claim_next_report_for_delivery(&root_assignment)
            .is_none()
    );
}
