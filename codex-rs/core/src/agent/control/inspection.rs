//! Reads registered agent identity, status and settings without restoring a runtime.
//! Known unloaded agents stay distinct from missing identities and backend failures.

use super::LocalAgentControl;
use super::LocalAgentRuntime;
use crate::agent::api::AgentInfo;
use crate::agent::types::AgentContextUsage;
use crate::agent::types::LiveAgent;
use crate::agent::types::PendingAgentWork;
use crate::agent::types::PendingAgentWorkItem;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::AgentStatus;

impl LocalAgentControl {
    pub(super) async fn inspect_agent(&self, thread_id: ThreadId) -> CodexResult<AgentInfo> {
        let manager = self.runtime.upgrade()?;
        let thread = match manager.get_thread(thread_id).await {
            Ok(thread) => thread,
            Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                return Ok(AgentInfo::Unloaded(
                    self.runtime.ensure_agent_known(thread_id)?,
                ));
            }
            Err(err) => return Err(err),
        };
        Ok(AgentInfo::Loaded {
            agent: LiveAgent {
                thread_id,
                metadata: self.get_agent_metadata(thread_id).unwrap_or_default(),
                status: thread.agent_status().await,
            },
            config: Box::new(thread.config_snapshot().await),
        })
    }
}

impl LocalAgentRuntime {
    /// Finite results retained for this owner. Unknown runtime state stays unavailable rather
    /// than being reported as an empty queue; known unloaded agents have no retained work.
    pub(crate) async fn agent_pending_work(&self, thread_id: ThreadId) -> Option<PendingAgentWork> {
        let manager = self.upgrade().ok()?;
        let thread = match manager.get_thread(thread_id).await {
            Ok(thread) => thread,
            Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                self.ensure_agent_known(thread_id).ok()?;
                return Some(PendingAgentWork::default());
            }
            Err(_) => return None,
        };
        let jobs = thread
            .session
            .services
            .async_completions
            .pending()
            .into_iter()
            .filter(|record| record.is_finite)
            .map(|record| PendingAgentWorkItem {
                id: record.id,
                call_id: record.call_id,
                title: record.title,
                command: record.command,
                process_id: record.process_id.map(|id| id.to_string()),
                cell_id: record.cell_id,
                ready: record.ready,
            })
            .collect::<Vec<_>>();
        Some(PendingAgentWork {
            count: jobs.len(),
            jobs,
        })
    }

    /// Closing idle agents must preserve finite records through acceptance, including inline
    /// producers and ready results. Service records do not keep an otherwise idle agent open.
    pub(crate) async fn agent_is_idle_for_close(&self, thread_id: ThreadId) -> bool {
        let Ok(manager) = self.upgrade() else {
            return false;
        };
        let thread = match manager.get_thread(thread_id).await {
            Ok(thread) => thread,
            Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                return self.ensure_agent_known(thread_id).is_ok();
            }
            Err(_) => return false,
        };
        matches!(
            thread.agent_status().await,
            AgentStatus::Completed(_)
                | AgentStatus::Errored(_)
                | AgentStatus::Interrupted
                | AgentStatus::Shutdown
        ) && thread.session.active_turn.lock().await.is_none()
            && !thread
                .session
                .services
                .async_completions
                .has_finite_owned_work()
            && !thread.session.has_outstanding_durable_sleep()
            && thread
                .session
                .input_queue
                .trigger_turn_mailbox_count()
                .await
                == 0
    }

    /// Returns `None` when the agent is not loaded, so parents can tell unavailable from zero.
    pub(crate) async fn agent_context_usage(
        &self,
        thread_id: ThreadId,
    ) -> Option<AgentContextUsage> {
        let thread = self.upgrade().ok()?.get_thread(thread_id).await.ok()?;
        Some(thread.session.context_usage().await)
    }
}
