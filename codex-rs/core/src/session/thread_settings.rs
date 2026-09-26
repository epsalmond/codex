//! Handles persistent thread-settings updates and serializes their persistence
//! with checkpoints written directly to storage.

use super::session::Session;
use super::session::SessionSettingsUpdate;
use super::step_settings::StepSettingsUpdate;
use crate::agent::context_policy;
use crate::config::ConstraintResult;
use codex_history::RolloutItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SubagentContextReductionOverrides;
use codex_protocol::protocol::SubagentContextReductionPolicyState;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_thread_store::ThreadStoreResult;
use std::sync::Arc;
use tokio::sync::SemaphorePermit;

impl Session {
    /// Captures and flushes current settings under the shared persistence permit.
    pub(crate) async fn checkpoint_thread_settings(&self) -> ThreadStoreResult<()> {
        let _settings_guard = acquire_persistence_lock(self).await;
        if let Some(live_thread) = self.live_thread() {
            live_thread
                .append_items(&[RolloutItem::EventMsg(applied_event(self).await)])
                .await?;
            live_thread.flush().await?;
        }
        Ok(())
    }
}

/// Updates and durably checkpoints a child policy before the caller acknowledges it.
pub(super) async fn update_subagent_context_reduction_policy(
    session: &Session,
    patch: SubagentContextReductionOverrides,
    inherit_to_children: bool,
    reset: bool,
) -> ThreadStoreResult<(u64, Option<u64>, bool)> {
    context_policy::validate_overrides(&patch)
        .map_err(|message| codex_thread_store::ThreadStoreError::InvalidRequest { message })?;
    let _settings_guard = acquire_persistence_lock(session).await;
    let mut policy = session
        .subagent_context_reduction_policy()
        .await
        .unwrap_or_default();
    if reset {
        context_policy::reset_local(&mut policy);
    } else {
        context_policy::update_local(&mut policy, patch, inherit_to_children);
    }
    let updates = SessionSettingsUpdate {
        subagent_context_reduction_policy: Some(policy.clone()),
        ..Default::default()
    };
    if session.live_thread().is_none() {
        if !session.is_ephemeral().await {
            return Err(codex_thread_store::ThreadStoreError::Unsupported {
                operation: "persistent_thread_settings",
            });
        }
        session.update_settings(updates).await.map_err(|error| {
            codex_thread_store::ThreadStoreError::InvalidRequest {
                message: error.to_string(),
            }
        })?;
        return Ok((
            policy.desired_revision,
            session.applied_context_policy_revision().await,
            false,
        ));
    }
    let snapshot = session
        .preview_thread_settings_snapshot(&updates)
        .await
        .map_err(
            |error| codex_thread_store::ThreadStoreError::InvalidRequest {
                message: error.to_string(),
            },
        )?;
    persist_settings_snapshot(session, snapshot).await?;
    session.update_settings(updates).await.map_err(|error| {
        codex_thread_store::ThreadStoreError::InvalidRequest {
            message: error.to_string(),
        }
    })?;
    Ok((
        policy.desired_revision,
        session.applied_context_policy_revision().await,
        true,
    ))
}

/// Installs inherited and spawn-time policy layers before a child receives its first input.
pub(super) async fn install_subagent_context_reduction_policy(
    session: &Session,
    policy: SubagentContextReductionPolicyState,
) -> ThreadStoreResult<()> {
    context_policy::validate_state(&policy)
        .map_err(|message| codex_thread_store::ThreadStoreError::InvalidRequest { message })?;
    let _settings_guard = acquire_persistence_lock(session).await;
    let updates = SessionSettingsUpdate {
        subagent_context_reduction_policy: Some(policy),
        ..Default::default()
    };
    if session.live_thread().is_some() {
        let snapshot = session
            .preview_thread_settings_snapshot(&updates)
            .await
            .map_err(
                |error| codex_thread_store::ThreadStoreError::InvalidRequest {
                    message: error.to_string(),
                },
            )?;
        persist_settings_snapshot(session, snapshot).await?;
    } else if !session.is_ephemeral().await {
        return Err(codex_thread_store::ThreadStoreError::Unsupported {
            operation: "persistent_thread_settings",
        });
    }
    session.update_settings(updates).await.map_err(|error| {
        codex_thread_store::ThreadStoreError::InvalidRequest {
            message: error.to_string(),
        }
    })?;
    Ok(())
}

async fn persist_settings_snapshot(
    session: &Session,
    snapshot: ThreadSettingsSnapshot,
) -> ThreadStoreResult<()> {
    let live_thread =
        session
            .live_thread()
            .ok_or(codex_thread_store::ThreadStoreError::Unsupported {
                operation: "persistent_thread_settings",
            })?;
    live_thread
        .append_items(&[RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
            ThreadSettingsAppliedEvent {
                thread_id: Some(session.thread_id),
                thread_settings: snapshot,
            },
        ))])
        .await?;
    live_thread.flush().await
}

/// Applies standalone thread settings and reports invalid overrides through the
/// normal event stream.
pub(super) async fn update(
    session: &Arc<Session>,
    submission_id: String,
    overrides: ThreadSettingsOverrides,
) {
    let updates = prepare_update(overrides);
    if let Err(error) = apply_update(session, submission_id.clone(), updates).await {
        session
            .send_event_raw(Event {
                id: submission_id,
                msg: EventMsg::Error(ErrorEvent {
                    misalignment: None,
                    message: format!("invalid thread settings override: {error}"),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
            })
            .await;
    } else {
        // Standalone settings changes supersede a pending automatic continuation.
        session.state.lock().await.last_started_turn_id = None;
    }
}

/// Converts protocol overrides into the internal settings update shape.
pub(super) fn prepare_update(overrides: ThreadSettingsOverrides) -> SessionSettingsUpdate {
    let ThreadSettingsOverrides {
        environments,
        runtime_workspace_roots,
        profile_workspace_roots,
        approval_policy,
        approvals_reviewer,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        model,
        effort,
        summary,
        service_tier,
        collaboration_mode,
        personality,
        disabled_plugin_ids,
    } = overrides;
    SessionSettingsUpdate {
        step_settings: StepSettingsUpdate {
            model,
            effort,
            collaboration_mode,
            reasoning_summary: summary,
            service_tier,
            personality,
            approval_policy,
            approvals_reviewer,
        },
        environments,
        runtime_workspace_roots,
        profile_workspace_roots,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        disabled_plugin_ids,
        ..Default::default()
    }
}

/// Acquires the shared permit before capturing or changing persistent settings.
pub(super) async fn acquire_persistence_lock(session: &Session) -> SemaphorePermit<'_> {
    session
        .thread_settings_persistence
        .acquire()
        .await
        .unwrap_or_else(|_| unreachable!("thread settings persistence semaphore is never closed"))
}

/// Applies persistent settings and emits the resulting thread-owned snapshot.
pub(super) async fn apply_update(
    session: &Session,
    submission_id: String,
    updates: SessionSettingsUpdate,
) -> ConstraintResult<()> {
    let _settings_guard = acquire_persistence_lock(session).await;
    let commit = session.update_settings(updates).await?;
    emit_applied(session, submission_id, commit.snapshot).await;
    Ok(())
}

/// Emits the snapshot published by one successful settings update.
pub(super) async fn emit_applied(
    session: &Session,
    submission_id: String,
    snapshot: ThreadSettingsSnapshot,
) {
    let msg = EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_id: Some(session.thread_id()),
        thread_settings: snapshot,
    });
    session
        .send_event_raw_without_materializing_rollout(Event {
            id: submission_id,
            msg,
        })
        .await;
}

/// Builds a current thread-owned snapshot for storage checkpoints.
pub(super) async fn applied_event(session: &Session) -> EventMsg {
    EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_id: Some(session.thread_id()),
        thread_settings: session.thread_settings_snapshot().await,
    })
}
