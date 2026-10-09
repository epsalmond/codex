//! Serializes automatic completion delivery with intentional teardown, including evicted threads.

use super::ThreadManager;
use codex_protocol::ThreadId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::OwnedMutexGuard;

#[derive(Default)]
pub(crate) struct CompletionDeliveryGates(Mutex<HashMap<ThreadId, Weak<CompletionDeliveryState>>>);

#[derive(Default)]
pub(crate) struct CompletionDeliveryState {
    gate: Arc<tokio::sync::Mutex<()>>,
    inhibitors: AtomicUsize,
}

impl CompletionDeliveryGates {
    pub(crate) fn for_thread(&self, thread_id: ThreadId) -> Arc<CompletionDeliveryState> {
        let mut states = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        states.retain(|_, state| state.strong_count() > 0);
        if let Some(state) = states.get(&thread_id).and_then(Weak::upgrade) {
            return state;
        }
        let state = Arc::new(CompletionDeliveryState::default());
        states.insert(thread_id, Arc::downgrade(&state));
        state
    }

    async fn inhibit(&self, thread_ids: &[ThreadId]) -> CompletionDeliveryInhibition {
        // Own each increment immediately, so cancelling acquisition of a later gate rolls back.
        let mut inhibition = CompletionDeliveryInhibition(Vec::new());
        for thread_id in thread_ids {
            let state = self.for_thread(*thread_id);
            let _guard = state.lock().await;
            state.inhibitors.fetch_add(1, Ordering::AcqRel);
            inhibition.0.push(state);
        }
        inhibition
    }
}

impl CompletionDeliveryState {
    pub(crate) async fn lock(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.gate).lock_owned().await
    }

    pub(crate) fn is_inhibited(&self) -> bool {
        self.inhibitors.load(Ordering::Acquire) > 0
    }
}

/// Holds automatic completion delivery during archive or close preparation.
///
/// Dropping this guard restores eligibility on success, error, and cancellation. Persisted
/// archive state and explicitly closed registry membership independently prevent later recovery.
pub struct CompletionDeliveryInhibition(Vec<Arc<CompletionDeliveryState>>);

impl Drop for CompletionDeliveryInhibition {
    fn drop(&mut self) {
        for state in &self.0 {
            state.inhibitors.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl ThreadManager {
    /// Excludes automatic completion delivery before archive teardown, even for evicted agents.
    /// Keep the returned guard through the archive commit. Explicit collaboration remains allowed.
    pub async fn inhibit_automatic_agent_completions(
        &self,
        thread_ids: &[ThreadId],
    ) -> CompletionDeliveryInhibition {
        self.state.completion_delivery.inhibit(thread_ids).await
    }
}

impl super::ThreadManagerState {
    pub(crate) async fn inhibit_automatic_agent_completions(
        &self,
        thread_ids: &[ThreadId],
    ) -> CompletionDeliveryInhibition {
        self.completion_delivery.inhibit(thread_ids).await
    }
}

#[cfg(test)]
#[path = "completion_delivery_tests.rs"]
mod tests;
