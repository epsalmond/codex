use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use super::AssignmentPhase;
use super::MAX_OUTSTANDING_ASSIGNMENTS;
use super::TurnEndDisposition;
use super::reports::TerminalReportPublication;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use std::sync::Arc;

fn new_root(coordinator: &AgentWakeCoordinator) -> AgentAssignmentId {
    coordinator
        .begin_or_continue_assignment(
            ThreadId::new(),
            None,
            "root-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment starts")
}

fn new_child(
    coordinator: &Arc<AgentWakeCoordinator>,
    parent: &AgentAssignmentId,
    initial_turn_id: &str,
) -> AgentAssignmentId {
    let id = coordinator
        .reserve_child_assignment(parent.clone(), ThreadId::new())
        .expect("child assignment is admitted")
        .commit()
        .expect("parent still owns the reserved assignment");
    coordinator
        .begin_or_continue_assignment(
            id.thread_id,
            Some(parent.clone()),
            initial_turn_id,
            /*allow_new_generation*/ false,
        )
        .expect("committed child starts its first turn");
    id
}

fn terminal_report(
    coordinator: &AgentWakeCoordinator,
    child: &AgentAssignmentId,
    terminal_turn_id: &str,
) -> InterAgentCommunication {
    coordinator
        .classify_turn_end(child, terminal_turn_id, TurnEndDisposition::Succeeded)
        .expect("child turn is classified");
    let parent = coordinator
        .state
        .lock()
        .expect("coordinator lock is healthy")
        .assignments
        .get(child)
        .and_then(|assignment| assignment.parent.clone())
        .expect("child has a parent");
    let report_id = match coordinator
        .publish_terminal_report(
            child,
            terminal_turn_id,
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("valid child path"),
                AgentPath::root(),
                Vec::new(),
                "delegated work finished".to_string(),
                /*trigger_turn*/ true,
            ),
        )
        .expect("active parent receives terminal report")
    {
        TerminalReportPublication::Published(report_id) => report_id,
        TerminalReportPublication::AlreadyPublished(_) => panic!("first publication is new"),
    };
    coordinator
        .claim_pending_mailbox_reports(&parent)
        .into_iter()
        .find(|report| report.id.as_ref() == Some(&report_id))
        .expect("claimed report contains the published ID")
}

fn input_with_report(report: &InterAgentCommunication) -> Vec<ResponseItem> {
    vec![report.to_model_input_item()]
}

fn outstanding(coordinator: &AgentWakeCoordinator) -> usize {
    coordinator
        .state
        .lock()
        .expect("coordinator lock is healthy")
        .outstanding_assignments
}

fn direct_children(coordinator: &AgentWakeCoordinator, parent: &AgentAssignmentId) -> usize {
    coordinator
        .state
        .lock()
        .expect("coordinator lock is healthy")
        .assignments
        .get(parent)
        .map_or(0, |assignment| assignment.direct_children.len())
}

#[test]
fn admission_is_bounded_and_uncommitted_reservations_roll_back() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let reservation = coordinator
        .reserve_child_assignment(root.clone(), ThreadId::new())
        .expect("first child assignment is admitted");
    assert_eq!(outstanding(&coordinator), 1);
    drop(reservation);
    assert_eq!(outstanding(&coordinator), 0);

    for _ in 0..MAX_OUTSTANDING_ASSIGNMENTS {
        new_child(&coordinator, &root, "child-turn");
    }
    assert_eq!(outstanding(&coordinator), MAX_OUTSTANDING_ASSIGNMENTS);
    assert!(matches!(
        coordinator.reserve_child_assignment(root, ThreadId::new()),
        Err("outstanding delegated assignment limit reached")
    ));
}

#[test]
fn active_and_waiting_turns_keep_their_generation_and_terminal_followup_advances_it() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "child-first-turn");
    let grandchild = new_child(&coordinator, &child, "grandchild-final-turn");

    assert_eq!(
        coordinator.classify_turn_end(&child, "child-first-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    let steered = coordinator
        .begin_or_continue_assignment(
            child.thread_id,
            Some(root.clone()),
            "child-final-turn",
            /*allow_new_generation*/ true,
        )
        .expect("followup to a waiting assignment keeps its generation");
    assert_eq!(steered, child);

    let report = terminal_report(&coordinator, &grandchild, "grandchild-final-turn");
    assert!(coordinator.mark_report_recorded(report.id.as_ref().expect("stable report ID")));
    let candidates = coordinator.report_ids_in_prompt(&child, &input_with_report(&report));
    assert_eq!(
        coordinator.accept_reports(&child, &candidates),
        1,
        "accepted grandchild report clears the child obligation"
    );
    assert_eq!(
        coordinator.classify_turn_end(&child, "child-final-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Completed)
    );
    let child_report_id = match coordinator
        .publish_terminal_report(
            &child,
            "child-final-turn",
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("valid child path"),
                AgentPath::root(),
                Vec::new(),
                "child finished after processing the grandchild report".to_string(),
                /*trigger_turn*/ true,
            ),
        )
        .expect("completed child reports to its parent")
    {
        TerminalReportPublication::Published(report_id) => report_id,
        TerminalReportPublication::AlreadyPublished(_) => panic!("first publication is new"),
    };
    let child_report = coordinator
        .claim_pending_mailbox_reports(&root)
        .into_iter()
        .find(|report| report.id.as_ref() == Some(&child_report_id))
        .expect("claimed report contains the published ID");
    coordinator.mark_report_recorded(&child_report_id);
    let child_candidates =
        coordinator.report_ids_in_prompt(&root, &input_with_report(&child_report));
    assert_eq!(coordinator.accept_reports(&root, &child_candidates), 1);
    let next = coordinator
        .begin_or_continue_assignment(
            child.thread_id,
            Some(root),
            "child-next-generation-turn",
            /*allow_new_generation*/ true,
        )
        .expect("explicit followup after completion starts a new generation");
    assert_ne!(next, child);
}

#[test]
fn mailbox_recording_does_not_consume_a_report_without_prompt_acceptance() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "child-turn");
    coordinator
        .classify_turn_end(&child, "child-turn", TurnEndDisposition::Succeeded)
        .expect("child turn is classified");
    let report_id = match coordinator
        .publish_terminal_report(
            &child,
            "child-turn",
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("valid child path"),
                AgentPath::root(),
                Vec::new(),
                "delegated work finished".to_string(),
                /*trigger_turn*/ true,
            ),
        )
        .expect("active parent receives terminal report")
    {
        TerminalReportPublication::Published(report_id) => report_id,
        TerminalReportPublication::AlreadyPublished(_) => panic!("first publication is new"),
    };
    let wake_request = coordinator
        .claim_next_wake_request()
        .expect("report publication queues its parent");
    assert_eq!(wake_request.assignment(), &parent);
    wake_request.complete();
    assert!(!coordinator.mark_report_recorded(&report_id));
    let report = coordinator
        .claim_pending_mailbox_reports(&parent)
        .into_iter()
        .find(|report| report.id.as_ref() == Some(&report_id))
        .expect("claim returns the published report payload");

    assert!(
        coordinator
            .claim_pending_mailbox_reports(&parent)
            .is_empty()
    );
    assert!(coordinator.release_mailbox_claim(&report_id));
    assert_eq!(
        coordinator.claim_pending_mailbox_reports(&parent),
        vec![report.clone()]
    );
    assert!(matches!(
        coordinator.publish_terminal_report(
            &child,
            "child-turn",
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("valid child path"),
                AgentPath::root(),
                Vec::new(),
                "duplicate payload should not create a second report".to_string(),
                /*trigger_turn*/ true,
            ),
        ),
        Some(TerminalReportPublication::AlreadyPublished(published_id))
            if published_id == report_id
    ));
    assert!(coordinator.claim_next_wake_request().is_none());
    assert!(coordinator.mark_report_recorded(&report_id));
    assert!(
        coordinator
            .claim_pending_mailbox_reports(&parent)
            .is_empty()
    );
    assert!(coordinator.report_ids_in_prompt(&parent, &[]).is_empty());
    assert_eq!(direct_children(&coordinator, &parent), 1);
    assert_eq!(outstanding(&coordinator), 1);

    let request_snapshot = input_with_report(&report);
    let accepted_candidates = coordinator.report_ids_in_prompt(&parent, &request_snapshot);

    let late_child = new_child(&coordinator, &parent, "late-child-turn");
    let late_report = terminal_report(&coordinator, &late_child, "late-child-turn");
    let late_id = late_report.id.clone().expect("late report ID");
    coordinator.mark_report_recorded(&late_id);

    assert_eq!(accepted_candidates, vec![report_id]);
    assert_eq!(coordinator.accept_reports(&parent, &accepted_candidates), 1);
    assert!(coordinator.has_pending_reports(&parent));
    let next_prompt = [request_snapshot, input_with_report(&late_report)].concat();
    assert_eq!(
        coordinator.report_ids_in_prompt(&parent, &next_prompt),
        vec![late_id.clone()]
    );
    assert_eq!(outstanding(&coordinator), 1);

    assert_eq!(
        coordinator.classify_turn_end(&parent, "root-turn", TurnEndDisposition::Interrupted,),
        Ok(AssignmentPhase::Interrupted)
    );
    let fresh_parent = coordinator
        .begin_or_continue_assignment(
            parent.thread_id,
            None,
            "fresh-root-turn",
            /*allow_new_generation*/ true,
        )
        .expect("explicit parent followup starts a fresh generation");
    assert_ne!(fresh_parent, parent);
    assert!(
        coordinator
            .report_ids_in_prompt(&fresh_parent, &next_prompt)
            .is_empty()
    );
    assert_eq!(coordinator.accept_reports(&fresh_parent, &[late_id]), 0);
    assert!(!coordinator.has_pending_reports(&fresh_parent));
    assert_eq!(direct_children(&coordinator, &fresh_parent), 0);
    assert_eq!(outstanding(&coordinator), 0);
}

#[test]
fn interrupted_child_requires_explicit_followup_and_then_gets_a_fresh_generation() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "child-interrupted-turn");
    let interruption = coordinator
        .classify_turn_end(
            &child,
            "child-interrupted-turn",
            TurnEndDisposition::Interrupted,
        )
        .expect("child interruption is classified");
    assert_eq!(interruption, AssignmentPhase::Interrupted);
    let report_id = match coordinator
        .publish_terminal_report(
            &child,
            "child-interrupted-turn",
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("valid child path"),
                AgentPath::root(),
                Vec::new(),
                "child turn interrupted".to_string(),
                /*trigger_turn*/ true,
            ),
        )
        .expect("interrupted attempt reports once")
    {
        TerminalReportPublication::Published(report_id) => report_id,
        TerminalReportPublication::AlreadyPublished(_) => panic!("first publication is new"),
    };
    let report = coordinator
        .claim_pending_mailbox_reports(&parent)
        .into_iter()
        .find(|report| report.id.as_ref() == Some(&report_id))
        .expect("claim returns the published report payload");
    coordinator.mark_report_recorded(&report_id);
    let candidates = coordinator.report_ids_in_prompt(&parent, &input_with_report(&report));
    assert_eq!(coordinator.accept_reports(&parent, &candidates), 1);

    assert!(
        coordinator
            .begin_or_continue_assignment(
                child.thread_id,
                Some(parent.clone()),
                "child-fresh-turn",
                /*allow_new_generation*/ false,
            )
            .is_err()
    );
    let resumed = coordinator
        .begin_or_continue_assignment(
            child.thread_id,
            Some(parent),
            "child-fresh-turn",
            /*allow_new_generation*/ true,
        )
        .expect("explicit followup clears the interruption pause");
    assert_ne!(resumed, child);
}

#[path = "coordinator_invariant_tests.rs"]
mod invariant_tests;
