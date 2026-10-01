use super::*;

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
    let child = reservation.id.clone();

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
