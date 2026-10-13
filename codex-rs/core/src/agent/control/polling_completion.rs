//! Recovers one definite preacceptance completion failure without scheduling a polling parent.

use super::*;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;

impl LocalAgentControl {
    pub(super) async fn deliver_polling_completion(
        &self,
        parent_thread_id: ThreadId,
        sender_thread_id: ThreadId,
        communication: InterAgentCommunication,
    ) -> CodexResult<()> {
        let state = self.runtime.upgrade()?;
        let lifecycle = state.completion_delivery.for_thread(parent_thread_id);
        let _guard = lifecycle.lock().await;
        if lifecycle.is_inhibited()
            || self
                .runtime
                .registry
                .completion_delivery_closed(parent_thread_id)
        {
            return Err(CodexErr::InvalidRequest(
                "completion target is closing or archiving".into(),
            ));
        }
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, sender_thread_id);
        match self
            .send_inter_agent_communication(
                parent_thread_id,
                communication.clone(),
                context.clone(),
                TurnStartOptions::default(),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {}
            Err(err) => return Err(err),
        }

        // Only current members of this tree may be automatically recovered. A caller's source
        // alone does not authorize reopening a closed parent or a replacement path identity.
        if self
            .runtime
            .registry
            .agent_id_for_path(&communication.recipient)
            != Some(parent_thread_id)
            || self
                .runtime
                .registry
                .agent_id_for_path(&communication.author)
                != Some(sender_thread_id)
        {
            return Err(CodexErr::ThreadNotFound(parent_thread_id));
        }
        let stored = state
            .read_stored_thread(ReadThreadParams {
                thread_id: parent_thread_id,
                include_archived: false,
                include_history: false,
            })
            .await?;
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: owner_thread_id,
            agent_path: Some(agent_path),
            ..
        }) = &stored.source
        else {
            return Err(CodexErr::InvalidRequest(
                "completion target is not a stored V2 child".into(),
            ));
        };
        let owner_path = communication
            .recipient
            .as_str()
            .rsplit_once('/')
            .map(|(path, _)| path);
        if stored.archived_at.is_some()
            || agent_path != &communication.recipient
            || stored
                .parent_thread_id
                .is_some_and(|id| id != *owner_thread_id)
            || stored
                .agent_path
                .as_deref()
                .is_some_and(|path| path != agent_path.as_str())
            || self
                .runtime
                .registry
                .agent_metadata_for_thread(*owner_thread_id)
                .and_then(|metadata| metadata.agent_path)
                .as_ref()
                .map(AgentPath::as_str)
                != owner_path
        {
            return Err(CodexErr::InvalidRequest(
                "completion target identity or ownership changed".into(),
            ));
        }
        if let Some(graph_store) = state.agent_graph_store()
            && !graph_store
                .list_thread_spawn_children(*owner_thread_id, Some(ThreadSpawnEdgeStatus::Open))
                .await
                .map_err(|err| CodexErr::InvalidRequest(err.to_string()))?
                .contains(&parent_thread_id)
        {
            return Err(CodexErr::InvalidRequest(
                "completion target has no open parent edge".into(),
            ));
        }
        let (parent, reload_error) = match state.get_thread(parent_thread_id).await {
            Ok(parent) => (parent, None),
            Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                let reload_error =
                    if let Some(config) = self.runtime.registry.evicted_config(parent_thread_id) {
                        // An owner-driven resume replaces target settings. Restore this target's own snapshot.
                        self.ensure_v2_agent_loaded(config, parent_thread_id, /*parent*/ None)
                            .await
                            .err()
                    } else {
                        None
                    };
                // Explicit resume publishes its runtime before clearing the saved settings.
                // It can also fill capacity while our loader prepares history. Validate that
                // winner before delivery, preserving the loader error if no valid runtime exists.
                let parent = match state.get_thread(parent_thread_id).await {
                    Ok(parent) => parent,
                    Err(err) => return Err(reload_error.unwrap_or(err)),
                };
                (parent, reload_error)
            }
            Err(err) => return Err(err),
        };
        if parent.multi_agent_version() != Some(MultiAgentVersion::V2)
            || parent.session_source != stored.source
            || !Arc::ptr_eq(
                &self.runtime.registry,
                &parent.session.services.local_agent_runtime.registry,
            )
        {
            return Err(reload_error.unwrap_or_else(|| {
                CodexErr::InvalidRequest(
                    "recovered completion target is not owned by this tree".into(),
                )
            }));
        }
        // Reuse the same identity and send once. Other failures may be ambiguous acceptance.
        self.send_inter_agent_communication(
            parent_thread_id,
            communication,
            context,
            TurnStartOptions::default(),
        )
        .await
        .map(|_| ())
    }
}
