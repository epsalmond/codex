use super::AgentAssignmentId;
use super::AgentWakeCoordinator;
use super::AssignmentPhase;
use super::MAX_OUTSTANDING_ASSIGNMENTS;
use super::TurnEndDisposition;
use super::WakeDispatchResult;
use super::reports::TerminalReportPublication;
use crate::agent::control::LocalAgentControl;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;

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

#[tokio::test]
async fn dispatcher_defers_fairly_and_waits_for_a_resource_event() {
    let control = Arc::new(LocalAgentControl::default());
    control.runtime.agent_execution_limiter.initialize(2);
    let coordinator = Arc::clone(&control.runtime.wake_coordinator);
    let root = new_root(&coordinator);
    let first = new_child(&coordinator, &root, "first-turn");
    let second = new_child(&coordinator, &root, "second-turn");
    assert!(coordinator.request_wake(first.clone()));
    assert!(coordinator.request_wake(second.clone()));
    let subagent_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let first_slot = control
        .execution_guard(MultiAgentVersion::V2, &subagent_source)
        .await
        .expect("first concurrent child occupies a slot");
    let second_slot = control
        .execution_guard(MultiAgentVersion::V2, &subagent_source)
        .await
        .expect("second concurrent child occupies a slot");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let (attempt_tx, mut attempt_rx) = tokio::sync::mpsc::unbounded_channel();
    let dispatch_control = Arc::clone(&control);
    let dispatch_source = subagent_source.clone();
    let worker = tokio::spawn(Arc::clone(&coordinator).run_wake_dispatcher(
        shutdown_rx,
        move |assignment| {
            let attempt_tx = attempt_tx.clone();
            let control = Arc::clone(&dispatch_control);
            let source = dispatch_source.clone();
            async move {
                attempt_tx
                    .send(assignment.clone())
                    .expect("test receiver remains open");
                if control
                    .ensure_execution_capacity(MultiAgentVersion::V2, &source)
                    .is_err()
                {
                    WakeDispatchResult::Defer
                } else {
                    WakeDispatchResult::Complete
                }
            }
        },
    ));

    assert_eq!(attempt_rx.recv().await, Some(first.clone()));
    assert_eq!(attempt_rx.recv().await, Some(second));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), attempt_rx.recv())
            .await
            .is_err()
    );

    drop(first_slot);
    assert_eq!(attempt_rx.recv().await, Some(first));
    drop(second_slot);
    drop(shutdown_tx);
    worker
        .await
        .expect("dispatcher exits after runtime shutdown");
}

#[tokio::test]
async fn dispatcher_preserves_events_arriving_during_a_pass() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let target = new_child(&coordinator, &root, "target-turn");
    assert!(coordinator.request_wake(target.clone()));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let (attempt_tx, mut attempt_rx) = tokio::sync::mpsc::unbounded_channel();
    let attempts = Arc::new(AtomicUsize::default());
    let expected_attempts = Arc::clone(&attempts);
    let dispatch_coordinator = Arc::clone(&coordinator);
    let worker = tokio::spawn(
        coordinator.run_wake_dispatcher(shutdown_rx, move |assignment| {
            let attempt_tx = attempt_tx.clone();
            let attempts = Arc::clone(&attempts);
            let coordinator = Arc::clone(&dispatch_coordinator);
            async move {
                attempt_tx
                    .send(assignment)
                    .expect("test receiver remains open");
                if attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                    coordinator.notify_capacity_available();
                    WakeDispatchResult::Defer
                } else {
                    WakeDispatchResult::Complete
                }
            }
        }),
    );

    assert_eq!(attempt_rx.recv().await, Some(target.clone()));
    assert_eq!(attempt_rx.recv().await, Some(target));
    assert_eq!(expected_attempts.load(Ordering::Acquire), 2);
    drop(shutdown_tx);
    worker
        .await
        .expect("dispatcher exits after runtime shutdown");
}

#[tokio::test]
async fn report_arrival_during_deferred_dispatch_retries_without_resource_event() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let target = new_child(&coordinator, &root, "target-turn");
    assert!(coordinator.request_wake(target.clone()));

    let dispatch_started = Arc::new(Notify::new());
    let release_first_attempt = Arc::new(Notify::new());
    let attempts = Arc::new(AtomicUsize::default());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let (attempt_tx, mut attempt_rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::spawn(Arc::clone(&coordinator).run_wake_dispatcher(shutdown_rx, {
        let dispatch_started = Arc::clone(&dispatch_started);
        let release_first_attempt = Arc::clone(&release_first_attempt);
        let attempts = Arc::clone(&attempts);
        move |assignment| {
            let attempt_tx = attempt_tx.clone();
            let dispatch_started = Arc::clone(&dispatch_started);
            let release_first_attempt = Arc::clone(&release_first_attempt);
            let attempts = Arc::clone(&attempts);
            async move {
                attempt_tx
                    .send(assignment)
                    .expect("test receiver remains open");
                if attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                    dispatch_started.notify_one();
                    release_first_attempt.notified().await;
                    WakeDispatchResult::Defer
                } else {
                    WakeDispatchResult::Complete
                }
            }
        }
    }));

    assert_eq!(attempt_rx.recv().await, Some(target.clone()));
    dispatch_started.notified().await;
    assert!(!coordinator.request_wake(target.clone()));
    release_first_attempt.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), attempt_rx.recv())
            .await
            .expect("report arrival wakes the dispatcher after a defer"),
        Some(target)
    );
    drop(shutdown_tx);
    worker
        .await
        .expect("dispatcher exits after runtime shutdown");
}

#[test]
fn sibling_terminal_reports_are_each_included_once_in_the_accepted_prompt() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let first = new_child(&coordinator, &parent, "first-turn");
    let second = new_child(&coordinator, &parent, "second-turn");
    let first_report = terminal_report(&coordinator, &first, "first-turn");
    let second_report = terminal_report(&coordinator, &second, "second-turn");
    let first_id = first_report.id.clone().expect("first report has an ID");
    let second_id = second_report.id.clone().expect("second report has an ID");
    assert!(coordinator.mark_report_recorded(&first_id));
    assert!(coordinator.mark_report_recorded(&second_id));

    let prompt_input = [
        input_with_report(&first_report),
        input_with_report(&second_report),
    ]
    .concat();
    let candidates = coordinator.report_ids_in_prompt(&parent, &prompt_input);

    assert_eq!(candidates, vec![first_id, second_id]);
    assert_eq!(coordinator.accept_reports(&parent, &candidates), 2);
    assert!(!coordinator.has_pending_reports(&parent));
    assert_eq!(direct_children(&coordinator, &parent), 0);
    assert_eq!(outstanding(&coordinator), 0);
}

#[test]
fn recorded_report_after_sampling_snapshot_wakes_without_mailbox_reinsertion() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "child-turn");
    let report = terminal_report(&coordinator, &child, "child-turn");
    let report_id = report.id.expect("report has a stable ID");
    let initial_wake = coordinator
        .claim_next_wake_request()
        .expect("child report requests a wake");
    assert_eq!(initial_wake.assignment(), Some(&parent));
    initial_wake.complete();
    assert!(coordinator.mark_report_recorded(&report_id));

    assert_eq!(
        coordinator.classify_turn_end(&parent, "root-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    assert!(coordinator.has_recorded_reports(&parent));
    assert!(coordinator.claim_next_mailbox_report(&parent).is_none());
    assert_eq!(
        coordinator
            .claim_next_wake_request()
            .and_then(|request| request.assignment().cloned()),
        Some(parent)
    );
}

#[test]
fn recorded_queue_only_report_does_not_request_an_automatic_turn() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "child-turn");
    let report = terminal_report_with_trigger(
        &coordinator,
        &child,
        "child-turn",
        /*trigger_turn*/ false,
    );
    let report_id = report.id.expect("report has a stable ID");
    let initial_wake = coordinator
        .claim_next_wake_request()
        .expect("delivery still wakes the dispatcher");
    initial_wake.complete();
    assert!(coordinator.mark_report_recorded(&report_id));

    assert!(!coordinator.has_recorded_reports(&parent));
    assert!(coordinator.claim_next_wake_request().is_none());
}

#[tokio::test]
async fn child_report_racing_parent_finalization_keeps_parent_waiting_until_resume() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "child-turn");
    let gate = Arc::new(tokio::sync::Barrier::new(2));

    let parent_task = {
        let coordinator = Arc::clone(&coordinator);
        let gate = Arc::clone(&gate);
        let parent = parent.clone();
        tokio::spawn(async move {
            gate.wait().await;
            coordinator.classify_turn_end(&parent, "root-turn", TurnEndDisposition::Succeeded)
        })
    };
    let child_task = {
        let coordinator = Arc::clone(&coordinator);
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.wait().await;
            terminal_report(&coordinator, &child, "child-turn")
        })
    };

    assert_eq!(
        parent_task
            .await
            .expect("parent classification task completes"),
        Ok(AssignmentPhase::Waiting)
    );
    let report = child_task.await.expect("child report task completes");
    let report_id = report.id.clone().expect("child report has an ID");
    assert!(coordinator.mark_report_recorded(&report_id));
    let prompt_input = input_with_report(&report);
    let accepted = coordinator.report_ids_in_prompt(&parent, &prompt_input);
    assert_eq!(coordinator.accept_reports(&parent, &accepted), 1);
    assert_eq!(
        coordinator.assignment_status(parent.thread_id),
        Some(AgentStatus::Waiting)
    );

    coordinator
        .begin_or_continue_assignment(
            parent.thread_id,
            None,
            "root-resumed-turn",
            /*allow_new_generation*/ false,
        )
        .expect("report starts a distinct resumed turn");
    assert_eq!(
        coordinator.classify_turn_end(&parent, "root-resumed-turn", TurnEndDisposition::Succeeded,),
        Ok(AssignmentPhase::Completed)
    );
}

fn terminal_report(
    coordinator: &Arc<AgentWakeCoordinator>,
    child: &AgentAssignmentId,
    terminal_turn_id: &str,
) -> InterAgentCommunication {
    terminal_report_with_trigger(
        coordinator,
        child,
        terminal_turn_id,
        /*trigger_turn*/ true,
    )
}

fn terminal_report_with_trigger(
    coordinator: &Arc<AgentWakeCoordinator>,
    child: &AgentAssignmentId,
    terminal_turn_id: &str,
    trigger_turn: bool,
) -> InterAgentCommunication {
    coordinator
        .classify_turn_end(child, terminal_turn_id, TurnEndDisposition::Succeeded)
        .expect("child turn is classified");
    let parent = coordinator
        .parent_assignment(child)
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
                trigger_turn,
            ),
        )
        .expect("active parent receives terminal report")
    {
        TerminalReportPublication::Published(report_id) => report_id,
        TerminalReportPublication::AlreadyPublished(_) => panic!("first publication is new"),
    };
    claim_report(coordinator, &parent, &report_id)
}

#[tokio::test]
async fn handler_cancelled_waiting_for_mailbox_lock_requeues_claim() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "mailbox-lock-child-turn");
    coordinator
        .classify_turn_end(
            &child,
            "mailbox-lock-child-turn",
            TurnEndDisposition::Succeeded,
        )
        .expect("child turn is classified");
    let TerminalReportPublication::Published(report_id) = coordinator
        .publish_terminal_report(
            &child,
            "mailbox-lock-child-turn",
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("valid child path"),
                AgentPath::root(),
                Vec::new(),
                "delegated work finished".to_string(),
                /*trigger_turn*/ true,
            ),
        )
        .expect("active parent receives terminal report")
    else {
        panic!("first report publication is new");
    };
    let claim = coordinator
        .claim_next_mailbox_report(&parent)
        .expect("report is claimed");
    assert_eq!(claim.id, report_id);
    assert!(claim.mark_enqueued());

    let mailbox_lock = Arc::new(AsyncMutex::new(()));
    let mailbox_guard = Arc::clone(&mailbox_lock)
        .try_lock_owned()
        .expect("mailbox mutex is initially available");
    let handler_guard = coordinator.delivery_guard(report_id.clone());
    let handler_lock = Arc::clone(&mailbox_lock);
    let lock_waiting = Arc::new(Notify::new());
    let task_lock_waiting = Arc::clone(&lock_waiting);
    let handler = tokio::spawn(async move {
        let _delivery_guard = handler_guard;
        let mut acquire = Box::pin(handler_lock.lock());
        let _mailbox_guard = std::future::poll_fn(|context| {
            let result = acquire.as_mut().poll(context);
            if result.is_pending() {
                task_lock_waiting.notify_one();
            }
            result
        })
        .await;
    });
    lock_waiting.notified().await;
    handler.abort();
    let _ = handler.await;
    drop(mailbox_guard);

    let retried = coordinator
        .claim_next_mailbox_report(&parent)
        .expect("cancelled handler restores the report for retry");
    assert_eq!(retried.id, report_id);
    drop(retried);
}

fn claim_report(
    coordinator: &Arc<AgentWakeCoordinator>,
    parent: &AgentAssignmentId,
    report_id: &codex_protocol::ResponseItemId,
) -> InterAgentCommunication {
    let claim = coordinator
        .claim_next_mailbox_report(parent)
        .expect("pending report can be claimed for delivery");
    assert_eq!(&claim.id, report_id);
    let communication = claim.communication.clone();
    assert!(claim.mark_enqueued());
    assert!(coordinator.acknowledge_report_mailbox_delivery(report_id, parent.thread_id));
    communication
}

#[test]
fn pending_report_is_woken_when_a_committed_assignment_starts() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let parent = coordinator
        .reserve_child_assignment(root.clone(), ThreadId::new())
        .expect("parent assignment is admitted")
        .commit()
        .expect("parent assignment commits before its initial turn");
    let child = new_child(&coordinator, &parent, "child-turn");
    coordinator
        .classify_turn_end(&child, "child-turn", TurnEndDisposition::Succeeded)
        .expect("child turn is classified");
    let report = coordinator
        .publish_terminal_report(
            &child,
            "child-turn",
            InterAgentCommunication::new(
                AgentPath::try_from("/root/parent/child").expect("valid child path"),
                AgentPath::try_from("/root/parent").expect("valid parent path"),
                Vec::new(),
                "delegated work finished".to_string(),
                /*trigger_turn*/ true,
            ),
        )
        .expect("committed parent retains its child's report");
    assert!(matches!(report, TerminalReportPublication::Published(_)));
    assert!(coordinator.claim_next_wake_request().is_none());

    coordinator
        .begin_or_continue_assignment(
            parent.thread_id,
            Some(root),
            "parent-first-turn",
            /*allow_new_generation*/ false,
        )
        .expect("committed parent starts its initial turn");
    assert!(coordinator.request_wake_for_pending_mailbox_reports(&parent));
    assert_eq!(
        coordinator
            .claim_next_wake_request()
            .and_then(|request| request.assignment().cloned()),
        Some(parent)
    );
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
    let child_report = claim_report(&coordinator, &root, &child_report_id);
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
    assert_eq!(wake_request.assignment(), Some(&parent));
    wake_request.complete();
    assert!(!coordinator.mark_report_recorded(&report_id));
    let failed_send = coordinator
        .claim_next_mailbox_report(&parent)
        .expect("pending report can be claimed for delivery");
    assert_eq!(failed_send.id, report_id);
    drop(failed_send);
    let report = claim_report(&coordinator, &parent, &report_id);
    assert!(coordinator.claim_next_mailbox_report(&parent).is_none());
    assert!(!coordinator.release_mailbox_claim(&report_id));
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
    let report = claim_report(&coordinator, &parent, &report_id);
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

#[test]
fn interrupt_resolves_running_assignment_without_a_resident_turn_task() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "turn-start-in-flight");

    assert!(
        coordinator
            .interrupt_idle_assignment(&child, "synthetic-interruption")
            .expect("current assignment can be interrupted")
    );
    assert!(
        !coordinator
            .interrupt_idle_assignment(&child, "duplicate-interruption")
            .expect("terminal assignment remains current")
    );
    assert!(
        coordinator
            .terminal_assignment_for_turn(child.thread_id, "synthetic-interruption")
            .is_some()
    );
    assert!(matches!(
        coordinator
            .publish_terminal_report(
                &child,
                "synthetic-interruption",
                InterAgentCommunication::new(
                    AgentPath::try_from("/root/worker").expect("valid child path"),
                    AgentPath::root(),
                    Vec::new(),
                    "child attempt interrupted".to_string(),
                    /*trigger_turn*/ true,
                ),
            )
            .expect("interruption report is published"),
        TerminalReportPublication::Published(_)
    ));
}

#[tokio::test]
async fn interrupting_waiting_assignment_releases_report_delivery_waiters() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let parent = new_root(&coordinator);
    let child = new_child(&coordinator, &parent, "child-turn");
    let grandchild = new_child(&coordinator, &child, "grandchild-turn");
    assert_eq!(
        coordinator.classify_turn_end(&child, "child-turn", TurnEndDisposition::Succeeded,),
        Ok(AssignmentPhase::Waiting)
    );
    coordinator
        .classify_turn_end(
            &grandchild,
            "grandchild-turn",
            TurnEndDisposition::Succeeded,
        )
        .expect("grandchild turn is classified");
    assert!(matches!(
        coordinator.publish_terminal_report(
            &grandchild,
            "grandchild-turn",
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker/grandchild").expect("valid grandchild path"),
                AgentPath::try_from("/root/worker").expect("valid child path"),
                Vec::new(),
                "grandchild finished".to_string(),
                /*trigger_turn*/ false,
            ),
        ),
        Some(TerminalReportPublication::Published(_))
    ));

    let mut report_waiter = Box::pin(coordinator.wait_for_queue_only_reports_enqueued(&child));
    let mut second_report_waiter =
        Box::pin(coordinator.wait_for_queue_only_reports_enqueued(&child));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut report_waiter)
            .await
            .is_err(),
        "queue-only report remains outstanding before mailbox insertion"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second_report_waiter)
            .await
            .is_err(),
        "concurrent callers wait for the same queue-only report"
    );
    assert!(
        coordinator
            .interrupt_idle_assignment(&child, "child-interrupted")
            .expect("waiting assignment can be interrupted")
    );
    tokio::time::timeout(Duration::from_secs(1), &mut report_waiter)
        .await
        .expect("interruption signals blocked report delivery waiters");
    tokio::time::timeout(Duration::from_secs(1), &mut second_report_waiter)
        .await
        .expect("interruption broadcasts to every report delivery waiter");
}

#[tokio::test]
async fn reload_config_lives_with_open_assignment_and_is_cleaned_up() {
    let coordinator = Arc::new(AgentWakeCoordinator::default());
    let root = new_root(&coordinator);
    let child = new_child(&coordinator, &root, "waiting-child-turn");
    let _grandchild = new_child(&coordinator, &child, "grandchild-turn");
    let config = crate::config::test_config().await;
    let expected_cwd = config.cwd.clone();

    assert!(coordinator.store_reload_config(&child, config));
    assert_eq!(
        coordinator.reload_config(&child).map(|config| config.cwd),
        Some(expected_cwd)
    );
    assert_eq!(
        coordinator.classify_turn_end(&child, "waiting-child-turn", TurnEndDisposition::Succeeded),
        Ok(AssignmentPhase::Waiting)
    );
    assert!(coordinator.reload_config(&child).is_some());

    coordinator.cancel_subtree(&[child.thread_id]);
    assert!(coordinator.reload_config(&child).is_none());
}

#[path = "coordinator_invariant_tests.rs"]
mod invariant_tests;
