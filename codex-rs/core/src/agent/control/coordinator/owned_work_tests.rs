use super::*;
use crate::agent::control::AssignmentPhase;
use crate::agent::control::TurnEndDisposition;
use crate::session::async_completion::CompletionStatus;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn owned_admission_retains_exact_generation_until_acceptance() {
    let (session, turn) = make_session_and_context().await;
    let coordinator = AgentWakeCoordinator::default();
    let assignment = coordinator
        .begin_or_continue_assignment(
            session.thread_id(),
            None,
            &turn.sub_id,
            /*allow_new_generation*/ true,
        )
        .unwrap();
    turn.agent_assignment.set(assignment.clone()).unwrap();
    let store = Arc::clone(&session.services.async_completions);
    let mut reservation = coordinator
        .admit_owned_completion(&assignment, &turn.sub_id, &store, || {
            store.reserve(
                session.thread_id(),
                &turn,
                "finite",
                /*process_id*/ 1,
                /*cell_id*/ None,
            )
        })
        .unwrap();
    assert_eq!(
        coordinator.classify_turn_end(&assignment, &turn.sub_id, TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    assert!(
        coordinator
            .admit_owned_completion(&assignment, &turn.sub_id, &store, || Ok(()))
            .is_err()
    );
    reservation.register().publish(
        CompletionStatus::Exited(0),
        b"done",
        /*omitted_bytes*/ 0,
    );
    drop(reservation);
    coordinator
        .continue_owned_completion(&assignment, "automatic")
        .unwrap();
    assert_eq!(
        coordinator.current_assignment(session.thread_id()),
        Some(assignment.clone())
    );
    assert_eq!(
        coordinator.classify_turn_end(&assignment, "automatic", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    store.retire();
    assert!(
        coordinator
            .continue_owned_completion(&assignment, "retired")
            .is_err()
    );
    assert!(!store.has_owned_work());
}

#[tokio::test]
async fn service_completion_record_does_not_retain_assignment_or_resurrect_generation() {
    use crate::agent::control::WorkKind;
    use crate::agent::control::work_lifecycle::WorkCoordinator;

    let (session, turn) = make_session_and_context().await;
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let lifecycle = Arc::new(WorkCoordinator::new(Arc::clone(&coordinator)));
    let assignment = coordinator
        .begin_or_continue_assignment(session.thread_id(), None, &turn.sub_id, true)
        .unwrap();
    turn.agent_assignment.set(assignment.clone()).unwrap();
    let service = lifecycle
        .reserve_producer(
            session.thread_id(),
            &turn.sub_id,
            "service",
            Some(&assignment),
            WorkKind::Service,
            None,
        )
        .unwrap();
    service.mark_started();
    service.mark_yielded(true);
    let store = Arc::new(AsyncCompletions::default());
    let mut reservation = store
        .reserve_with_lifecycle(session.thread_id(), &turn, "service", 1, None, &service)
        .unwrap();
    let publisher = reservation.register();
    drop(reservation);
    assert!(!store.owns_assignment(&assignment, /*excluded*/ None));
    assert_eq!(
        coordinator.classify_turn_end(&assignment, &turn.sub_id, TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Completed)
    );
    publisher.publish(CompletionStatus::Exited(3), b"service ended", 0);
    service.mark_terminal();
    drop(service);
    assert!(store.has_ready(session.thread_id(), Some(&assignment)));
    assert!(!coordinator.has_ready_owned_completions(&assignment));
    assert!(
        coordinator
            .continue_owned_completion(&assignment, "resurrected")
            .is_err()
    );
    let replacement = coordinator
        .begin_or_continue_assignment(session.thread_id(), None, "explicit", true)
        .unwrap();
    assert!(!coordinator.has_ready_owned_completions(&replacement));
    assert!(lifecycle.has_live_registrations(session.thread_id()));
    store.retire();
    assert!(!lifecycle.has_live_registrations(session.thread_id()));
}

#[tokio::test]
async fn registered_nested_completion_is_admitted_under_original_waiting_generation() {
    use crate::agent::control::WorkKind;
    use crate::agent::control::work_lifecycle::WorkCoordinator;

    let (session, turn) = make_session_and_context().await;
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let lifecycle = Arc::new(WorkCoordinator::new(Arc::clone(&coordinator)));
    let assignment = coordinator
        .begin_or_continue_assignment(session.thread_id(), None, &turn.sub_id, true)
        .unwrap();
    turn.agent_assignment.set(assignment.clone()).unwrap();
    let cell = lifecycle
        .reserve_producer(
            session.thread_id(),
            &turn.sub_id,
            "cell",
            Some(&assignment),
            WorkKind::Cell,
            None,
        )
        .unwrap();
    cell.mark_started();
    coordinator
        .classify_turn_end(&assignment, &turn.sub_id, TurnEndDisposition::Succeeded)
        .unwrap();
    let nested = lifecycle
        .reserve_producer(
            session.thread_id(),
            &turn.sub_id,
            "nested",
            Some(&assignment),
            WorkKind::Task,
            Some(&cell),
        )
        .unwrap();
    let store = Arc::new(AsyncCompletions::default());
    let mut reservation = store
        .reserve_with_lifecycle(
            session.thread_id(),
            &turn,
            "nested",
            2,
            Some("cell-id"),
            &nested,
        )
        .unwrap();
    nested.mark_started();
    nested.mark_yielded(true);
    reservation
        .register()
        .publish(CompletionStatus::Exited(0), b"nested result", 0);
    drop(reservation);
    nested.mark_terminal();
    drop(nested);
    assert!(store.owns_assignment(&assignment, /*excluded*/ None));
    assert!(coordinator.has_ready_owned_completions(&assignment));
    coordinator
        .continue_owned_completion(&assignment, "collector")
        .unwrap();
    store.retire();
    // Automatic record removal cannot manufacture a second hint.
    assert!(!coordinator.has_ready_owned_completions(&assignment));
    drop(cell);
    assert_eq!(
        coordinator.classify_turn_end(&assignment, "collector", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Completed)
    );
}

#[tokio::test]
async fn pending_turn_end_leaves_out_only_the_asking_cell() {
    use crate::agent::control::WorkKind;
    use crate::agent::control::work_lifecycle::WorkCoordinator;

    let (session, turn) = make_session_and_context().await;
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let lifecycle = Arc::new(WorkCoordinator::new(Arc::clone(&coordinator)));
    let assignment = coordinator
        .begin_or_continue_assignment(session.thread_id(), None, &turn.sub_id, true)
        .unwrap();
    turn.agent_assignment.set(assignment.clone()).unwrap();
    assert_eq!(
        coordinator.pending_turn_end_children(&assignment, /*asking*/ None),
        None
    );
    let cell = lifecycle
        .reserve_producer(
            session.thread_id(),
            &turn.sub_id,
            "cell",
            Some(&assignment),
            WorkKind::Cell,
            None,
        )
        .unwrap();
    cell.mark_started();
    let store = Arc::new(AsyncCompletions::default());
    let _cell_record = store
        .reserve_cell_with_lifecycle(session.thread_id(), &turn, "cell", &cell)
        .unwrap();
    // Neither the asking cell's producer count nor its own record is pending work.
    assert_eq!(
        coordinator.pending_turn_end_children(&assignment, Some(&cell)),
        None
    );
    assert_eq!(
        coordinator.pending_turn_end_children(&assignment, /*asking*/ None),
        Some(Vec::new())
    );
    let nested = lifecycle
        .reserve_producer(
            session.thread_id(),
            &turn.sub_id,
            "nested",
            Some(&assignment),
            WorkKind::Task,
            Some(&cell),
        )
        .unwrap();
    assert_eq!(
        coordinator.pending_turn_end_children(&assignment, Some(&cell)),
        Some(Vec::new())
    );
    drop(nested);
    assert_eq!(
        coordinator.pending_turn_end_children(&assignment, Some(&cell)),
        None
    );
    // Turn-end classification shares the predicate and counts the cell itself.
    assert_eq!(
        coordinator.classify_turn_end(&assignment, &turn.sub_id, TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
}
