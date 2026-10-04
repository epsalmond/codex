use super::complete_event_handling;
use super::complete_event_handling_or_cancel;
use tokio::sync::oneshot;

#[tokio::test]
async fn listener_shutdown_cancels_an_event_waiting_for_terminal_output() {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let (started_tx, started_rx) = oneshot::channel();
    let event_handling = async move {
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    };
    let task = tokio::spawn(async move {
        complete_event_handling_or_cancel(&mut cancel_rx, event_handling).await
    });

    started_rx
        .await
        .expect("event handler should start before cancellation");
    cancel_tx
        .send(())
        .expect("listener should still be waiting on its event");
    assert!(!task.await.expect("listener wait task should not panic"));
}

#[tokio::test]
async fn completed_shutdown_event_wins_over_listener_cancellation() {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    cancel_tx
        .send(())
        .expect("cancel should be ready before event handling");

    assert!(complete_event_handling_or_cancel(&mut cancel_rx, async {}).await);
}

#[tokio::test]
async fn listener_cancellation_finishes_the_event_and_only_abandons_the_ack_wait() {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let (started_tx, started_rx) = oneshot::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    let terminal_ack_cancel = tokio_util::sync::CancellationToken::new();
    let ack_cancel = terminal_ack_cancel.clone();
    let event_handling = async move {
        let _ = started_tx.send(());
        ack_cancel.cancelled().await;
        let _ = finished_tx.send(());
    };
    let task = tokio::spawn(async move {
        complete_event_handling(&mut cancel_rx, &terminal_ack_cancel, event_handling).await
    });

    started_rx
        .await
        .expect("event handler should start before cancellation");
    cancel_tx
        .send(())
        .expect("listener should still be handling its event");
    assert!(!task.await.expect("listener task should not panic"));
    finished_rx
        .await
        .expect("event handling should run to completion after cancellation");
}

#[tokio::test]
async fn completed_event_continues_the_listener() {
    let (_cancel_tx, mut cancel_rx) = oneshot::channel();
    let terminal_ack_cancel = tokio_util::sync::CancellationToken::new();

    assert!(complete_event_handling(&mut cancel_rx, &terminal_ack_cancel, async {}).await);
    assert!(!terminal_ack_cancel.is_cancelled());
}
