use super::DrainInterrupt;
use super::DrainInterrupts;
use super::WorkLifecycle;
use codex_app_server_protocol::ThreadWorkSnapshot;
use codex_app_server_protocol::ThreadWorkUpdatedNotification;
use pretty_assertions::assert_eq;

fn snapshot(revision: &str, quiescent: bool) -> ThreadWorkSnapshot {
    ThreadWorkSnapshot {
        revision: revision.to_string(),
        outstanding_work: if quiescent { 0 } else { 1 },
        running_finite_work: if quiescent { 0 } else { 1 },
        pending_notifications: 0,
        active_root_turns: 0,
        pending_terminal_outputs: 0,
        output_forwarding_observed: true,
        closed: false,
        quiescent,
    }
}

#[test]
fn lifecycle_updates_are_scoped_to_the_exec_root() {
    let mut lifecycle = WorkLifecycle {
        thread_id: "root".to_string(),
        snapshot: snapshot("tree:1", /*quiescent*/ false),
        root_turn_open: false,
    };
    let unrelated = ThreadWorkUpdatedNotification {
        thread_id: "child".to_string(),
        snapshot: snapshot("child:2", /*quiescent*/ true),
    };

    assert!(!lifecycle.observe(&unrelated));
    assert_eq!(lifecycle.snapshot, snapshot("tree:1", /*quiescent*/ false));
    assert!(!lifecycle.ready_to_close());

    let root_update = ThreadWorkUpdatedNotification {
        thread_id: "root".to_string(),
        snapshot: snapshot("tree:3", /*quiescent*/ true),
    };
    assert!(lifecycle.observe(&root_update));
    assert_eq!(lifecycle.snapshot, snapshot("tree:3", /*quiescent*/ true));
    assert!(lifecycle.ready_to_close());
}

/// `review/start` only queues the review turn, so snapshots published before core admits it are
/// quiescent. Exec must not close until the root turn it started has completed.
#[test]
fn quiescent_snapshot_cannot_close_before_the_root_turn_completes() {
    let mut lifecycle = WorkLifecycle {
        thread_id: "root".to_string(),
        snapshot: snapshot("tree:0", /*quiescent*/ true),
        root_turn_open: true,
    };
    assert!(!lifecycle.ready_to_close());

    lifecycle.root_turn_completed();
    assert!(lifecycle.ready_to_close());

    // A wake turn reopens the gate until it completes.
    lifecycle.root_turn_started();
    assert!(!lifecycle.ready_to_close());
}

#[test]
fn first_ctrl_c_interrupts_the_active_root_turn() {
    let mut interrupts = DrainInterrupts::default();
    assert_eq!(
        interrupts.on_ctrl_c(/*root_turn_open*/ true),
        DrainInterrupt::InterruptTurn
    );
    // The interrupted turn may still be running, or a wake turn may have started.
    assert_eq!(
        interrupts.on_ctrl_c(/*root_turn_open*/ true),
        DrainInterrupt::StopDraining
    );
}

#[test]
fn ctrl_c_between_root_turns_stops_draining() {
    let mut interrupts = DrainInterrupts::default();
    assert_eq!(
        interrupts.on_ctrl_c(/*root_turn_open*/ false),
        DrainInterrupt::StopDraining
    );
}
