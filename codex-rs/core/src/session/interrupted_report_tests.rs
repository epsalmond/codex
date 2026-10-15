use super::tests::make_session_and_context;
use super::tests::make_session_and_context_with_session_source;
use crate::compact::CompactedHistoryMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
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
        runtime
            .classify_wake_turn_end(
                &child_assignment,
                &turn_context.sub_id,
                crate::agent::control::TurnEndDisposition::Interrupted,
            )
            .await,
        Ok(crate::agent::control::AssignmentPhase::Interrupted)
    );

    let interrupted = EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: Some(turn_context.sub_id.clone()),
        reason: TurnAbortReason::Interrupted,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        root_turn_id: None,
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

#[tokio::test]
async fn compaction_requeues_only_trigger_reports_omitted_from_replacement_history() {
    let (mut parent, mut parent_turn) = make_session_and_context().await;
    parent.multi_agent_version = OnceLock::from(MultiAgentVersion::V2);
    parent_turn.multi_agent_version = MultiAgentVersion::V2;
    let runtime = parent.services.local_agent_runtime.clone();
    runtime.enable_wake_mode();
    let parent_assignment = runtime
        .begin_wake_assignment_for_turn(
            parent.thread_id,
            &SessionSource::Cli,
            &parent_turn.sub_id,
            /*allow_new_generation*/ true,
        )
        .expect("parent assignment starts")
        .expect("wake mode binds the parent");

    let first_report = publish_successful_report(
        parent.thread_id,
        &runtime,
        "/root/worker_one",
        "first child result",
    )
    .await;
    let second_report = publish_successful_report(
        parent.thread_id,
        &runtime,
        "/root/worker_two",
        "second child result",
    )
    .await;
    let first_id = first_report.id.clone().expect("first report has an ID");
    let second_id = second_report.id.clone().expect("second report has an ID");
    for report in [&first_report, &second_report] {
        parent
            .record_inter_agent_communication(
                &parent_turn,
                parent_turn.model_info(),
                report.clone(),
            )
            .await;
        assert!(
            runtime.mark_wake_report_recorded(report.id.as_ref().expect("report has a stable ID"))
        );
    }

    // Simulate compaction retaining the second report while dropping the first one.
    let (window_number, window_ids) = parent.advance_auto_compact_window().await;
    assert!(
        parent
            .replace_compacted_history(
                vec![ResponseItemEnvelope::new(
                    second_report.to_model_input_item(),
                )],
                /*reference_context_item*/ None,
                /*world_state_baseline*/ None,
                CompactedHistoryMetadata {
                    input_goal_ids: Default::default(),
                    message: "test compaction".to_string(),
                    window_number,
                    window_ids,
                    compaction_response_id: None,
                    compaction_model_hash: None,
                    shake_watermark_index: None,
                    reviewer_compaction_hash: None,
                },
            )
            .await
    );

    let redelivered = runtime
        .claim_next_report_for_delivery(&parent_assignment)
        .expect("the omitted report is returned to the mailbox");
    assert_eq!(redelivered.id, first_id);
    let first_report = redelivered.communication.clone();
    assert!(redelivered.mark_enqueued());
    assert!(runtime.acknowledge_terminal_report_mailbox_delivery(&first_id, parent.thread_id));
    parent
        .record_inter_agent_communication(&parent_turn, parent_turn.model_info(), first_report)
        .await;
    assert!(runtime.mark_wake_report_recorded(&first_id));

    let compacted_history = parent.clone_history().await;
    let prompt_items = compacted_history
        .annotated_items()
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        runtime.wake_report_ids_in_prompt(&parent_assignment, &prompt_items),
        vec![first_id.clone(), second_id.clone()]
    );
    assert_eq!(
        runtime.accept_wake_reports(&parent_assignment, &[first_id, second_id],),
        2
    );
    assert!(!runtime.has_recorded_wake_reports(&parent_assignment));
}

async fn publish_successful_report(
    parent_thread_id: ThreadId,
    runtime: &crate::agent::control::LocalAgentRuntime,
    agent_path: &str,
    message: &str,
) -> codex_protocol::protocol::InterAgentCommunication {
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: Some(AgentPath::try_from(agent_path).expect("agent path")),
        agent_nickname: None,
        agent_role: None,
    });
    let (mut child, mut child_turn) =
        make_session_and_context_with_session_source(source.clone()).await;
    child.thread_id = ThreadId::new();
    child.multi_agent_version = OnceLock::from(MultiAgentVersion::V2);
    child.services.local_agent_runtime = runtime.clone();
    child.services.agent_control =
        std::sync::Arc::new(runtime.control(SessionId::from(parent_thread_id)));
    child_turn.multi_agent_version = MultiAgentVersion::V2;
    let child_assignment = runtime
        .begin_wake_assignment_for_turn(
            child.thread_id,
            &source,
            &child_turn.sub_id,
            /*allow_new_generation*/ true,
        )
        .expect("child assignment starts")
        .expect("wake mode binds the child");
    child_turn
        .agent_assignment
        .set(child_assignment.clone())
        .expect("turn is bound to its assignment");
    assert_eq!(
        runtime
            .classify_wake_turn_end(
                &child_assignment,
                &child_turn.sub_id,
                crate::agent::control::TurnEndDisposition::Succeeded,
            )
            .await,
        Ok(crate::agent::control::AssignmentPhase::Completed)
    );
    child
        .maybe_notify_parent_of_terminal_turn(
            &child_turn,
            &EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: child_turn.sub_id.clone(),
                last_agent_message: Some(message.to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                root_turn_id: None,
            }),
        )
        .await;
    let report = runtime
        .claim_next_report_for_delivery(
            &runtime
                .current_wake_assignment(parent_thread_id)
                .expect("parent assignment remains current"),
        )
        .expect("terminal report is published");
    let report_id = report.id.clone();
    let communication = report.communication.clone();
    assert!(report.mark_enqueued());
    assert!(runtime.acknowledge_terminal_report_mailbox_delivery(&report_id, parent_thread_id));
    communication
}
