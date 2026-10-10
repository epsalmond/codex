use super::*;
use crate::agent::control::AssignmentPhase;
use crate::agent::control::TurnEndDisposition;
use crate::session::async_completion::CompletionStatus;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn owned_admission_retains_exact_generation_until_retirement() {
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
        .begin_or_continue_assignment(
            assignment.thread_id,
            None,
            "manual",
            /*allow_new_generation*/ false,
        )
        .unwrap();
    assert_eq!(
        coordinator.current_assignment(session.thread_id()),
        Some(assignment.clone())
    );
    assert_eq!(
        coordinator.classify_turn_end(&assignment, "manual", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    store.retire();
    assert!(!store.has_owned_work());
}
