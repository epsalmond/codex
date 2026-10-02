//! Tracks local running capacity and releases reservations through the shared guard.
//! The local permit owns the running count; root and MAv1 turns remain unrestricted.

use super::LocalAgentControl;
use super::coordinator::AgentWakeCoordinator;
use crate::agent::types::AgentExecutionGuard;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

#[derive(Default)]
pub(super) struct AgentExecutionLimiter {
    max_threads: OnceLock<usize>,
    permits: OnceLock<Arc<Semaphore>>,
}

struct LocalExecutionPermit {
    permit: Option<OwnedSemaphorePermit>,
    wake_coordinator: Weak<AgentWakeCoordinator>,
}

impl Drop for LocalExecutionPermit {
    fn drop(&mut self) {
        drop(self.permit.take());
        if let Some(coordinator) = self.wake_coordinator.upgrade() {
            coordinator.notify_capacity_available();
        }
    }
}

impl LocalAgentControl {
    pub(crate) fn ensure_execution_capacity(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> CodexResult<()> {
        if !is_execution_limited(multi_agent_version, session_source) {
            return Ok(());
        }
        let max_threads = self.runtime.agent_execution_limiter.max_threads();
        if self.runtime.agent_execution_limiter.has_capacity() {
            Ok(())
        } else {
            Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                max_threads,
            }))
        }
    }

    pub(crate) fn execution_guard<'a>(
        &'a self,
        multi_agent_version: MultiAgentVersion,
        session_source: &'a SessionSource,
    ) -> impl std::future::Future<Output = Option<AgentExecutionGuard>> + Send + 'a {
        async move {
            if !is_execution_limited(multi_agent_version, session_source) {
                return None;
            }
            Arc::clone(&self.runtime.agent_execution_limiter)
                .guard(Arc::downgrade(&self.runtime.wake_coordinator))
                .await
        }
    }
}

impl AgentExecutionLimiter {
    pub(super) fn initialize(&self, max_threads: usize) {
        let max_threads = *self.max_threads.get_or_init(|| max_threads);
        self.permits
            .get_or_init(|| Arc::new(Semaphore::new(max_threads)));
    }

    fn max_threads(&self) -> usize {
        self.max_threads.get().copied().unwrap_or(usize::MAX)
    }

    fn has_capacity(&self) -> bool {
        self.permits
            .get()
            .is_none_or(|permits| permits.available_permits() > 0)
    }

    async fn guard(
        self: Arc<Self>,
        wake_coordinator: Weak<AgentWakeCoordinator>,
    ) -> Option<AgentExecutionGuard> {
        let permits = Arc::clone(self.permits.get()?);
        let permit = permits.acquire_owned().await.ok()?;
        Some(AgentExecutionGuard::new(LocalExecutionPermit {
            permit: Some(permit),
            wake_coordinator,
        }))
    }
}

fn is_execution_limited(
    multi_agent_version: MultiAgentVersion,
    session_source: &SessionSource,
) -> bool {
    multi_agent_version == MultiAgentVersion::V2
        && matches!(session_source, SessionSource::SubAgent(_))
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
