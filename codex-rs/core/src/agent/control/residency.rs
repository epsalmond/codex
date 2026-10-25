//! Accounts for resident MAv2 children and unloads exact idle runtimes while preserving their state.

use super::LocalAgentControl;
use super::LocalAgentRuntime;
use super::coordinator::AgentWakeCoordinator;
use super::runtime::AgentTreeMembership;
use crate::agent::AgentStatus;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::ThreadId;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrKind;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::warn;

/// The result of trying to unload one exact MAv2 child runtime.
#[derive(Debug, PartialEq, Eq)]
pub enum ThreadEvictionOutcome {
    /// Work or another teardown owns the runtime; retry later.
    Busy,
    /// This operation stopped and removed the runtime.
    Evicted,
    /// The runtime was removed or replaced; leave its replacement alone.
    NotCurrent,
}

impl ThreadManagerState {
    pub(crate) async fn try_evict_v2_thread(
        self: &Arc<Self>,
        thread: Arc<CodexThread>,
        membership: &AgentTreeMembership,
    ) -> CodexResult<ThreadEvictionOutcome> {
        let Ok(residency_guard) = Arc::clone(&thread.residency_gate).try_write_owned() else {
            return Ok(ThreadEvictionOutcome::Busy);
        };
        let thread_id = thread.session.thread_id;
        if !self
            .get_thread(thread_id)
            .await
            .is_ok_and(|registered| Arc::ptr_eq(&registered, &thread))
        {
            return Ok(ThreadEvictionOutcome::NotCurrent);
        }
        if !is_resident_candidate(&thread) || !is_unloadable(&thread).await {
            return Ok(ThreadEvictionOutcome::Busy);
        }
        // The original config omits live settings; this context-only builder keeps them.
        let config = thread
            .session
            .new_startup_prewarm_turn_with_sub_id(uuid::Uuid::now_v7().to_string())
            .await
            .config
            .as_ref()
            .clone();
        let wake_coordinator = &thread.session.services.local_agent_runtime.wake_coordinator;
        if let Some(assignment) = wake_coordinator.current_assignment(thread_id) {
            wake_coordinator.store_reload_config(
                &assignment,
                thread.session.get_config().await.as_ref().clone(),
            );
        }
        let manager = Arc::clone(self);
        let teardown = membership
            .clone()
            .into_teardown_guard("resident_eviction", Some(thread_id));
        // Once shutdown is submitted, cancellation cannot release delivery or capacity early.
        tokio::spawn(async move {
            let _residency_guard = residency_guard;
            thread.ensure_rollout_materialized().await;
            if let Err(err) = thread.shutdown_and_wait().await {
                teardown.record_shutdown_failure("stop_resident", CodexErrKind::from(&err).into());
                teardown.complete();
                return Err(err);
            }
            // Dispatch has stopped and the residency guard excludes senders. A tree
            // shutdown deliberately discards its mailbox instead of retaining it.
            let mail = thread
                .session
                .input_queue
                .drain_mailbox()
                .await
                .into_iter()
                .map(|mail| mail.communication)
                .collect();
            let runtime = &thread.session.services.local_agent_runtime;
            let _ = runtime.mailboxes.enqueue(thread_id, /*id*/ None, mail);
            let environments = thread.environment_selections().await;
            let mut threads = manager.threads.write().await;
            if threads
                .get(&thread_id)
                .is_some_and(|registered| !Arc::ptr_eq(registered, &thread))
            {
                teardown.complete();
                return Ok(ThreadEvictionOutcome::NotCurrent);
            }
            runtime
                .registry
                .save_evicted_settings(thread_id, environments, config);
            let removed = threads.remove(&thread_id).is_some();
            runtime.residency.remove(thread_id);
            teardown.complete();
            Ok(if removed {
                ThreadEvictionOutcome::Evicted
            } else {
                ThreadEvictionOutcome::NotCurrent
            })
        })
        .await?
    }
}
pub(super) struct V2Residency {
    state: Mutex<V2ResidencyState>,
    wake_coordinator: Arc<AgentWakeCoordinator>,
}

#[derive(Default)]
struct V2ResidencyState {
    residents: VecDeque<ThreadId>,
    pending_slots: usize,
}

pub(super) struct V2ResidencySlot {
    residency: Arc<V2Residency>,
    active: bool,
}

impl V2ResidencySlot {
    pub(super) fn commit(mut self, thread_id: ThreadId) {
        self.residency.commit_slot(thread_id);
        self.active = false;
    }
}

impl Drop for V2ResidencySlot {
    fn drop(&mut self) {
        if self.active {
            self.residency.release_pending_slot();
            self.residency.wake_coordinator.notify_residency_available();
        }
    }
}

impl LocalAgentControl {
    pub(super) async fn reserve_v2_residency_slot(
        &self,
        state: &Arc<ThreadManagerState>,
        config: &Config,
        membership: &AgentTreeMembership,
        protected_thread_id: Option<ThreadId>,
    ) -> CodexResult<V2ResidencySlot> {
        let capacity = config
            .effective_agent_max_threads(MultiAgentVersion::V2)
            .unwrap_or(usize::MAX);
        Arc::clone(&self.runtime.residency)
            .reserve_slot(state, capacity, membership, protected_thread_id)
            .await
    }

    pub(crate) fn forget_v2_residency(&self, thread_id: ThreadId) {
        self.runtime.residency.remove(thread_id);
    }
}

impl LocalAgentRuntime {
    /// Retained-cell starts acquire this before admission and hold it through publication/cleanup.
    /// Read pins compose with normal dispatch's read pin; eviction alone holds the writer.
    pub(crate) async fn pin_owned_work_owner(
        &self,
        owner: &Arc<crate::session::session::Session>,
    ) -> CodexResult<Option<tokio::sync::OwnedRwLockReadGuard<()>>> {
        let Some(state) = self.manager.upgrade() else {
            return Ok(None);
        };
        let thread = state.get_thread(owner.thread_id()).await?;
        if !Arc::ptr_eq(&thread.session, owner) {
            return Err(CodexErr::ThreadNotFound(owner.thread_id()));
        }
        if !is_resident_candidate(&thread) {
            return Ok(None);
        }
        // Never queue behind a writer while a normal dispatch may already hold a read pin.
        let guard = Arc::clone(&thread.residency_gate)
            .try_read_owned()
            .map_err(|_| {
                CodexErr::InvalidRequest("work owner is closing or unloading".to_owned())
            })?;
        self.ensure_agent_not_closed(owner.thread_id())?;
        if !Arc::ptr_eq(&thread, &state.get_thread(owner.thread_id()).await?) {
            return Err(CodexErr::ThreadNotFound(owner.thread_id()));
        }
        self.residency.touch(owner.thread_id());
        self.wake_coordinator.notify_residency_available();
        Ok(Some(guard))
    }

    /// Pins and touches the registered runtime without waiting for unrelated eviction.
    pub(crate) async fn pin_v2_residency(
        &self,
        state: &ThreadManagerState,
        thread: &Arc<CodexThread>,
    ) -> CodexResult<Option<tokio::sync::OwnedRwLockReadGuard<()>>> {
        if !is_resident_candidate(thread) {
            return Ok(None);
        }
        let guard = Arc::clone(&thread.residency_gate).read_owned().await;
        let thread_id = thread.session.thread_id;
        // A close holding the gate marks the agent closed before releasing it.
        self.ensure_agent_not_closed(thread_id)?;
        if !Arc::ptr_eq(thread, &state.get_thread(thread_id).await?) {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        self.residency.touch(thread_id);
        // Touching changes eviction order, so a waiting wake may now find capacity.
        self.wake_coordinator.notify_residency_available();
        Ok(Some(guard))
    }
}

impl V2Residency {
    pub(super) fn new(wake_coordinator: Arc<AgentWakeCoordinator>) -> Self {
        Self {
            state: Mutex::default(),
            wake_coordinator,
        }
    }

    async fn reserve_slot(
        self: Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        capacity: usize,
        membership: &AgentTreeMembership,
        protected_thread_id: Option<ThreadId>,
    ) -> CodexResult<V2ResidencySlot> {
        loop {
            if self.try_reserve_pending_slot(capacity) {
                return Ok(V2ResidencySlot {
                    residency: self,
                    active: true,
                });
            }
            if !self
                .try_unload_one_resident(manager, membership, protected_thread_id)
                .await
            {
                return Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                    max_threads: capacity,
                })
                .with_agent_context(AgentErrorContext::ResidencyCapacity));
            }
        }
    }

    fn try_reserve_pending_slot(&self, capacity: usize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.residents.len().saturating_add(state.pending_slots) >= capacity {
            return false;
        }
        state.pending_slots += 1;
        true
    }

    async fn try_unload_one_resident(
        self: &Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        membership: &AgentTreeMembership,
        protected_thread_id: Option<ThreadId>,
    ) -> bool {
        // Keep shutting-down workers counted until removal. Each runtime's write guard
        // excludes delivery and competing evictions without blocking unrelated workers.
        let candidates = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .clone();
        for candidate_thread_id in candidates {
            if Some(candidate_thread_id) == protected_thread_id {
                continue;
            }
            let candidate_thread = {
                let threads = manager.threads.read().await;
                match threads.get(&candidate_thread_id) {
                    Some(thread) if is_resident_candidate(thread) => Arc::clone(thread),
                    Some(_) | None => {
                        // A reload cannot publish between the lookup and stale-entry removal.
                        self.remove(candidate_thread_id);
                        return true;
                    }
                }
            };
            // Capacity eviction keeps its existing policy of retaining unstarted workers.
            if matches!(
                candidate_thread.agent_status().await,
                AgentStatus::PendingInit
            ) {
                continue;
            }
            match manager
                .try_evict_v2_thread(candidate_thread, membership)
                .await
            {
                Ok(ThreadEvictionOutcome::Evicted | ThreadEvictionOutcome::NotCurrent) => {
                    return true;
                }
                Ok(ThreadEvictionOutcome::Busy) => {}
                Err(err) => warn!("v2 resident eviction failed: {err}"),
            }
        }
        false
    }

    fn touch(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        touch_resident(&mut state.residents, thread_id);
    }

    fn remove(&self, thread_id: ThreadId) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .retain(|resident_thread_id| *resident_thread_id != thread_id);
        self.wake_coordinator.notify_residency_available();
    }

    fn commit_slot(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
        touch_resident(&mut state.residents, thread_id);
    }

    fn release_pending_slot(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
    }
}

fn touch_resident(residents: &mut VecDeque<ThreadId>, thread_id: ThreadId) {
    residents.retain(|resident_thread_id| *resident_thread_id != thread_id);
    residents.push_back(thread_id);
}

fn is_resident_candidate(thread: &CodexThread) -> bool {
    thread.multi_agent_version() == Some(MultiAgentVersion::V2)
        && is_v2_resident_session_source(&thread.session_source)
}

pub(super) fn is_v2_resident_session_source(session_source: &SessionSource) -> bool {
    matches!(session_source, SessionSource::SubAgent(_))
}

async fn is_unloadable(thread: &CodexThread) -> bool {
    // Read lifecycle before any wake/store checks, including the exclusive-gate final recheck.
    if thread
        .session
        .services
        .local_agent_runtime
        .has_live_owned_work(thread.session.thread_id())
    {
        return false;
    }
    matches!(
        thread.agent_status().await,
        AgentStatus::PendingInit
            | AgentStatus::Waiting
            | AgentStatus::Completed(_)
            | AgentStatus::Errored(_)
            | AgentStatus::Interrupted
    ) && thread.session.active_turn.lock().await.is_none()
        && !thread.session.services.async_completions.has_owned_work()
        && !thread.session.has_outstanding_durable_sleep()
        // Count trigger mail even while wakeups are paused: unloading keeps the mail but drops
        // the pause and each mail's start options, so a reload would start an unexpected turn.
        && thread
            .session
            .input_queue
            .trigger_turn_mailbox_count()
            .await
            == 0
}

#[cfg(test)]
#[path = "residency_tests.rs"]
mod tests;
