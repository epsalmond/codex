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
use std::sync::Arc;
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
    generation: u64,
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

    fn take(self: &Arc<Self>, ancestor: Option<&AgentPath>) -> Option<AnswerCreditReservation> {
        let mut queue = self.lock();
        let id = match ancestor {
            Some(ancestor) => queue
                .credits
                .iter()
                .find_map(|(&id, credit)| (credit == ancestor).then_some(id))?,
            None => *queue.credits.first_key_value()?.0,
        };
        let (_, ancestor) = queue.credits.remove_entry(&id)?;
        Some(AnswerCreditReservation {
            credits: Arc::clone(self),
            generation: queue.generation,
            id,
            ancestor,
            delivered: false,
        })
    }

    fn clear(&self) {
        let mut queue = self.lock();
        queue.generation += 1;
        queue.credits.clear();
    }
}

/// Returns an undelivered credit on errors or cancellation, unless its turn has ended.
pub(crate) struct AnswerCreditReservation {
    credits: Arc<AnswerCredits>,
    generation: u64,
    id: u64,
    ancestor: AgentPath,
    delivered: bool,
}

impl AnswerCreditReservation {
    pub(crate) fn ancestor(&self) -> &AgentPath {
        &self.ancestor
    }

    pub(crate) fn commit(mut self) {
        self.delivered = true;
    }
}

impl Drop for AnswerCreditReservation {
    fn drop(&mut self) {
        if !self.delivered {
            let mut queue = self.credits.lock();
            if queue.generation == self.generation {
                queue.credits.insert(self.id, self.ancestor.clone());
            }
        }
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

    /// Reserves a credit for the selected ancestor, or the oldest credit when omitted.
    pub(crate) fn take_answer_credit(
        &self,
        ancestor: Option<&AgentPath>,
    ) -> Option<AnswerCreditReservation> {
        self.services
            .thread_extension_data
            .get::<AnswerCredits>()?
            .take(ancestor)
    }

    /// Questions end with the turn: its final or blocked report covers them.
    pub(crate) fn clear_answer_credits(&self) {
        if let Some(credits) = self.services.thread_extension_data.get::<AnswerCredits>() {
            credits.clear();
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
    use std::future::Future;

    fn spend(
        credits: &Arc<AnswerCredits>,
        ancestor: Option<&AgentPath>,
    ) -> Option<(u64, AgentPath)> {
        let credit = credits.take(ancestor)?;
        let selected = (credit.id, credit.ancestor.clone());
        credit.commit();
        Some(selected)
    }

    #[test]
    fn answer_credits_select_and_restore_without_reordering_other_ancestors() {
        let root = AgentPath::try_from("/root").expect("root");
        let parent = AgentPath::try_from("/root/lead").expect("parent");
        let credits = Arc::new(AnswerCredits::default());
        for ancestor in [root.clone(), parent.clone(), parent.clone()] {
            credits.grant(ancestor);
        }

        let selected = credits.take(Some(&parent)).expect("parent credit");
        assert_eq!(selected.id, 1);
        drop(selected);
        assert_eq!(spend(&credits, None), Some((0, root)));
        assert_eq!(spend(&credits, Some(&parent)), Some((1, parent.clone())));
        assert_eq!(spend(&credits, Some(&parent)), Some((2, parent.clone())));
        assert!(credits.take(Some(&parent)).is_none());
        assert!(credits.take(None).is_none());
    }

    #[test]
    fn answer_credits_restore_keeps_fifo_after_another_answer_completes() {
        let root = AgentPath::try_from("/root").expect("root");
        let parent = AgentPath::try_from("/root/lead").expect("parent");
        let credits = Arc::new(AnswerCredits::default());
        for ancestor in [root.clone(), parent.clone(), root.clone()] {
            credits.grant(ancestor);
        }

        let selected = credits.take(Some(&parent)).expect("parent credit");
        assert_eq!(spend(&credits, None), Some((0, root.clone())));
        credits.grant(parent.clone());
        drop(selected);
        assert_eq!(spend(&credits, None), Some((1, parent.clone())));
        assert_eq!(spend(&credits, None), Some((2, root)));
        assert_eq!(spend(&credits, None), Some((3, parent)));
    }

    #[test]
    fn answer_credits_missing_target_preserves_fifo() {
        let root = AgentPath::try_from("/root").expect("root");
        let parent = AgentPath::try_from("/root/lead").expect("parent");
        let credits = Arc::new(AnswerCredits::default());
        credits.grant(root.clone());

        assert!(credits.take(Some(&parent)).is_none());
        assert_eq!(spend(&credits, None), Some((0, root)));
    }

    #[test]
    fn answer_credits_dropped_delivery_cannot_restore_after_turn_cleanup() {
        let root = AgentPath::try_from("/root").expect("root");
        let parent = AgentPath::try_from("/root/lead").expect("parent");
        let credits = Arc::new(AnswerCredits::default());
        credits.grant(root.clone());
        let previous_turn = credits.take(None).expect("old credit");

        credits.clear();
        credits.grant(parent.clone());
        drop(previous_turn);
        assert_eq!(spend(&credits, None), Some((1, parent)));
        assert!(credits.take(Some(&root)).is_none());
    }

    #[test]
    fn answer_credits_cancelled_delivery_restores_selected_credit() {
        let root = AgentPath::try_from("/root").expect("root");
        let parent = AgentPath::try_from("/root/lead").expect("parent");
        let credits = Arc::new(AnswerCredits::default());
        credits.grant(root.clone());
        credits.grant(parent.clone());
        let mut delivery = Box::pin(async {
            let credit = credits.take(Some(&parent)).expect("selected credit");
            std::future::pending::<()>().await;
            credit.commit();
        });
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(delivery.as_mut().poll(&mut context).is_pending());
        assert!(credits.take(Some(&parent)).is_none());
        drop(delivery);

        assert_eq!(spend(&credits, None), Some((0, root)));
        assert_eq!(spend(&credits, None), Some((1, parent)));
        assert!(credits.take(None).is_none());
    }
}
