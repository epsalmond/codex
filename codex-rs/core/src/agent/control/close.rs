//! Closes multi-agent V2 subtrees through the shared legacy teardown.
//! Closure is permanent: tombstones keep later references explaining why delivery failed.

use super::LocalAgentControl;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use std::sync::Arc;

impl LocalAgentControl {
    /// Close `agent_id` and every descendant, loaded, evicted or only persisted.
    ///
    /// Pending wakes are cancelled, loaded runtimes are shut down, spawn edges are marked
    /// closed, and registry membership, queued mail and residency are released. Tombstones
    /// are recorded first so input racing the teardown is rejected rather than reloading
    /// the agent. Closing an already closed agent succeeds.
    pub(crate) async fn close_v2_agent(&self, agent_id: ThreadId) -> CodexResult<()> {
        if self.runtime.registry.closed_agent_path(agent_id).is_some()
            && self.get_agent_metadata(agent_id).is_none()
        {
            return Ok(());
        }
        self.record_closed_subtree(agent_id).await?;
        Box::pin(self.close_agent(agent_id)).await.map(drop)
    }

    /// Closes a task agent after its final report was accepted, unless input reached it first.
    ///
    /// A delivery to a loaded agent holds a residency pin until the agent has handled it, so
    /// taking the residency gate exclusively waits for deliveries already admitted and holds back
    /// new ones. A follow-up among them starts a turn or stays queued, and the agent stays open for
    /// that turn; it closes after its next final report. Otherwise the agent is marked closed
    /// before the gate is released, so a delivery that was waiting fails as sent to a closed
    /// agent instead of being accepted and dropped by the teardown. Returns whether it closed.
    pub(crate) async fn close_task_agent_after_final(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<bool> {
        let state = self.runtime.upgrade()?;
        while let Ok(thread) = state.get_thread(agent_id).await {
            let _gate = Arc::clone(&thread.residency_gate).write_owned().await;
            if !state
                .get_thread(agent_id)
                .await
                .is_ok_and(|registered| Arc::ptr_eq(&registered, &thread))
            {
                // Unloaded or reloaded while we waited; decide against the current runtime.
                continue;
            }
            if thread.session.active_turn.lock().await.is_some()
                || thread
                    .session
                    .input_queue
                    .trigger_turn_mailbox_count()
                    .await
                    > 0
            {
                return Ok(false);
            }
            self.record_closed_subtree(agent_id).await?;
            break;
        }
        self.close_v2_agent(agent_id).await.map(|()| true)
    }

    /// Records tombstones for `agent_id` and its live descendants, so input racing the teardown
    /// is rejected rather than reloading them.
    async fn record_closed_subtree(&self, agent_id: ThreadId) -> CodexResult<()> {
        let metadata = self.runtime.ensure_agent_known(agent_id)?;
        if metadata.agent_path.as_ref().is_none_or(AgentPath::is_root) {
            return Err(CodexErr::InvalidRequest(
                "only spawned multi-agent v2 agents can be closed".to_string(),
            ));
        }
        let closed_agents = self
            .runtime
            .list_live_agent_subtree_thread_ids(agent_id)
            .await?
            .into_iter()
            .filter_map(|thread_id| {
                self.get_agent_metadata(thread_id)
                    .and_then(|metadata| metadata.agent_path)
                    .map(|agent_path| (thread_id, agent_path))
            })
            .collect::<Vec<_>>();
        self.runtime.registry.record_closed_agents(closed_agents);
        Ok(())
    }
}
