use super::*;
use codex_protocol::AgentPath;

#[test]
fn assignment_reservation_commit_rejects_parent_interruption() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let reservation = coordinator
        .reserve_child_assignment(root.clone(), ThreadId::new())
        .expect("child assignment is reserved");
    assert_eq!(
        coordinator.classify_turn_end(&root, "root-turn", TurnEndDisposition::Interrupted),
        Ok(AssignmentPhase::Interrupted)
    );
    assert!(reservation.commit().is_err());
    assert_eq!(outstanding(&coordinator), 0);
}

#[test]
fn reserved_assignment_cannot_start_a_turn_before_commit() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let reservation = coordinator
        .reserve_child_assignment(root.clone(), ThreadId::new())
        .expect("child assignment is reserved");
    let child = reservation.id().clone();

    assert!(
        coordinator
            .begin_or_continue_assignment(
                child.thread_id,
                Some(root.clone()),
                "child-turn",
                /*allow_new_generation*/ false,
            )
            .is_err()
    );
    assert_eq!(reservation.commit(), Ok(child.clone()));
    assert_eq!(
        coordinator.begin_or_continue_assignment(
            child.thread_id,
            Some(root),
            "child-turn",
            /*allow_new_generation*/ false,
        ),
        Ok(child)
    );
}

#[test]
fn wake_queue_coalesces_targets_and_defers_fairly() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let first = new_child(&coordinator, &root, "first-turn");
    let second = new_child(&coordinator, &root, "second-turn");

    assert!(coordinator.request_wake(first.clone()));
    assert!(!coordinator.request_wake(first.clone()));
    assert!(coordinator.request_wake(second.clone()));
    let request = coordinator
        .claim_next_wake_request()
        .expect("oldest wake is claimed");
    assert_eq!(request.assignment(), Some(&first));
    request.defer();
    let request = coordinator
        .claim_next_wake_request()
        .expect("deferred wake rotates behind its sibling");
    assert_eq!(request.assignment(), Some(&second));
    request.complete();
    let request = coordinator
        .claim_next_wake_request()
        .expect("deferred wake is retained");
    assert_eq!(request.assignment(), Some(&first));
    request.complete();
}

#[test]
fn wake_queue_discards_a_terminal_assignment_generation() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "interrupted-turn");

    assert!(coordinator.request_wake(child.clone()));
    assert_eq!(
        coordinator.classify_turn_end(&child, "interrupted-turn", TurnEndDisposition::Interrupted,),
        Ok(AssignmentPhase::Interrupted)
    );
    assert!(coordinator.claim_next_wake_request().is_none());
}

#[test]
fn wake_queue_rejects_a_reserved_target_until_its_turn_starts() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let reservation = coordinator
        .reserve_child_assignment(root.clone(), ThreadId::new())
        .expect("child assignment is reserved");
    let child = reservation.id().clone();

    assert!(!coordinator.request_wake(child.clone()));
    assert_eq!(reservation.commit(), Ok(child.clone()));
    assert!(!coordinator.request_wake(child.clone()));
    coordinator
        .begin_or_continue_assignment(
            child.thread_id,
            Some(root),
            "child-turn",
            /*allow_new_generation*/ false,
        )
        .expect("committed child starts its first turn");
    assert!(coordinator.request_wake(child.clone()));
    let request = coordinator
        .claim_next_wake_request()
        .expect("started child is wakeable");
    assert_eq!(request.assignment(), Some(&child));
    request.complete();
}

#[test]
fn reload_authority_tracks_current_parent_and_root_generations() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "child-turn");
    let grandchild = new_child(&coordinator, &child, "grandchild-turn");

    let authority = coordinator
        .reload_authority(&grandchild)
        .expect("current nested assignment has reload authority");
    assert_eq!(authority.parent_thread_id, child.thread_id);
    assert_eq!(authority.root_thread_id, root.thread_id);

    coordinator
        .classify_turn_end(&child, "child-turn", TurnEndDisposition::Interrupted)
        .expect("parent interruption detaches nested work");
    assert!(coordinator.reload_authority(&grandchild).is_none());
}

#[test]
fn subtree_close_retires_assignments_and_pending_reports_before_shutdown() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "child-turn");
    let grandchild = new_child(&coordinator, &child, "grandchild-turn");
    assert_eq!(
        coordinator.classify_turn_end(
            &grandchild,
            "grandchild-turn",
            TurnEndDisposition::Succeeded,
        ),
        Ok(AssignmentPhase::Completed)
    );
    let communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/child/grandchild").expect("valid path"),
        AgentPath::try_from("/root/child").expect("valid path"),
        Vec::new(),
        "done".to_string(),
        true,
    );
    assert!(
        coordinator
            .publish_terminal_report(&grandchild, "grandchild-turn", communication)
            .is_some()
    );
    assert!(coordinator.has_pending_reports(&child));

    coordinator.cancel_subtree(&[child.thread_id]);

    assert!(coordinator.current_assignment(child.thread_id).is_none());
    assert!(
        coordinator
            .current_assignment(grandchild.thread_id)
            .is_none()
    );
    assert!(!coordinator.has_pending_reports(&child));
    assert!(coordinator.is_current_open_assignment(&root));
}

#[test]
fn subtree_close_cancels_grandchildren_after_parent_interruption_detaches_them() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "child-turn");
    let grandchild = new_child(&coordinator, &child, "grandchild-turn");

    assert_eq!(
        coordinator.classify_turn_end(&child, "child-turn", TurnEndDisposition::Interrupted,),
        Ok(AssignmentPhase::Interrupted)
    );
    assert_eq!(coordinator.parent_assignment(&grandchild), None);

    coordinator.cancel_subtree(&[child.thread_id, grandchild.thread_id]);

    // The child's turn ended before the close, so it stays for the report that turn publishes.
    assert_eq!(
        coordinator.terminal_assignment_for_turn(child.thread_id, "child-turn"),
        Some(child)
    );
    assert!(
        coordinator
            .current_assignment(grandchild.thread_id)
            .is_none()
    );
    assert!(coordinator.is_current_open_assignment(&root));
}

#[test]
fn production_report_claim_retries_after_failed_send_and_stops_after_enqueue() {
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

    let failed_send = coordinator
        .claim_next_mailbox_report(&parent)
        .expect("pending report can be claimed for delivery");
    assert_eq!(failed_send.id, report_id);
    drop(failed_send);

    let retry = coordinator
        .claim_next_mailbox_report(&parent)
        .expect("dropping a failed-send claim releases it for retry");
    assert_eq!(retry.id, report_id);
    assert!(retry.mark_enqueued());
    assert!(coordinator.acknowledge_report_mailbox_delivery(&report_id, parent.thread_id));
    assert!(coordinator.claim_next_mailbox_report(&parent).is_none());
    assert!(!coordinator.release_mailbox_claim(&report_id));
    assert_eq!(direct_children(&coordinator, &parent), 1);

    assert!(coordinator.mark_report_recorded(&report_id));
    let communication = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("valid child path"),
        AgentPath::root(),
        Vec::new(),
        "delegated work finished".to_string(),
        /*trigger_turn*/ true,
    );
    let communication = InterAgentCommunication {
        id: Some(report_id.clone()),
        ..communication
    };
    let prompt_input = vec![communication.to_model_input_item()];
    let accepted = coordinator.report_ids_in_prompt(&parent, &prompt_input);
    assert_eq!(accepted, vec![report_id]);
    assert_eq!(coordinator.accept_reports(&parent, &accepted), 1);
    assert_eq!(outstanding(&coordinator), 0);
}

#[test]
fn wake_queue_retries_once_for_reports_arriving_during_delivery() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "child-turn");

    assert!(coordinator.request_wake(child.clone()));
    let request = coordinator
        .claim_next_wake_request()
        .expect("wake is claimed");
    assert_eq!(request.assignment(), Some(&child));
    assert!(!coordinator.request_wake(child.clone()));
    assert!(!coordinator.request_wake(child.clone()));
    request.complete();
    let request = coordinator
        .claim_next_wake_request()
        .expect("one additional wake is queued for reports that arrived in flight");
    assert_eq!(request.assignment(), Some(&child));
    request.complete();
    assert!(coordinator.claim_next_wake_request().is_none());
}

#[test]
fn dropped_wake_claim_returns_to_the_queue() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "child-turn");

    assert!(coordinator.request_wake(child.clone()));
    drop(
        coordinator
            .claim_next_wake_request()
            .expect("wake request can be claimed"),
    );
    let request = coordinator
        .claim_next_wake_request()
        .expect("dropped request is requeued");
    assert_eq!(request.assignment(), Some(&child));
    request.complete();
}

#[test]
fn active_assignment_rejects_a_different_parent_generation() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let first_parent = new_root(&coordinator);
    let other_parent = new_root(&coordinator);
    let child = new_child(&coordinator, &first_parent, "child-turn");

    assert!(
        coordinator
            .begin_or_continue_assignment(
                child.thread_id,
                Some(other_parent.clone()),
                "other-parent-turn",
                /*allow_new_generation*/ true,
            )
            .is_err()
    );
    assert_eq!(direct_children(&coordinator, &first_parent), 1);
    assert_eq!(direct_children(&coordinator, &other_parent), 0);
}

#[test]
fn same_waiting_turn_remains_waiting_after_its_last_report_is_accepted() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "waiting-turn");
    let grandchild = new_child(&coordinator, &child, "grandchild-turn");
    assert_eq!(
        coordinator.classify_turn_end(&child, "waiting-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    let report = terminal_report(&coordinator, &grandchild, "grandchild-turn");
    let report_id = report.id.as_ref().expect("stable report ID");
    assert!(coordinator.mark_report_recorded(report_id));
    let candidates = coordinator.report_ids_in_prompt(&child, &input_with_report(&report));
    assert_eq!(coordinator.accept_reports(&child, &candidates), 1);
    assert_eq!(
        coordinator.classify_turn_end(&child, "waiting-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
}

#[test]
fn stale_turn_classification_cannot_overwrite_a_later_waiting_turn() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "turn-a");
    let _grandchild = new_child(&coordinator, &child, "grandchild-turn");

    assert_eq!(
        coordinator.classify_turn_end(&child, "turn-a", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    assert!(
        coordinator
            .begin_or_continue_assignment(
                child.thread_id,
                Some(root.clone()),
                "turn-a",
                /*allow_new_generation*/ false,
            )
            .is_err()
    );
    assert_eq!(
        coordinator.begin_or_continue_assignment(
            child.thread_id,
            Some(root.clone()),
            "turn-b",
            /*allow_new_generation*/ false,
        ),
        Ok(child.clone())
    );
    assert_eq!(
        coordinator.classify_turn_end(&child, "turn-b", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    assert_eq!(
        coordinator.begin_or_continue_assignment(
            child.thread_id,
            Some(root),
            "turn-c",
            /*allow_new_generation*/ false,
        ),
        Ok(child.clone())
    );

    assert!(
        coordinator
            .classify_turn_end(&child, "turn-a", TurnEndDisposition::Succeeded)
            .is_err()
    );
    assert!(
        coordinator
            .classify_turn_end(&child, "turn-b", TurnEndDisposition::Succeeded)
            .is_err()
    );
    assert_eq!(
        coordinator.classify_turn_end(&child, "turn-c", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
}

#[test]
fn accepting_a_terminal_report_retires_its_assignment() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "child-turn");
    let report = terminal_report(&coordinator, &child, "child-turn");
    let report_id = report.id.as_ref().expect("stable report ID");
    assert!(coordinator.mark_report_recorded(report_id));
    let candidates = coordinator.report_ids_in_prompt(&parent, &input_with_report(&report));

    assert_eq!(coordinator.accept_reports(&parent, &candidates), 1);
    let state = coordinator
        .state
        .lock()
        .expect("coordinator lock is healthy");
    assert!(!state.assignments.contains_key(&child));
    assert!(!state.current_by_thread.contains_key(&child.thread_id));
}

#[test]
fn interrupted_turn_rejects_late_terminal_classification() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "interrupted-turn");
    assert_eq!(
        coordinator.classify_turn_end(&child, "interrupted-turn", TurnEndDisposition::Interrupted),
        Ok(AssignmentPhase::Interrupted)
    );
    assert!(
        coordinator
            .classify_turn_end(&child, "interrupted-turn", TurnEndDisposition::Succeeded)
            .is_err()
    );
    assert!(
        coordinator
            .classify_turn_end(&child, "stale-turn", TurnEndDisposition::Succeeded)
            .is_err()
    );
}

#[test]
fn failed_root_invalidates_waiting_reserved_and_in_flight_descendants() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "child-turn");
    let grandchild = new_child(&coordinator, &child, "grandchild-turn");
    let reservation = coordinator
        .reserve_child_assignment(root.clone(), ThreadId::new())
        .expect("reservation");
    let reserved = reservation.id().clone();
    let reload = coordinator
        .begin_generation_operation(&child)
        .expect("reload guard");
    assert_eq!(
        coordinator.classify_turn_end(&child, "child-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    assert!(coordinator.request_wake(child.clone()));
    let in_flight = coordinator
        .claim_next_wake_request()
        .expect("in-flight wake");
    let report = terminal_report(&coordinator, &grandchild, "grandchild-turn");
    let classification = coordinator
        .prepare_turn_end(&root, "root-turn", TurnEndDisposition::Errored, || {})
        .expect("failed root");
    assert_eq!(classification.phase, AssignmentPhase::Errored);
    let cancelled = classification
        .cancelled_descendants
        .iter()
        .map(|(id, _)| *id)
        .collect::<std::collections::HashSet<_>>();
    let expected = std::collections::HashSet::from([
        child.thread_id,
        grandchild.thread_id,
        reserved.thread_id,
    ]);
    assert_eq!(cancelled, expected);
    assert_eq!(outstanding(&coordinator), 0);
    assert!(!coordinator.has_pending_reports(&child));
    assert!(
        !coordinator.report_is_current_for_thread(&report.id.expect("report ID"), child.thread_id)
    );
    assert!(coordinator.begin_generation_operation(&child).is_err());
    assert!(reservation.commit().is_err());
    in_flight.defer();
    assert!(coordinator.claim_next_wake_request().is_none());
    let completion = classification
        .cancelled_descendants
        .into_iter()
        .find(|(id, _)| *id == child.thread_id)
        .expect("child cleanup")
        .1
        .expect("operation receiver");
    assert!(
        completion.has_changed().is_ok(),
        "reload still owns its guard"
    );
    drop(reload);
    assert!(
        completion.has_changed().is_err(),
        "operation closure is the cleanup barrier"
    );
}

#[test]
fn failed_root_retains_cleanup_after_reservation_rollback() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let reservation = coordinator
        .reserve_child_assignment(root.clone(), ThreadId::new())
        .expect("reservation");
    let child = reservation.id().clone();
    let operation = reservation.operation();
    drop(reservation);
    assert!(coordinator.current_assignment(child.thread_id).is_none());
    let classification = coordinator
        .prepare_turn_end(&root, "root-turn", TurnEndDisposition::Errored, || {})
        .expect("failed root");
    let (thread_id, completion) = classification
        .cancelled_descendants
        .into_iter()
        .next()
        .expect("rollback cleanup is retained");
    assert_eq!(thread_id, child.thread_id);
    assert!(operation.cancellation.is_cancelled());
    let completion = completion.expect("operation completion");
    assert!(completion.has_changed().is_ok());
    drop(operation);
    assert!(completion.has_changed().is_err());
}
