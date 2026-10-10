//! Delivers terminal child results and completion activity to the agent tree.
//!
//! Sessions capture terminal state; the controller owns routing and queue-only delivery.
//! Delivery remains best effort, with tracing recorded only after the parent accepts it.

use super::LocalAgentControl;
use super::LocalAgentRuntime;
use super::coordinator::AgentAssignmentId;
use super::coordinator::InterruptedChild;
use super::coordinator::TerminalReportPublication;
use crate::agent::api::AgentTurnOutcome;
use crate::session_prefix::format_inter_agent_completion_message;
use crate::session_prefix::format_inter_agent_interruption_message;
use codex_protocol::AgentPath;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::WarningEvent;
use codex_rollout_trace::AgentResultTracePayload;
use codex_rollout_trace::ThreadTraceContext;
use tracing::debug;
use tracing::warn;

impl LocalAgentRuntime {
    /// Publishes the report of a child generation interrupted by `interrupt_idle_assignment` or,
    /// with a `note`, of one interrupted for its waiting parent. Returns false if the report could
    /// not be published.
    pub(crate) fn publish_interrupted_assignment(
        &self,
        assignment: &AgentAssignmentId,
        turn_id: &str,
        child_agent_path: AgentPath,
        note: Option<&str>,
    ) -> bool {
        let Some(_parent_assignment) = self.wake_coordinator.parent_assignment(assignment) else {
            return true;
        };
        let Some(parent_agent_path) = child_agent_path
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
        else {
            return false;
        };
        let message = match note {
            Some(note) => format_inter_agent_interruption_message(
                parent_agent_path.clone(),
                child_agent_path.clone(),
                note,
            ),
            None => {
                let Some(message) = format_inter_agent_completion_message(
                    parent_agent_path.clone(),
                    child_agent_path.clone(),
                    &AgentStatus::Interrupted,
                ) else {
                    return false;
                };
                message
            }
        };
        let trigger_turn = !parent_agent_path.is_root() || !self.root_waits_for_children();
        let communication = InterAgentCommunication::new(
            child_agent_path,
            parent_agent_path,
            Vec::new(),
            message,
            trigger_turn,
        );
        matches!(
            self.wake_coordinator
                .publish_terminal_report(assignment, turn_id, communication),
            Some(
                TerminalReportPublication::Published(_)
                    | TerminalReportPublication::AlreadyPublished(_)
            )
        )
    }

    /// Publishes the report of a child generation interrupted for its waiting parent, so the
    /// parent wakes through ordinary report delivery. The child's path comes from its turn
    /// context when the caller has one, and from the registry otherwise. A report that cannot be
    /// published releases the child, as before this report existed.
    pub(crate) fn publish_waiting_parent_report(
        &self,
        child: InterruptedChild,
        child_agent_path: Option<AgentPath>,
        note: &str,
    ) {
        let child_agent_path = child_agent_path.or_else(|| {
            self.registry
                .agent_metadata_for_thread(child.assignment.thread_id)
                .and_then(|metadata| metadata.agent_path)
        });
        let published = child_agent_path.is_some_and(|child_agent_path| {
            self.publish_interrupted_assignment(
                &child.assignment,
                &child.terminal_turn_id,
                child_agent_path,
                Some(note),
            )
        });
        if !published {
            warn!(
                "failed to report interrupted agent {} to its waiting parent",
                child.assignment.thread_id
            );
            self.wake_coordinator.cancel_assignment(&child.assignment);
        }
    }
}

impl LocalAgentControl {
    /// Routes a captured terminal outcome without retaining the child's live turn context.
    pub(crate) async fn notify_parent_of_terminal_turn(
        &self,
        outcome: AgentTurnOutcome,
        trace: &ThreadTraceContext,
    ) {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path: Some(child_agent_path),
            ..
        }) = &outcome.source
        else {
            return;
        };
        let parent_thread_id = *parent_thread_id;
        let status = outcome.status;
        let Some(parent_agent_path) = child_agent_path
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
        else {
            return;
        };

        if matches!(status, AgentStatus::Completed(_))
            && let Some(parent_turn_id) = outcome.parent_turn_id
        {
            let initiating_thread_id = match outcome.initiating_agent_path.as_ref() {
                Some(initiating_agent_path) if initiating_agent_path != &parent_agent_path => {
                    self.runtime.resolve_agent_reference(
                        outcome.thread_id,
                        &outcome.source,
                        initiating_agent_path.as_str(),
                    )
                    .await
                    .inspect_err(|err| {
                        debug!(
                            "failed to resolve completed activity initiator {initiating_agent_path}: {err}"
                        );
                    })
                    .ok()
                }
                _ => Some(parent_thread_id),
            };
            if let Some(initiating_thread_id) = initiating_thread_id
                && let Err(err) = self
                    .emit_sub_agent_activity(
                        initiating_thread_id,
                        parent_turn_id,
                        SubAgentActivityItem {
                            id: format!("subagent-completed-{}", outcome.turn_id),
                            kind: SubAgentActivityKind::Completed,
                            agent_thread_id: outcome.thread_id,
                            agent_path: child_agent_path.clone(),
                        },
                    )
                    .await
            {
                debug!(
                    "failed to emit completed activity to initiating thread {initiating_thread_id}: {err}"
                );
            }
        }

        let Some(message) = format_inter_agent_completion_message(
            parent_agent_path.clone(),
            child_agent_path.clone(),
            &status,
        ) else {
            return;
        };
        if self.runtime.wake_mode_enabled() {
            let Some(assignment) = self
                .runtime
                .terminal_wake_assignment_for_turn(outcome.thread_id, &outcome.turn_id)
            else {
                debug!(
                    "terminal turn {} has no current wake assignment",
                    outcome.turn_id
                );
                return;
            };
            let trace_message = trace.is_enabled().then(|| message.clone());
            let trigger_turn =
                !parent_agent_path.is_root() || !self.runtime.root_waits_for_children();
            let communication = InterAgentCommunication::new(
                child_agent_path.clone(),
                parent_agent_path.clone(),
                Vec::new(),
                message,
                trigger_turn,
            );
            let publication = self.runtime.wake_coordinator.publish_terminal_report(
                &assignment,
                &outcome.turn_id,
                communication,
            );
            match publication {
                Some(TerminalReportPublication::Published(report_id)) => {
                    debug!("published terminal report {report_id}");
                }
                Some(TerminalReportPublication::AlreadyPublished(report_id)) => {
                    debug!("terminal report {report_id} was already published");
                }
                None => {
                    warn!(
                        "failed to publish terminal result for agent {} turn {}",
                        outcome.thread_id, outcome.turn_id
                    );
                    return;
                }
            }
            if let Some(message) = trace_message {
                trace.record_agent_result_interaction(
                    outcome.turn_id.as_str(),
                    parent_thread_id,
                    &AgentResultTracePayload {
                        child_agent_path: child_agent_path.as_str(),
                        message: &message,
                        status: &status,
                    },
                );
            }
            return;
        }
        // `communication` owns the message. Keep a second copy only when the
        // recorder will actually need it after parent delivery succeeds.
        let trace_message = trace.is_enabled().then(|| message.clone());
        // Mark completions to the root as wakes; the root decides on receipt whether it is in
        // wake mode. The root is exempt from the capacity check a triggered send adds.
        // Non-root parents keep `wait_agent`.
        let trigger_turn = parent_agent_path.is_root();
        let mut communication = InterAgentCommunication::new(
            child_agent_path.clone(),
            parent_agent_path,
            Vec::new(),
            message,
            trigger_turn,
        );
        communication.id = Some(codex_protocol::ResponseItemId::new("msg"));
        if let Err(err) = self
            .deliver_polling_completion(parent_thread_id, outcome.thread_id, communication)
            .await
        {
            warn!("failed to notify parent thread {parent_thread_id}: {err}");
            if let Ok(state) = self.runtime.upgrade()
                && let Ok(sender) = state.get_thread(outcome.thread_id).await
            {
                let diagnostic: String = err.to_string().chars().take(512).collect();
                sender.session.send_event_raw(Event {
                    id: outcome.turn_id.clone(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: format!("Could not deliver completion to parent {parent_thread_id}: {diagnostic}"),
                    }),
                }).await;
            }
            return;
        }
        if let Some(message) = trace_message {
            trace.record_agent_result_interaction(
                outcome.turn_id.as_str(),
                parent_thread_id,
                &AgentResultTracePayload {
                    child_agent_path: child_agent_path.as_str(),
                    message: &message,
                    status: &status,
                },
            );
        }
    }
}
