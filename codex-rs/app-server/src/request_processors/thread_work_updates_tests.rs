use super::ThreadWorkUpdates;
use crate::outgoing_message::ConnectionId;
use codex_app_server_protocol::ServerNotification;
use codex_core::WorkObservationSnapshot;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use tokio::sync::watch;

fn snapshot(revision: &str, quiescent: bool) -> WorkObservationSnapshot {
    WorkObservationSnapshot {
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

#[tokio::test]
async fn reattached_watcher_emits_unread_final_snapshot_without_next_mutation() {
    let initial = snapshot("tree:1", /*quiescent*/ false);
    let (updates_tx, old_receiver) = watch::channel(initial.clone());
    let thread_id = ThreadId::new();
    let owner = ConnectionId(17);
    let mut old_updates = ThreadWorkUpdates::from_listener_parts(owner, thread_id, old_receiver);
    old_updates
        .next_notification()
        .await
        .expect("initial watcher snapshot should be available")
        .expect("initial snapshot should convert");

    updates_tx
        .send(snapshot("tree:2", /*quiescent*/ true))
        .expect("old listener should still have an update receiver");

    // Reattachment observes the current revision and makes that revision the new initial item.
    // The previous receiver has not consumed tree:2, and there is no subsequent producer update.
    let reattached_receiver = updates_tx.subscribe();
    let mut reattached =
        ThreadWorkUpdates::from_listener_parts(owner, thread_id, reattached_receiver);
    let notification = reattached
        .next_notification()
        .await
        .expect("reattachment must expose its initial snapshot")
        .expect("final snapshot should convert");
    let ServerNotification::ThreadWorkUpdated(notification) = notification else {
        panic!("watcher should produce a work-state notification");
    };
    assert_eq!(notification.snapshot.revision, "tree:2");
    assert!(notification.snapshot.quiescent);
}
