//! Turn-side state for MultiAgentV2 typed reports.
//!
//! A subagent reports to its ancestors only through `report_to_parent`. `blocked` and `final` end the turn
//! and travel as the turn's terminal report; `answer` replies to an ancestor's question and spends
//! one answer credit, granted per ancestor message delivered to the agent. The turn also counts
//! model calls so a long-running subagent is told once to wrap up.

use super::session::Session;
use super::turn_context::TurnContext;
use crate::context::SubagentWrapUpReminder;
use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Terminal report kinds; `answer` never ends a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalReportKind {
    Blocked,
    Final,
}

/// A `report_to_parent(blocked|final)` recorded during the current turn. The turn ends after the step that
/// recorded it, and its message becomes the turn's last agent message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingTerminalReport {
    pub(crate) kind: TerminalReportKind,
    pub(crate) message: String,
}

/// Set when this turn's final report reached the parent and the agent's lifetime is `task`.
pub(crate) struct CloseAfterFinalReport;

/// Ancestors whose messages this agent has not yet answered, oldest first.
#[derive(Default)]
pub(crate) struct AnswerCredits(Mutex<AnswerCreditQueue>);

#[derive(Default)]
struct AnswerCreditQueue {
    next_id: u64,
    credits: BTreeMap<u64, AgentPath>,
}

impl AnswerCredits {
    fn lock(&self) -> std::sync::MutexGuard<'_, AnswerCreditQueue> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn grant(&self, ancestor: AgentPath) {
        let mut queue = self.lock();
        let id = queue.next_id;
        queue.next_id += 1;
        queue.credits.insert(id, ancestor);
    }

    fn take(&self, ancestor: Option<&AgentPath>) -> Option<(u64, AgentPath)> {
        let mut queue = self.lock();
        let id = match ancestor {
            Some(ancestor) => queue
                .credits
                .iter()
                .find_map(|(&id, credit)| (credit == ancestor).then_some(id))?,
            None => *queue.credits.first_key_value()?.0,
        };
        queue.credits.remove_entry(&id)
    }

    fn restore(&self, (id, ancestor): (u64, AgentPath)) {
        self.lock().credits.insert(id, ancestor);
    }
}

/// Returns the agent path of a non-root V2 agent that reports through `report_to_parent`. Hosts that hide
/// the native tools keep the earlier reporting behavior.
pub(crate) fn typed_report_agent_path(turn_context: &TurnContext) -> Option<AgentPath> {
    let config = &turn_context.config.multi_agent_v2;
    if turn_context.multi_agent_version != MultiAgentVersion::V2
        || !config.typed_reports
        || config.disable_native_tools
    {
        return None;
    }
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        agent_path: Some(agent_path),
        ..
    }) = &turn_context.session_source
    else {
        return None;
    };
    (!agent_path.is_root()).then(|| agent_path.clone())
}

impl Session {
    /// Grants one answer credit for a queued message an ancestor sent this agent. Follow-up tasks
    /// and spawn messages are assignments, answered by `report_to_parent(final)`.
    pub(crate) fn grant_answer_credit(
        &self,
        turn_context: &TurnContext,
        communication: &InterAgentCommunication,
    ) {
        let Some(agent_path) = typed_report_agent_path(turn_context) else {
            return;
        };
        if communication.trigger_turn
            || communication.recipient != agent_path
            || !communication.author.is_ancestor_of(&agent_path)
        {
            return;
        }
        self.services
            .thread_extension_data
            .get_or_init(AnswerCredits::default)
            .grant(communication.author.clone());
    }

    /// Spends one credit for the selected ancestor, or the oldest credit when omitted.
    pub(crate) fn take_answer_credit(
        &self,
        ancestor: Option<&AgentPath>,
    ) -> Option<(u64, AgentPath)> {
        self.services
            .thread_extension_data
            .get::<AnswerCredits>()?
            .take(ancestor)
    }

    /// Restores the selected credit in FIFO order, even if another answer completed meanwhile.
    pub(crate) fn restore_answer_credit(&self, credit: (u64, AgentPath)) {
        self.services
            .thread_extension_data
            .get_or_init(AnswerCredits::default)
            .restore(credit);
    }

    /// Questions end with the turn: its final or blocked report covers them.
    pub(crate) fn clear_answer_credits(&self) {
        if let Some(credits) = self.services.thread_extension_data.get::<AnswerCredits>() {
            credits.lock().credits.clear();
        }
    }
}

/// Records the wrap-up reminder before the next model call once a subagent turn has made
/// `wrap_up_after_model_calls` calls. Returns true when it recorded the reminder.
pub(super) async fn maybe_record_wrap_up_reminder(
    sess: &Session,
    turn_context: &TurnContext,
    model_calls: u32,
) -> bool {
    let threshold = turn_context.config.multi_agent_v2.wrap_up_after_model_calls;
    if threshold == 0 || model_calls != threshold || typed_report_agent_path(turn_context).is_none()
    {
        return false;
    }
    let item = crate::context::ContextualUserFragment::into(SubagentWrapUpReminder { model_calls });
    sess.record_conversation_items(
        turn_context,
        turn_context.model_info(),
        std::slice::from_ref(&item),
    )
    .await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn answer_credits_select_and_restore_without_reordering_other_ancestors() {
        let root = AgentPath::try_from("/root").expect("root");
        let parent = AgentPath::try_from("/root/lead").expect("parent");
        let credits = AnswerCredits::default();
        for ancestor in [root.clone(), parent.clone(), parent.clone()] {
            credits.grant(ancestor);
        }

        let selected = credits.take(Some(&parent)).expect("parent credit");
        assert_eq!(selected, (1, parent.clone()));
        credits.restore(selected);
        assert_eq!(credits.take(None), Some((0, root)));
        assert_eq!(credits.take(Some(&parent)), Some((1, parent.clone())));
        assert_eq!(credits.take(Some(&parent)), Some((2, parent.clone())));
        assert_eq!(credits.take(Some(&parent)), None);
        assert_eq!(credits.take(None), None);
    }

    #[test]
    fn answer_credits_restore_keeps_fifo_after_another_answer_completes() {
        let root = AgentPath::try_from("/root").expect("root");
        let parent = AgentPath::try_from("/root/lead").expect("parent");
        let credits = AnswerCredits::default();
        for ancestor in [root.clone(), parent.clone(), root.clone()] {
            credits.grant(ancestor);
        }

        let selected = credits.take(Some(&parent)).expect("parent credit");
        assert_eq!(credits.take(None), Some((0, root.clone())));
        credits.grant(parent.clone());
        credits.restore(selected);
        assert_eq!(credits.take(None), Some((1, parent.clone())));
        assert_eq!(credits.take(None), Some((2, root)));
        assert_eq!(credits.take(None), Some((3, parent)));
    }

    #[test]
    fn answer_credits_missing_target_preserves_fifo() {
        let root = AgentPath::try_from("/root").expect("root");
        let parent = AgentPath::try_from("/root/lead").expect("parent");
        let credits = AnswerCredits::default();
        credits.grant(root.clone());

        assert_eq!(credits.take(Some(&parent)), None);
        assert_eq!(credits.take(None), Some((0, root)));
    }
}
