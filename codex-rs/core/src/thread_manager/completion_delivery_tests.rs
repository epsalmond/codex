use super::*;
use futures::poll;

#[tokio::test]
async fn archive_waits_for_delivery_and_releases_inhibition_on_drop() {
    let gates = CompletionDeliveryGates::default();
    let thread_id = ThreadId::new();
    let state = gates.for_thread(thread_id);
    let delivery = state.lock().await;
    let ids = [thread_id];
    let mut archive = Box::pin(gates.inhibit(&ids));
    assert!(poll!(archive.as_mut()).is_pending());
    assert!(!state.is_inhibited());
    drop(delivery);
    let inhibition = archive.await;
    assert!(state.is_inhibited());
    assert!(Arc::ptr_eq(&state, &gates.for_thread(thread_id)));
    drop(inhibition);
    assert!(!state.is_inhibited());
}

#[tokio::test]
async fn cancelling_partial_archive_acquisition_releases_prior_members() {
    let gates = CompletionDeliveryGates::default();
    let ids = [ThreadId::new(), ThreadId::new()];
    let first = gates.for_thread(ids[0]);
    let second = gates.for_thread(ids[1]);
    let delivery = second.lock().await;
    let mut archive = Box::pin(gates.inhibit(&ids));
    assert!(poll!(archive.as_mut()).is_pending());
    assert!(first.is_inhibited());
    assert!(!second.is_inhibited());
    drop(archive);
    assert!(!first.is_inhibited());
    drop(delivery);
    let inhibition = gates.inhibit(&ids).await;
    assert!(first.is_inhibited());
    assert!(second.is_inhibited());
    drop(inhibition);
    assert!(!first.is_inhibited());
    assert!(!second.is_inhibited());
}

#[tokio::test]
async fn overlapping_teardowns_do_not_restore_delivery_before_both_finish() {
    let gates = CompletionDeliveryGates::default();
    let ids = [ThreadId::new()];
    let first = gates.inhibit(&ids).await;
    let second = gates.inhibit(&ids).await;
    let state = gates.for_thread(ids[0]);
    drop(first);
    assert!(state.is_inhibited());
    drop(second);
    assert!(!state.is_inhibited());
}
