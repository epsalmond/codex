//! Resolves controller targets using the caller's registered identity.
//! Legacy callers can still supply their captured session source for path resolution.

use super::LocalAgentControl;
use crate::agent::api::AgentTarget;
use crate::agent::context_policy;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubagentContextReductionOverrides;
use codex_thread_store::ReadThreadParams;

impl LocalAgentControl {
    pub(crate) async fn set_agent_context_policy(
        &self,
        caller: ThreadId,
        target: AgentTarget,
        patch: SubagentContextReductionOverrides,
        inherit_to_children: bool,
        reset: bool,
    ) -> CodexResult<(u64, Option<u64>, bool)> {
        let target = self.resolve_target(caller, &target)?;
        if target == caller {
            return Err(CodexErr::InvalidRequest(
                "set_agent_context_policy can only update a descendant".to_string(),
            ));
        }
        context_policy::validate_overrides(&patch).map_err(CodexErr::InvalidRequest)?;
        if reset && !overrides_are_empty(&patch) {
            return Err(CodexErr::InvalidRequest(
                "reset cannot be combined with context_policy fields".to_string(),
            ));
        }

        let manager = self.upgrade()?;
        let thread = manager.get_thread(target).await.map_err(|error| {
            if matches!(
                error.details(),
                codex_protocol::error::CodexErrorDetails::ThreadNotFound(_)
            ) {
                CodexErr::InvalidRequest(
                    "target agent must be loaded; resume it before changing its context policy"
                        .to_string(),
                )
            } else {
                error
            }
        })?;
        self.ensure_descendant(caller, target, &thread, &manager)
            .await?;
        if !matches!(
            thread.config_snapshot().await.session_source,
            SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn { .. })
        ) {
            return Err(CodexErr::InvalidRequest(
                "context policy can only be set on spawned subagents".to_string(),
            ));
        }
        thread
            .session
            .update_subagent_context_reduction_policy(patch, inherit_to_children, reset)
            .await
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))
    }

    async fn ensure_descendant(
        &self,
        caller: ThreadId,
        target: ThreadId,
        target_thread: &crate::codex_thread::CodexThread,
        manager: &super::ThreadManagerState,
    ) -> CodexResult<()> {
        let mut current = target;
        let mut parent = target_thread.config_snapshot().await.parent_thread_id;
        for _ in 0..128 {
            let parent_thread_id = match parent {
                Some(parent) => parent,
                None => manager
                    .read_stored_thread(ReadThreadParams {
                        thread_id: current,
                        include_archived: true,
                        include_history: false,
                    })
                    .await?
                    .parent_thread_id
                    .ok_or_else(|| {
                        CodexErr::InvalidRequest(
                            "context policy target is not a descendant of the caller".to_string(),
                        )
                    })?,
            };
            if parent_thread_id == caller {
                return Ok(());
            }
            current = parent_thread_id;
            parent = match manager.get_thread(current).await {
                Ok(thread) => thread.config_snapshot().await.parent_thread_id,
                Err(_) => None,
            };
        }
        Err(CodexErr::InvalidRequest(
            "context policy target lineage is invalid".to_string(),
        ))
    }

    pub(crate) fn resolve_target(
        &self,
        caller: ThreadId,
        target: &AgentTarget,
    ) -> CodexResult<ThreadId> {
        match target {
            AgentTarget::Id(thread_id) => Ok(*thread_id),
            AgentTarget::Reference(reference) => {
                let caller = self.ensure_agent_known(caller)?;
                self.resolve_path_reference(
                    &caller.agent_path.unwrap_or_else(AgentPath::root),
                    reference,
                )
            }
        }
    }

    pub(crate) async fn resolve_agent_reference(
        &self,
        _current_thread_id: ThreadId,
        current_session_source: &SessionSource,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let current_agent_path = current_session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        self.resolve_path_reference(&current_agent_path, agent_reference)
    }

    fn resolve_path_reference(
        &self,
        current_agent_path: &AgentPath,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let agent_path = current_agent_path
            .resolve(agent_reference)
            .map_err(CodexErr::UnsupportedOperation)?;
        if let Some(thread_id) = self.state.agent_id_for_path(&agent_path) {
            return Ok(thread_id);
        }
        Err(CodexErr::UnsupportedOperation(format!(
            "live agent path `{}` not found",
            agent_path.as_str()
        )))
    }
}

fn overrides_are_empty(overrides: &SubagentContextReductionOverrides) -> bool {
    overrides.enabled.is_none()
        && overrides.threshold_tokens.is_none()
        && overrides.check_after_tools.is_none()
        && overrides.shake.is_none()
        && overrides.on_failure.is_none()
}
