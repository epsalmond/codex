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

fn reserve_work(
    coordinator: &Arc<WorkCoordinator>,
    owner: ThreadId,
    assignment: Option<&AgentAssignmentId>,
    kind: super::WorkKind,
) -> super::OwnedWorkRegistration {
    coordinator
        .reserve_producer(
            owner,
            "producer-turn",
            "producer-call",
            assignment,
            kind,
            None,
        )
        .unwrap()
}

#[tokio::test]
async fn finite_registration_is_shared_until_last_terminal_holder_releases() {
    let (coordinator, _) = coordinator();
    let observation = coordinator.observation();
    let (before, _) = observation.subscribe();
    let owner = ThreadId::new();
    let work = reserve_work(&coordinator, owner, None, super::WorkKind::Task);
    work.mark_started();
    let accepted_result_hold = work.clone();
    let running = observation.snapshot();
    assert_eq!(running.outstanding_work, 1);
    assert_eq!(running.running_finite_work, 1);
    assert_ne!(running.revision, before.revision);
    assert!(matches!(
        observation.shutdown_if_quiescent(&before.revision),
        GuardedShutdownOutcome::StaleRevision(_)
    ));
    work.mark_terminal();
    drop(work);
    let terminal = observation.snapshot();
    assert_eq!(terminal.running_finite_work, 0);
    assert_eq!(terminal.pending_notifications, 1);
    assert_eq!(terminal.outstanding_work, 1);
    assert!(coordinator.has_live_registrations(owner));
    assert!(matches!(
        observation.shutdown_if_quiescent(&terminal.revision),
        GuardedShutdownOutcome::NotQuiescent(_)
    ));
    drop(accepted_result_hold);
    let drained = observation.snapshot();
    assert!(drained.quiescent);
    assert!(!coordinator.has_live_registrations(owner));
    assert!(matches!(
        observation.shutdown_if_quiescent(&drained.revision),
        GuardedShutdownOutcome::Closed(_)
    ));
}

#[tokio::test]
async fn registration_admission_and_guarded_close_share_one_fence_including_services() {
    for kind in [
        super::WorkKind::Task,
        super::WorkKind::Cell,
        super::WorkKind::Service,
    ] {
        let (coordinator, _) = coordinator();
        let observation = coordinator.observation();
        let (before, _) = observation.subscribe();
        let work = reserve_work(&coordinator, ThreadId::new(), None, kind);
        // Even zero-weight services invalidate an earlier close observation before launch.
        assert!(matches!(
            observation.shutdown_if_quiescent(&before.revision),
            GuardedShutdownOutcome::StaleRevision(_)
        ));
        if kind == super::WorkKind::Service {
            assert!(observation.snapshot().quiescent);
        } else {
            assert!(!observation.snapshot().quiescent);
        }
        drop(work);
        let drained = observation.snapshot();
        assert!(matches!(
            observation.shutdown_if_quiescent(&drained.revision),
            GuardedShutdownOutcome::Closed(_)
        ));
        assert!(
            coordinator
                .reserve_producer(ThreadId::new(), "producer-turn", "late", None, kind, None)
                .is_err()
        );
    }
}

#[tokio::test]
async fn finite_admission_racing_close_either_holds_scope_or_rejects_before_launch() {
    let (coordinator, _) = coordinator();
    let observation = coordinator.observation();
    let (before, _) = observation.subscribe();
    let barrier = Arc::new(Barrier::new(2));
    let launching = Arc::clone(&coordinator);
    let starting = Arc::clone(&barrier);
    let start = tokio::spawn(async move {
        starting.wait().await;
        launching.reserve_producer(
            ThreadId::new(),
            "producer-turn",
            "race",
            None,
            super::WorkKind::Task,
            None,
        )
    });
    barrier.wait().await;
    let closed = observation.shutdown_if_quiescent(&before.revision);
    let admitted = start.await.unwrap();
    match closed {
        GuardedShutdownOutcome::Closed(_) => assert!(admitted.is_err()),
        GuardedShutdownOutcome::StaleRevision(_) => {
            assert!(admitted.is_ok());
            assert!(!observation.snapshot().quiescent);
        }
        other => panic!("unexpected close outcome: {other:?}"),
    }
}

#[tokio::test]
async fn registration_limits_are_per_owner_and_failed_start_releases_capacity() {
    let (coordinator, _) = coordinator();
    let owner = ThreadId::new();
    let other_owner = ThreadId::new();
    let mut services = (0..16)
        .map(|_| reserve_work(&coordinator, owner, None, super::WorkKind::Service))
        .collect::<Vec<_>>();
    assert!(
        coordinator
            .reserve_producer(
                owner,
                "producer-turn",
                "service-overflow",
                None,
                super::WorkKind::Service,
                None
            )
            .is_err()
    );
    let tasks = (16..64)
        .map(|_| reserve_work(&coordinator, owner, None, super::WorkKind::Task))
        .collect::<Vec<_>>();
    assert!(
        coordinator
            .reserve_producer(
                owner,
                "producer-turn",
                "total-overflow",
                None,
                super::WorkKind::Cell,
                None
            )
            .is_err()
    );
    let unrelated = reserve_work(&coordinator, other_owner, None, super::WorkKind::Service);
    drop(services.pop());
    let replacement = reserve_work(&coordinator, owner, None, super::WorkKind::Service);
    drop((services, tasks, replacement));
    assert!(!coordinator.has_live_registrations(owner));
    assert!(coordinator.has_live_registrations(other_owner));
    drop(unrelated);
    assert_eq!(coordinator.observation().snapshot().outstanding_work, 0);
}

fn producer_assignment(wake: &AgentWakeCoordinator) -> AgentAssignmentId {
    wake.begin_or_continue_assignment(ThreadId::new(), None, "producer-turn", true)
        .unwrap()
}

#[tokio::test]
async fn nonautomatic_terminal_hint_binds_once_and_unstarted_bind_restores_it() {
    let (coordinator, wake) = coordinator();
    let assignment = producer_assignment(&wake);
    let work = reserve_work(
        &coordinator,
        assignment.thread_id,
        Some(&assignment),
        super::WorkKind::Task,
    );
    work.mark_started();
    work.mark_yielded(false);
    assert_eq!(
        wake.classify_turn_end(&assignment, "producer-turn", TurnEndDisposition::Succeeded),
        Ok(super::super::coordinator::AssignmentPhase::Waiting)
    );
    assert!(!wake.has_ready_owned_completions(&assignment));
    work.mark_terminal();
    drop(work);
    assert!(wake.has_ready_owned_completions(&assignment));
    wake.continue_owned_completion(&assignment, "unstarted")
        .unwrap();
    assert!(!wake.has_ready_owned_completions(&assignment));
    assert_eq!(
        wake.release_unstarted_turn(assignment.thread_id, "unstarted"),
        None
    );
    assert!(wake.has_ready_owned_completions(&assignment));
    wake.continue_owned_completion(&assignment, "started")
        .unwrap();
    wake.acknowledge_finite_terminal_hint(&assignment, "started");
    assert!(!wake.has_ready_owned_completions(&assignment));
    assert_eq!(
        wake.classify_turn_end(&assignment, "started", TurnEndDisposition::Succeeded),
        Ok(super::super::coordinator::AssignmentPhase::Completed)
    );
    assert!(
        wake.continue_owned_completion(&assignment, "extra")
            .is_err()
    );
}

#[tokio::test]
async fn final_boundary_retains_new_hint_but_prepared_active_request_coalesces_it() {
    for observed in [false, true] {
        let (coordinator, wake) = coordinator();
        let assignment = producer_assignment(&wake);
        let work = reserve_work(
            &coordinator,
            assignment.thread_id,
            Some(&assignment),
            super::WorkKind::Task,
        );
        work.mark_started();
        work.mark_yielded(false);
        work.mark_terminal();
        drop(work);
        if observed {
            wake.acknowledge_finite_terminal_hint(&assignment, "producer-turn");
        }
        let phase = wake
            .classify_turn_end(&assignment, "producer-turn", TurnEndDisposition::Succeeded)
            .unwrap();
        assert_eq!(
            phase,
            if observed {
                super::super::coordinator::AssignmentPhase::Completed
            } else {
                super::super::coordinator::AssignmentPhase::Waiting
            }
        );
        assert_eq!(wake.has_ready_owned_completions(&assignment), !observed);
    }
}

#[tokio::test]
async fn newer_terminal_revision_survives_bind_and_generation_replacement_fences_old_hint() {
    let (coordinator, wake) = coordinator();
    let assignment = producer_assignment(&wake);
    let first = reserve_work(
        &coordinator,
        assignment.thread_id,
        Some(&assignment),
        super::WorkKind::Task,
    );
    let second = reserve_work(
        &coordinator,
        assignment.thread_id,
        Some(&assignment),
        super::WorkKind::Task,
    );
    for work in [&first, &second] {
        work.mark_started();
        work.mark_yielded(false);
    }
    wake.classify_turn_end(&assignment, "producer-turn", TurnEndDisposition::Succeeded)
        .unwrap();
    first.mark_terminal();
    drop(first);
    wake.continue_owned_completion(&assignment, "bound")
        .unwrap();
    second.mark_terminal();
    drop(second);
    assert!(wake.has_ready_owned_completions(&assignment));
    // A stale abort cannot consume either this turn's or the newer revision's authority.
    wake.release_unstarted_turn(assignment.thread_id, "another-turn");
    assert!(wake.has_ready_owned_completions(&assignment));
    wake.release_unstarted_turn(assignment.thread_id, "bound");
    assert!(wake.has_ready_owned_completions(&assignment));
    wake.cancel_assignment(&assignment);
    let replacement = wake
        .begin_or_continue_assignment(assignment.thread_id, None, "replacement", true)
        .unwrap();
    assert!(!wake.has_ready_owned_completions(&assignment));
    assert!(!wake.has_ready_owned_completions(&replacement));
    assert!(wake.continue_owned_completion(&assignment, "late").is_err());
}

#[tokio::test]
async fn failed_inline_automatic_and_service_releases_do_not_create_terminal_hints() {
    for (kind, started, yielded, automatic) in [
        (super::WorkKind::Task, false, true, false),
        (super::WorkKind::Task, true, false, false),
        (super::WorkKind::Task, true, true, true),
        (super::WorkKind::Service, true, true, false),
    ] {
        let (coordinator, wake) = coordinator();
        let assignment = producer_assignment(&wake);
        let keep_waiting = reserve_work(
            &coordinator,
            assignment.thread_id,
            Some(&assignment),
            super::WorkKind::Cell,
        );
        let work = reserve_work(&coordinator, assignment.thread_id, Some(&assignment), kind);
        if started {
            work.mark_started();
        }
        if yielded {
            work.mark_yielded(automatic);
        }
        wake.classify_turn_end(&assignment, "producer-turn", TurnEndDisposition::Succeeded)
            .unwrap();
        work.mark_terminal();
        drop(work);
        assert!(!wake.has_ready_owned_completions(&assignment));
        assert!(
            wake.continue_owned_completion(&assignment, "unwanted")
                .is_err()
        );
        drop(keep_waiting);
        assert!(!wake.has_ready_owned_completions(&assignment));
    }
}

#[tokio::test]
async fn retained_cell_authorizes_original_generation_after_yield_and_rejects_replacement() {
    let (coordinator, wake) = coordinator();
    let assignment = producer_assignment(&wake);
    let cell = reserve_work(
        &coordinator,
        assignment.thread_id,
        Some(&assignment),
        super::WorkKind::Cell,
    );
    cell.mark_started();
    cell.mark_yielded(false);
    wake.classify_turn_end(&assignment, "producer-turn", TurnEndDisposition::Succeeded)
        .unwrap();
    assert!(
        coordinator
            .reserve_producer(
                assignment.thread_id,
                "producer-turn",
                "no-authority",
                Some(&assignment),
                super::WorkKind::Task,
                None
            )
            .is_err()
    );
    let nested = coordinator
        .reserve_producer(
            assignment.thread_id,
            "producer-turn",
            "nested",
            Some(&assignment),
            super::WorkKind::Task,
            Some(&cell),
        )
        .unwrap();
    assert!(nested.matches(
        assignment.thread_id,
        "producer-turn",
        "nested",
        Some(&assignment)
    ));
    assert!(
        coordinator
            .reserve_producer(
                assignment.thread_id,
                "different-turn",
                "wrong-origin",
                Some(&assignment),
                super::WorkKind::Task,
                Some(&cell)
            )
            .is_err()
    );
    wake.cancel_assignment(&assignment);
    let replacement = wake
        .begin_or_continue_assignment(assignment.thread_id, None, "replacement", true)
        .unwrap();
    assert!(
        coordinator
            .reserve_producer(
                assignment.thread_id,
                "producer-turn",
                "stale",
                Some(&assignment),
                super::WorkKind::Task,
                Some(&cell)
            )
            .is_err()
    );
    assert!(
        coordinator
            .reserve_producer(
                assignment.thread_id,
                "replacement",
                "donated",
                Some(&replacement),
                super::WorkKind::Task,
                Some(&cell)
            )
            .is_err()
    );
    drop((cell, nested));
    assert!(!wake.has_ready_owned_completions(&replacement));
}

#[tokio::test]
async fn service_registration_remains_live_without_retaining_successful_assignment() {
    let (coordinator, wake) = coordinator();
    let assignment = producer_assignment(&wake);
    let service = reserve_work(
        &coordinator,
        assignment.thread_id,
        Some(&assignment),
        super::WorkKind::Service,
    );
    service.mark_started();
    service.mark_yielded(false);
    assert_eq!(
        wake.classify_turn_end(&assignment, "producer-turn", TurnEndDisposition::Succeeded),
        Ok(super::super::coordinator::AssignmentPhase::Completed)
    );
    assert!(coordinator.has_live_registrations(assignment.thread_id));
    assert_eq!(coordinator.observation().snapshot().outstanding_work, 0);
    service.mark_terminal();
    drop(service);
    assert!(!coordinator.has_live_registrations(assignment.thread_id));
    assert!(!wake.has_ready_owned_completions(&assignment));
}
