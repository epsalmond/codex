use super::ConnectionCapabilities;
use super::LifecycleObserverClaimError;
use super::ThreadStateManager;
use crate::outgoing_message::ConnectionId;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn lifecycle_observer_owner_is_exclusive_and_survives_disconnect() {
    let manager = ThreadStateManager::new();
    let thread_id = ThreadId::new();
    let first_owner = ConnectionId(41);
    let second_owner = ConnectionId(42);
    let unattached_connection = ConnectionId(43);
    manager
        .connection_initialized(first_owner, ConnectionCapabilities::default())
        .await;
    manager
        .connection_initialized(second_owner, ConnectionCapabilities::default())
        .await;
    manager
        .connection_initialized(unattached_connection, ConnectionCapabilities::default())
        .await;
    assert!(
        manager
            .try_add_connection_to_thread(thread_id, first_owner)
            .await
    );
    assert!(
        manager
            .try_add_connection_to_thread(thread_id, second_owner)
            .await
    );

    assert_eq!(
        manager
            .claim_lifecycle_observer(thread_id, first_owner)
            .await,
        Ok(())
    );
    assert_eq!(
        manager
            .claim_lifecycle_observer(thread_id, unattached_connection)
            .await,
        Err(LifecycleObserverClaimError::ConnectionNotSubscribed)
    );
    assert_eq!(
        manager
            .claim_lifecycle_observer(thread_id, first_owner)
            .await,
        Ok(())
    );
    assert_eq!(
        manager
            .claim_lifecycle_observer(thread_id, second_owner)
            .await,
        Err(LifecycleObserverClaimError::AlreadyClaimed { owner: first_owner })
    );

    manager.remove_connection(first_owner).await;

    assert_eq!(
        manager.lifecycle_observer_owner(thread_id).await,
        Some(first_owner)
    );
    assert_eq!(
        manager
            .claim_lifecycle_observer(thread_id, second_owner)
            .await,
        Err(LifecycleObserverClaimError::AlreadyClaimed { owner: first_owner })
    );
}
