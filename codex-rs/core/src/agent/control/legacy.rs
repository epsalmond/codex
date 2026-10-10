use super::*;
use crate::agent::api::AgentInfo;
use codex_protocol::error::CodexErrorDetails;
use codex_thread_store::PersistContext;

impl LocalAgentControl {
    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.runtime.upgrade()?;
        let result = if let Ok(thread) = state.get_thread(agent_id).await {
            thread
                .session
                .ensure_rollout_materialized(PersistContext::Standard)
                .await;
            thread.session.flush_rollout().await?;
            let result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
                Ok(String::new())
            } else {
                state
                    .send_op(
                        agent_id,
                        Op::Shutdown {},
                        /*parent_turn_id*/ None,
                        /*root_turn_id*/ None,
                    )
                    .await
            };
            thread.wait_until_terminated().await;
            result
        } else {
            state
                .send_op(
                    agent_id,
                    Op::Shutdown {},
                    /*parent_turn_id*/ None,
                    /*root_turn_id*/ None,
                )
                .await
        };
        let _ = state.remove_thread(&agent_id).await;
        self.forget_v2_residency(agent_id);
        self.runtime.registry.release_spawned_thread(agent_id);
        result
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down its
    /// registered and persisted descendants, including agents that are currently evicted.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<AgentInfo> {
        let state = self.runtime.upgrade()?;
        let mut subtree_thread_ids = self
            .runtime
            .list_live_agent_subtree_thread_ids(agent_id)
            .await?;
        if let Some(agent_graph_store) = state.agent_graph_store() {
            for descendant_id in agent_graph_store
                .list_thread_spawn_descendants(
                    agent_id,
                    Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
                )
                .await
                .map_err(|err| {
                    CodexErr::Fatal(format!("failed to load thread-spawn descendants: {err}"))
                })?
            {
                if !subtree_thread_ids.contains(&descendant_id) {
                    subtree_thread_ids.push(descendant_id);
                }
            }
        }
        let completion_inhibition = state
            .inhibit_automatic_agent_completions(&subtree_thread_ids)
            .await;
        for child in self
            .runtime
            .wake_coordinator
            .cancel_subtree(&subtree_thread_ids)
        {
            self.runtime.publish_waiting_parent_report(
                child,
                /*child_agent_path*/ None,
                crate::session_prefix::CLOSED_CHILD_NOTE,
            );
        }
        let metadata = self.get_agent_metadata(agent_id);
        let known_agent = metadata.is_some();
        if let Some(agent_graph_store) = state.agent_graph_store() {
            for descendant_id in subtree_thread_ids
                .iter()
                .copied()
                .filter(|id| *id != agent_id)
            {
                if let Err(err) = agent_graph_store
                    .set_thread_spawn_edge_status(
                        descendant_id,
                        codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                    )
                    .await
                {
                    warn!(
                        "failed to persist descendant thread-spawn edge closure for {descendant_id}: {err}"
                    );
                }
            }
        }
        let snapshot = match state.get_thread(agent_id).await {
            Ok(thread) => {
                let agent = LiveAgent {
                    thread_id: agent_id,
                    metadata: metadata.unwrap_or_default(),
                    status: thread.agent_status().await,
                };
                let config = Box::new(thread.config_snapshot().await);
                if !config.ephemeral
                    && let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    warn!("failed to persist thread-spawn edge status for {agent_id}: {err}");
                }
                AgentInfo::Loaded { agent, config }
            }
            Err(err)
                if known_agent && matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) =>
            {
                if let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    return Err(CodexErr::Fatal(format!(
                        "failed to persist stale thread-spawn edge status for {agent_id}: {err}"
                    )));
                }
                AgentInfo::Unloaded(metadata.unwrap_or_default())
            }
            Err(err) => return Err(err),
        };
        for thread_id in &subtree_thread_ids {
            self.runtime.registry.close_completion_delivery(*thread_id);
        }
        // Shutdown callbacks can report to these members; do not hold their gates while waiting.
        drop(completion_inhibition);
        match Box::pin(self.shutdown_agent_tree_with_ids(&subtree_thread_ids)).await {
            Err(err)
                if known_agent
                    && matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
            {
                Ok(snapshot)
            }
            result => result.map(|_| snapshot),
        }
    }

    /// Shut down `agent_id` and every registered descendant, including evicted agents.
    #[cfg(test)]
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let subtree_thread_ids = self
            .runtime
            .list_live_agent_subtree_thread_ids(agent_id)
            .await?;
        self.shutdown_agent_tree_with_ids(&subtree_thread_ids).await
    }

    async fn shutdown_agent_tree_with_ids(
        &self,
        subtree_thread_ids: &[ThreadId],
    ) -> CodexResult<String> {
        let Some((&agent_id, descendant_ids)) = subtree_thread_ids.split_first() else {
            return Err(CodexErr::InvalidRequest(
                "cannot shut down an empty agent subtree".to_string(),
            ));
        };
        let result = self.shutdown_live_agent(agent_id).await;
        for descendant_id in descendant_ids.iter().copied() {
            match self.shutdown_live_agent(descendant_id).await {
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) => {}
                Err(err) => return Err(err),
            }
        }
        result
    }
}
