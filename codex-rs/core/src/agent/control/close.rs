//! Closes multi-agent V2 subtrees through the shared legacy teardown.
//! Closure is permanent: tombstones keep later references explaining why delivery failed.

use super::LocalAgentControl;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

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
        Box::pin(self.close_agent(agent_id)).await.map(drop)
    }
}
