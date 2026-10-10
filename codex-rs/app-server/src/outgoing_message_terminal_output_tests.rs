use super::ConnectionId;
use super::OutgoingEnvelope;
use super::OutgoingMessage;
use super::OutgoingMessageSender;
use super::ServerNotification;
use super::ThreadScopedOutgoingMessageSender;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[tokio::test]
async fn terminal_notification_requires_write_ack_from_lifecycle_owner() {
    let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    let owner = ConnectionId(8);
    let scoped = ThreadScopedOutgoingMessageSender::new(
        outgoing,
        vec![ConnectionId(7), owner],
        ThreadId::new(),
    )
    .with_lifecycle_observer_owner(Some(owner));
    let send_task = tokio::spawn(async move {
        scoped
            .send_terminal_server_notification(turn_completed_notification())
            .await
    });

    let mut owner_write_complete_tx = None;
    let mut emitted_at_ms = Vec::new();
    for expected_connection_id in [ConnectionId(7), owner] {
        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("subscriber should receive terminal notification")
            .expect("outgoing channel should remain open");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx,
        } = envelope
        else {
            panic!("expected targeted terminal notification");
        };
        assert_eq!(connection_id, expected_connection_id);
        let OutgoingMessage::AppServerNotification(notification) = message else {
            panic!("expected app-server notification");
        };
        emitted_at_ms.push(notification.emitted_at_ms);
        if connection_id == owner {
            owner_write_complete_tx = write_complete_tx;
        } else {
            assert!(write_complete_tx.is_none());
        }
    }
    assert_eq!(emitted_at_ms[0], emitted_at_ms[1]);
    owner_write_complete_tx
        .expect("only the lifecycle owner should carry a write ack")
        .send(())
        .expect("the lifecycle sender should still await the owner's ack");

    assert_eq!(
        timeout(Duration::from_secs(1), send_task)
            .await
            .expect("sender should finish after owner write ack")
            .expect("sender task should not panic"),
        Some(true)
    );
}

#[tokio::test]
async fn terminal_notification_without_owner_recipient_is_not_acknowledged() {
    let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    let scoped =
        ThreadScopedOutgoingMessageSender::new(outgoing, vec![ConnectionId(7)], ThreadId::new())
            .with_lifecycle_observer_owner(Some(ConnectionId(8)));
    let send_task = tokio::spawn(async move {
        scoped
            .send_terminal_server_notification(turn_completed_notification())
            .await
    });

    let envelope = rx
        .recv()
        .await
        .expect("passive subscriber still receives output");
    let OutgoingEnvelope::ToConnection {
        connection_id,
        write_complete_tx,
        ..
    } = envelope
    else {
        panic!("expected targeted terminal notification");
    };
    assert_eq!(connection_id, ConnectionId(7));
    assert!(write_complete_tx.is_none());
    assert_eq!(
        send_task.await.expect("sender task should not panic"),
        Some(false)
    );
}

#[tokio::test]
async fn empty_recipient_list_does_not_prove_terminal_output_delivery() {
    let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    let owner = ConnectionId(8);
    let scoped = ThreadScopedOutgoingMessageSender::new(outgoing, Vec::new(), ThreadId::new())
        .with_lifecycle_observer_owner(Some(owner));

    assert_eq!(
        scoped
            .send_terminal_server_notification(turn_completed_notification())
            .await,
        Some(false)
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn dropped_owner_write_ack_is_reported_as_delivery_failure() {
    let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(2);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    let owner = ConnectionId(8);
    let scoped = ThreadScopedOutgoingMessageSender::new(outgoing, vec![owner], ThreadId::new())
        .with_lifecycle_observer_owner(Some(owner));
    let send_task = tokio::spawn(async move {
        scoped
            .send_terminal_server_notification(turn_completed_notification())
            .await
    });

    let envelope = rx
        .recv()
        .await
        .expect("owner should receive terminal output");
    let OutgoingEnvelope::ToConnection {
        connection_id,
        write_complete_tx,
        ..
    } = envelope
    else {
        panic!("expected targeted terminal notification");
    };
    assert_eq!(connection_id, owner);
    drop(write_complete_tx);
    assert_eq!(
        send_task.await.expect("sender task should not panic"),
        Some(false)
    );
}

#[tokio::test]
async fn cancelled_terminal_notification_wait_drops_owner_ack_receiver() {
    let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(2);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    let owner = ConnectionId(8);
    let scoped = ThreadScopedOutgoingMessageSender::new(outgoing, vec![owner], ThreadId::new())
        .with_lifecycle_observer_owner(Some(owner));
    let send_task = tokio::spawn(async move {
        scoped
            .send_terminal_server_notification(turn_completed_notification())
            .await
    });

    let envelope = rx
        .recv()
        .await
        .expect("owner should receive terminal output");
    let OutgoingEnvelope::ToConnection {
        connection_id,
        write_complete_tx,
        ..
    } = envelope
    else {
        panic!("expected targeted terminal notification");
    };
    assert_eq!(connection_id, owner);
    let write_complete_tx = write_complete_tx.expect("owner ack sender should be attached");

    send_task.abort();
    assert!(
        send_task
            .await
            .expect_err("send task should be cancelled")
            .is_cancelled()
    );
    assert!(write_complete_tx.send(()).is_err());
}

fn turn_completed_notification() -> ServerNotification {
    ServerNotification::TurnCompleted(TurnCompletedNotification {
        thread_id: "thread-1".to_string(),
        turn: Turn {
            id: "turn-1".to_string(),
            items: Vec::new(),
            items_view: TurnItemsView::NotLoaded,
            error: None,
            status: TurnStatus::Completed,
            started_at: None,
            completed_at: Some(123),
            duration_ms: Some(456),
            root_turn_id: None,
        },
        context_usage: None,
    })
}

#[tokio::test]
async fn cancelled_owner_ack_wait_reports_terminal_output_undelivered() {
    let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    let owner = ConnectionId(8);
    let cancel = tokio_util::sync::CancellationToken::new();
    let scoped = ThreadScopedOutgoingMessageSender::new(outgoing, vec![owner], ThreadId::new())
        .with_lifecycle_observer_owner(Some(owner))
        .with_terminal_ack_cancel(cancel.clone());
    let send_task = tokio::spawn(async move {
        scoped
            .send_terminal_server_notification(turn_completed_notification())
            .await
    });

    let OutgoingEnvelope::ToConnection {
        write_complete_tx, ..
    } = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("owner should receive terminal notification")
        .expect("outgoing channel should remain open")
    else {
        panic!("expected targeted terminal notification");
    };
    let _owner_write_complete_tx = write_complete_tx.expect("owner write ack is requested");
    cancel.cancel();

    assert_eq!(
        timeout(Duration::from_secs(1), send_task)
            .await
            .expect("cancellation should end the ack wait")
            .expect("sender task should not panic"),
        Some(false)
    );
}
