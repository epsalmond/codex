//! Event-driven delivery of terminal reports to their owning agent assignment.

use super::LocalAgentControl;
use super::coordinator::AgentAssignmentId;
use super::coordinator::WakeDispatchResult;
use crate::TurnStartOptions;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::tasks::PendingWorkStartResult;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_thread_store::ReadThreadParams;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::watch;
use tracing::warn;

/// Keeps one dispatcher alive for as long as the shared runtime is retained.
pub(super) struct WakeDispatcherLifetime {
    started: AtomicBool,
    _shutdown_tx: watch::Sender<()>,
    shutdown_rx: std::sync::Mutex<Option<watch::Receiver<()>>>,
}

impl WakeDispatcherLifetime {
    pub(super) fn new() -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        Self {
            started: AtomicBool::new(false),
            _shutdown_tx: shutdown_tx,
            shutdown_rx: std::sync::Mutex::new(Some(shutdown_rx)),
        }
    }

    fn start(&self) -> Option<watch::Receiver<()>> {
        self.started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        self.shutdown_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

impl Default for WakeDispatcherLifetime {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalAgentControl {
    pub(crate) fn start_wake_dispatcher(&self, root_waits_for_children: bool) {
        self.runtime.enable_wake_mode();
        self.runtime
            .set_root_waits_for_children(root_waits_for_children);
        let Some(shutdown) = self
            .runtime
            .wake_dispatcher
            .as_ref()
            .and_then(|dispatcher| dispatcher.start())
        else {
            return;
        };
        let dispatcher_control = LocalAgentControl {
            session_id: self.session_id,
            runtime: self.runtime.without_wake_dispatcher(),
        };
        let coordinator = Arc::clone(&dispatcher_control.runtime.wake_coordinator);
        tokio::spawn(async move {
            let dispatch_control = dispatcher_control;
            Arc::clone(&coordinator)
                .run_wake_dispatcher(shutdown, move |assignment| {
                    let control = dispatch_control.clone();
                    async move { control.deliver_pending_reports(&assignment).await }
                })
                .await;
        });
    }

    async fn deliver_pending_reports(&self, assignment: &AgentAssignmentId) -> WakeDispatchResult {
        if !self
            .runtime
            .wake_coordinator
            .has_pending_reports(assignment)
        {
            return WakeDispatchResult::Complete;
        }
        if let Err(err) = self.ensure_coordinator_target_loaded(assignment).await {
            warn!(
                "failed to load agent {} for its terminal report: {err}",
                assignment.thread_id
            );
            return WakeDispatchResult::Defer;
        }

        loop {
            let Some(report) = self.runtime.claim_next_report_for_delivery(assignment) else {
                break;
            };
            let report_id = report.id.clone();
            let result = self
                .enqueue_terminal_report(
                    assignment.thread_id,
                    report.communication.clone(),
                    AgentCommunicationContext::new(
                        AgentCommunicationKind::Result,
                        report.sender_thread_id,
                    ),
                    TurnStartOptions::default(),
                )
                .await;
            match result {
                Ok(_) => {
                    if report.mark_enqueued() {
                        continue;
                    }
                    warn!(
                        "terminal report {} lost its mailbox claim before enqueue",
                        report_id
                    );
                    return WakeDispatchResult::Complete;
                }
                Err(err) => {
                    warn!(
                        "failed to deliver terminal report {} to agent {}: {err}",
                        report_id, assignment.thread_id
                    );
                    return WakeDispatchResult::Defer;
                }
            }
        }

        let manager = match self.runtime.manager.upgrade() {
            Some(manager) => manager,
            None => return WakeDispatchResult::Defer,
        };
        let target = match manager.get_thread(assignment.thread_id).await {
            Ok(target) => target,
            Err(err) => {
                warn!("failed to get report wake target after delivery: {err}");
                return WakeDispatchResult::Defer;
            }
        };
        let trigger_mail_pending = target
            .session
            .input_queue
            .has_trigger_turn_mailbox_items()
            .await;
        if trigger_mail_pending && target.session.active_turn.lock().await.is_some() {
            return WakeDispatchResult::Defer;
        }
        if trigger_mail_pending {
            let config = target.session.get_config().await;
            let version = target
                .session
                .multi_agent_version()
                .unwrap_or_else(|| config.multi_agent_version_from_features());
            if self
                .ensure_execution_capacity(version, &target.session_source)
                .is_err()
            {
                return WakeDispatchResult::Defer;
            }
            let start_result = target.session.maybe_start_turn_for_pending_work().await;
            let trigger_mail_pending = target
                .session
                .input_queue
                .has_trigger_turn_mailbox_items()
                .await;
            let wakeups_paused = target.session.input_queue.wakeups_paused();
            if classify_pending_wake_after_start(
                start_result,
                PendingWakeState {
                    trigger_mail_pending,
                    wakeups_paused,
                },
            ) == WakeDispatchResult::Defer
            {
                return WakeDispatchResult::Defer;
            }
        }

        if self.runtime.has_recorded_wake_reports(assignment) {
            let state = match self.runtime.upgrade() {
                Ok(state) => state,
                Err(err) => {
                    warn!("failed to load recorded-report wake target: {err}");
                    return WakeDispatchResult::Defer;
                }
            };
            let target = match state.get_thread(assignment.thread_id).await {
                Ok(target) => target,
                Err(err) => {
                    warn!("failed to load recorded-report wake target: {err}");
                    return WakeDispatchResult::Defer;
                }
            };
            return match target
                .session
                .start_turn_for_recorded_reports(assignment)
                .await
            {
                crate::tasks::RecordedReportWakeResult::Started
                | crate::tasks::RecordedReportWakeResult::NoLongerNeeded => {
                    WakeDispatchResult::Complete
                }
                crate::tasks::RecordedReportWakeResult::Busy => WakeDispatchResult::Defer,
            };
        }

        WakeDispatchResult::Complete
    }

    pub(crate) async fn ensure_coordinator_target_loaded(
        &self,
        assignment: &AgentAssignmentId,
    ) -> CodexResult<()> {
        let operation = self
            .runtime
            .wake_coordinator
            .begin_generation_operation(assignment)
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
        tokio::select! {
            biased;
            _ = operation.cancelled() => Err(CodexErr::TurnAborted),
            result = self.load_coordinator_target(assignment) => result,
        }
    }

    async fn load_coordinator_target(&self, assignment: &AgentAssignmentId) -> CodexResult<()> {
        let state = self.runtime.upgrade()?;
        let root_thread_id = ThreadId::from(self.session_id);
        if assignment.thread_id == root_thread_id
            && self
                .runtime
                .wake_coordinator
                .is_current_open_assignment(assignment)
        {
            let root = state.get_thread(root_thread_id).await?;
            if root.multi_agent_version() != Some(MultiAgentVersion::V2)
                || !self
                    .runtime
                    .registry
                    .agent_metadata_for_thread(root_thread_id)
                    .is_some_and(|metadata| {
                        metadata.agent_id == Some(root_thread_id)
                            && metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
                    })
            {
                return Err(CodexErr::InvalidRequest(
                    "wake target is not the registered root thread".to_string(),
                ));
            }
            return Ok(());
        }
        let authority = self
            .runtime
            .wake_coordinator
            .reload_authority(assignment)
            .ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "wake target is no longer owned by a current assignment".to_string(),
                )
            })?;
        if SessionId::from(authority.root_thread_id) != self.session_id {
            return Err(CodexErr::InvalidRequest(
                "wake target belongs to a different root runtime".to_string(),
            ));
        }
        let metadata = self
            .runtime
            .registry
            .agent_metadata_for_thread(assignment.thread_id)
            .filter(|metadata| metadata.agent_id == Some(assignment.thread_id))
            .ok_or(CodexErr::ThreadNotFound(assignment.thread_id))?;
        let agent_path = metadata.agent_path.ok_or_else(|| {
            CodexErr::InvalidRequest("wake target has no registered agent path".to_string())
        })?;

        if let Ok(thread) = state.get_thread(assignment.thread_id).await {
            return validate_coordinator_reload_target(
                &thread.session_source,
                thread.multi_agent_version(),
                authority.parent_thread_id,
                &agent_path,
            );
        }

        let stored_thread = state
            .read_stored_thread(ReadThreadParams {
                thread_id: assignment.thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        validate_coordinator_reload_target(
            &stored_thread.source,
            None,
            authority.parent_thread_id,
            &agent_path,
        )?;
        if stored_thread
            .parent_thread_id
            .is_some_and(|parent_thread_id| parent_thread_id != authority.parent_thread_id)
            || stored_thread
                .agent_path
                .as_deref()
                .is_some_and(|stored_agent_path| stored_agent_path != agent_path.as_str())
        {
            return Err(CodexErr::InvalidRequest(
                "stored wake target identity does not match its assignment".to_string(),
            ));
        }
        if let Some(graph_store) = state.agent_graph_store() {
            let children = graph_store
                .list_thread_spawn_children(
                    authority.parent_thread_id,
                    Some(ThreadSpawnEdgeStatus::Open),
                )
                .await
                .map_err(|err| CodexErr::InvalidRequest(err.to_string()))?;
            if !children.contains(&assignment.thread_id) {
                return Err(CodexErr::InvalidRequest(
                    "stored wake target has no open parent edge".to_string(),
                ));
            }
        }

        let config = self
            .runtime
            .wake_coordinator
            .reload_config(assignment)
            .ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "wake target has no root-owned reload configuration".to_string(),
                )
            })?;
        self.ensure_v2_agent_loaded_for_wake(
            config,
            assignment.thread_id,
            /*parent*/ None,
            Some(assignment),
        )
        .await?;
        if !self
            .runtime
            .wake_coordinator
            .is_current_open_assignment(assignment)
        {
            return Err(CodexErr::InvalidRequest(
                "wake assignment was cancelled during reload".to_owned(),
            ));
        }
        let thread = state.get_thread(assignment.thread_id).await?;
        validate_coordinator_reload_target(
            &thread.session_source,
            thread.multi_agent_version(),
            authority.parent_thread_id,
            &agent_path,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingWakeState {
    trigger_mail_pending: bool,
    wakeups_paused: bool,
}

fn classify_pending_wake_after_start(
    start_result: PendingWorkStartResult,
    state: PendingWakeState,
) -> WakeDispatchResult {
    if start_result == PendingWorkStartResult::Deferred
        || (state.trigger_mail_pending && !state.wakeups_paused)
    {
        WakeDispatchResult::Defer
    } else {
        WakeDispatchResult::Complete
    }
}

fn validate_coordinator_reload_target(
    source: &SessionSource,
    multi_agent_version: Option<MultiAgentVersion>,
    parent_thread_id: ThreadId,
    agent_path: &AgentPath,
) -> CodexResult<()> {
    if multi_agent_version.is_some_and(|version| version != MultiAgentVersion::V2)
        || !matches!(
            source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: recorded_parent,
                agent_path: Some(recorded_path),
                ..
            }) if *recorded_parent == parent_thread_id && recorded_path == agent_path
        )
    {
        return Err(CodexErr::InvalidRequest(
            "wake target source does not match its registered parent and path".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "dispatcher_tests.rs"]
mod tests;
