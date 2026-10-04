use super::GuardedShutdownOutcome;
use super::WorkAdmissionError;
use super::WorkCoordinator;
use crate::agent::control::coordinator::AgentAssignmentId;
use crate::agent::control::coordinator::AgentWakeCoordinator;
use crate::agent::control::coordinator::TurnEndDisposition;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

fn coordinator() -> (Arc<WorkCoordinator>, Arc<AgentWakeCoordinator>) {
    let wake = Arc::new(AgentWakeCoordinator::default());
    (Arc::new(WorkCoordinator::new(Arc::clone(&wake))), wake)
}

fn start_root(wake: &AgentWakeCoordinator, turn_id: &str) -> AgentAssignmentId {
    wake.begin_or_continue_assignment(
        ThreadId::new(),
        None,
        turn_id,
        /*allow_new_generation*/ true,
    )
    .expect("root assignment starts")
}

fn start_child(wake: &Arc<AgentWakeCoordinator>, parent: &AgentAssignmentId) -> AgentAssignmentId {
    let child = wake
        .reserve_child_assignment(parent.clone(), ThreadId::new())
        .expect("child assignment is admitted")
        .commit()
        .expect("parent still owns the reservation");
    wake.begin_or_continue_assignment(
        child.thread_id,
        Some(parent.clone()),
        "child-turn",
        /*allow_new_generation*/ false,
    )
    .expect("committed child starts");
    child
}

#[tokio::test]
async fn subscription_cannot_miss_a_concurrent_root_turn() {
    let (coordinator, _wake) = coordinator();
    let barrier = Arc::new(Barrier::new(3));

    let subscribe_coordinator = Arc::clone(&coordinator);
    let subscribe_barrier = Arc::clone(&barrier);
    let subscribe = tokio::spawn(async move {
        subscribe_barrier.wait().await;
        subscribe_coordinator.observation().subscribe()
    });

    let start_coordinator = Arc::clone(&coordinator);
    let start_barrier = Arc::clone(&barrier);
    let start = tokio::spawn(async move {
        start_barrier.wait().await;
        start_coordinator
            .root_turn_started("turn-1")
            .expect("root turn should start");
    });

    barrier.wait().await;
    let (initial, mut updates) = subscribe.await.expect("subscription task should finish");
    start.await.expect("root turn task should finish");
    let observed = if updates.has_changed().expect("sender remains alive") {
        updates.borrow_and_update().clone()
    } else {
        initial
    };
    assert_eq!(observed, coordinator.observation().snapshot());
    assert_eq!(observed.active_root_turns, 1);
}

#[tokio::test]
async fn duplicate_terminal_event_does_not_publish_an_extra_snapshot() {
    let (coordinator, _wake) = coordinator();
    coordinator
        .root_turn_started("turn-1")
        .expect("root turn should start");
    let (initial, mut updates) = coordinator.observation().subscribe();
    assert_eq!(initial.active_root_turns, 1);

    coordinator.root_turn_terminal("turn-1");
    assert!(updates.has_changed().expect("sender remains alive"));
    let terminal = updates.borrow_and_update().clone();
    assert_eq!(terminal.pending_terminal_outputs, 1);

    coordinator.root_turn_terminal("turn-1");
    assert!(!updates.has_changed().expect("sender remains alive"));
}

#[tokio::test]
async fn close_waits_for_root_terminal_output_to_be_forwarded() {
    let (coordinator, _wake) = coordinator();
    let observation = coordinator.observation();
    observation.subscribe();
    coordinator
        .root_turn_started("turn-1")
        .expect("root turn should start");
    let active = observation.snapshot();
    assert_eq!(active.active_root_turns, 1);
    assert!(!active.quiescent);
    assert!(matches!(
        observation.shutdown_if_quiescent(&active.revision),
        GuardedShutdownOutcome::NotQuiescent(_)
    ));

    coordinator.root_turn_terminal("turn-1");
    let terminal = observation.snapshot();
    assert_eq!(terminal.active_root_turns, 0);
    assert_eq!(terminal.pending_terminal_outputs, 1);
    assert!(!terminal.quiescent);
    assert!(matches!(
        observation.shutdown_if_quiescent(&terminal.revision),
        GuardedShutdownOutcome::NotQuiescent(_)
    ));

    coordinator.root_turn_output_forwarded("turn-1");
    let drained = observation.snapshot();
    assert!(drained.quiescent);
    let GuardedShutdownOutcome::Closed(closed) =
        observation.shutdown_if_quiescent(&drained.revision)
    else {
        panic!("the current drained revision should close");
    };
    assert!(closed.closed);
    assert!(matches!(
        coordinator.root_turn_started("turn-2"),
        Err(WorkAdmissionError::Closed)
    ));
}

#[tokio::test]
async fn stale_revision_cannot_close_after_a_root_turn_starts() {
    let (coordinator, _wake) = coordinator();
    let observation = coordinator.observation();
    observation.subscribe();
    let stale = observation.snapshot();
    coordinator
        .root_turn_started("turn-1")
        .expect("root turn should start");
    assert!(matches!(
        observation.shutdown_if_quiescent(&stale.revision),
        GuardedShutdownOutcome::StaleRevision(_)
    ));
    assert!(!observation.snapshot().closed);
}

#[tokio::test]
async fn a_root_turn_that_comes_and_goes_still_stales_the_revision() {
    let (coordinator, wake) = coordinator();
    let observation = coordinator.observation();
    observation.subscribe();
    let before = observation.snapshot();
    let (_, epoch_before) = wake.work_summary();

    coordinator
        .root_turn_started("turn-1")
        .expect("root turn should start");
    coordinator.root_turn_abandoned("turn-1");
    let after = observation.snapshot();
    assert_eq!(wake.work_summary().1, epoch_before);
    assert_eq!(
        (after.active_root_turns, after.quiescent),
        (before.active_root_turns, before.quiescent)
    );
    assert_ne!(after.revision, before.revision);
    assert!(matches!(
        observation.shutdown_if_quiescent(&before.revision),
        GuardedShutdownOutcome::StaleRevision(_)
    ));
}

#[tokio::test]
async fn guarded_close_serializes_with_root_turn_admission() {
    let (coordinator, _wake) = coordinator();
    let observation = coordinator.observation();
    observation.subscribe();
    let revision = observation.snapshot().revision;
    let barrier = Arc::new(Barrier::new(3));

    let start_coordinator = Arc::clone(&coordinator);
    let start_barrier = Arc::clone(&barrier);
    let start = tokio::spawn(async move {
        start_barrier.wait().await;
        start_coordinator.root_turn_started("turn-1")
    });

    let close_observation = observation.clone();
    let close_barrier = Arc::clone(&barrier);
    let close = tokio::spawn(async move {
        close_barrier.wait().await;
        close_observation.shutdown_if_quiescent(&revision)
    });

    barrier.wait().await;
    let started = start.await.expect("root turn task should finish");
    let closed = close.await.expect("close should finish");
    match (started, closed) {
        (Ok(()), GuardedShutdownOutcome::StaleRevision(snapshot)) => {
            assert_eq!(snapshot.active_root_turns, 1);
            assert!(!snapshot.closed);
        }
        (Err(WorkAdmissionError::Closed), GuardedShutdownOutcome::Closed(snapshot)) => {
            assert!(snapshot.closed);
            assert_eq!(snapshot.active_root_turns, 0);
        }
        outcome => panic!("admission and close must share one atomic order: {outcome:?}"),
    }
}

#[tokio::test]
async fn unobserved_legacy_turns_release_terminal_markers_without_a_cap() {
    let (coordinator, _wake) = coordinator();
    for index in 0..(super::MAX_TRACKED_ROOT_TURNS * 2) {
        let turn_id = format!("legacy-{index}");
        coordinator
            .root_turn_started(&turn_id)
            .expect("legacy root turn should start");
        coordinator.root_turn_terminal(&turn_id);
    }
    let snapshot = coordinator.observation_snapshot();
    assert_eq!(snapshot.active_root_turns, 0);
    assert_eq!(snapshot.pending_terminal_outputs, 0);
    assert!(!snapshot.output_forwarding_observed);
    assert!(!snapshot.quiescent);
}

#[tokio::test]
async fn close_requires_terminal_output_observation_to_be_active() {
    let (coordinator, _wake) = coordinator();
    let observation = coordinator.observation();
    let snapshot = observation.snapshot();
    assert!(matches!(
        observation.shutdown_if_quiescent(&snapshot.revision),
        GuardedShutdownOutcome::ObservationInactive(_)
    ));
    assert!(!observation.snapshot().closed);
}

#[tokio::test]
async fn open_assignments_and_unrecorded_reports_keep_the_tree_open() {
    let (coordinator, wake) = coordinator();
    let observation = coordinator.observation();
    observation.subscribe();

    let root = start_root(&wake, "root-turn");
    let child = start_child(&wake, &root);
    assert_eq!(
        wake.classify_turn_end(&root, "root-turn", TurnEndDisposition::Succeeded),
        Ok(crate::agent::control::coordinator::AssignmentPhase::Waiting)
    );
    let waiting = observation.snapshot();
    assert_eq!(waiting.running_finite_work, 2);
    assert!(!waiting.quiescent);
    assert!(matches!(
        observation.shutdown_if_quiescent(&waiting.revision),
        GuardedShutdownOutcome::NotQuiescent(_)
    ));

    wake.classify_turn_end(&child, "child-turn", TurnEndDisposition::Succeeded)
        .expect("child turn is classified");
    wake.publish_terminal_report(
        &child,
        "child-turn",
        InterAgentCommunication::new(
            AgentPath::try_from("/root/worker").expect("valid child path"),
            AgentPath::root(),
            Vec::new(),
            "done".to_string(),
            /*trigger_turn*/ true,
        ),
    )
    .expect("waiting parent receives the report");
    let reported = observation.snapshot();
    assert_eq!(reported.running_finite_work, 1);
    assert!(reported.pending_notifications >= 1);
    assert!(!reported.quiescent);
}

#[tokio::test]
async fn wake_epoch_change_makes_an_observed_revision_stale() {
    let (coordinator, wake) = coordinator();
    let observation = coordinator.observation();
    observation.subscribe();
    let observed = observation.snapshot();
    assert!(observed.quiescent);

    wake.notify_capacity_available();
    let GuardedShutdownOutcome::StaleRevision(current) =
        observation.shutdown_if_quiescent(&observed.revision)
    else {
        panic!("a newer wake epoch must invalidate the observed revision");
    };
    assert!(current.quiescent);
    assert!(matches!(
        observation.shutdown_if_quiescent(&current.revision),
        GuardedShutdownOutcome::Closed(_)
    ));
}

#[tokio::test]
async fn forwarder_publishes_agent_tree_changes() {
    let (coordinator, wake) = coordinator();
    let (initial, mut updates) = coordinator.observation().subscribe();
    assert!(initial.quiescent);

    let root = start_root(&wake, "root-turn");
    let running = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            updates.changed().await.expect("sender remains alive");
            let snapshot = updates.borrow_and_update().clone();
            if snapshot.running_finite_work == 1 {
                return snapshot;
            }
        }
    })
    .await
    .expect("forwarder should publish the new assignment");
    assert!(!running.quiescent);

    wake.classify_turn_end(&root, "root-turn", TurnEndDisposition::Succeeded)
        .expect("root turn is classified");
    let drained = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            updates.changed().await.expect("sender remains alive");
            let snapshot = updates.borrow_and_update().clone();
            if snapshot.quiescent {
                return snapshot;
            }
        }
    })
    .await
    .expect("forwarder should publish the released assignment");
    assert_eq!(drained, coordinator.observation().snapshot());
}
