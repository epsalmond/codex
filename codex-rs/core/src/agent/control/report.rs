//! Delivers typed subagent reports that are not terminal results.
//!
//! A `report(answer)` replies to an ancestor's question. It reaches a running ancestor at its next
//! boundary without a new turn, and starts a turn in an idle one so an ancestor that asked and
//! went idle waiting is woken. The root follows its child-report wake mode instead.

use super::LocalAgentControl;
use crate::agent::api::DeliveryReceipt;
use crate::agent::types::AgentMessage;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::config::Config;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::turn_input::TurnStartOptions;

impl LocalAgentControl {
    pub(crate) async fn send_report_answer(
        &self,
        caller: ThreadId,
        ancestor: ThreadId,
        resume_config: Config,
        message: AgentMessage,
        start_options: TurnStartOptions,
    ) -> CodexResult<DeliveryReceipt> {
        self.runtime.ensure_agent_not_closed(ancestor)?;
        let receiver = self.runtime.ensure_agent_known(ancestor)?;
        let author = self
            .runtime
            .ensure_agent_known(caller)?
            .agent_path
            .unwrap_or_else(AgentPath::root);
        let receiver_path = receiver.agent_path.clone().ok_or_else(|| {
            CodexErr::UnsupportedOperation("target agent is missing an agent_path".to_string())
        })?;
        if !receiver_path.is_ancestor_of(&author) {
            return Err(CodexErr::UnsupportedOperation(format!(
                "`{receiver_path}` is not an ancestor of `{author}`; answers go only to the ancestor that asked."
            )));
        }
        let trigger_turn = !receiver_path.is_root() || !self.runtime.root_waits_for_children();
        let _residency_pin = if trigger_turn
            || (self.runtime.upgrade()?.get_thread(ancestor).await.is_err()
                && self
                    .runtime
                    .registry
                    .evicted_environments(ancestor)
                    .is_none())
        {
            self.ensure_v2_agent_loaded(resume_config, ancestor, /*parent*/ None)
                .await?
        } else {
            None
        };
        let communication = message.into_answer_communication(author, receiver_path, trigger_turn);
        let context = AgentCommunicationContext::new(AgentCommunicationKind::Message, caller);
        let submission_id = match self
            .send_inter_agent_communication(
                ancestor,
                communication.clone(),
                context.clone(),
                start_options.clone(),
            )
            .await
        {
            Ok(submission_id) => submission_id,
            // An idle ancestor without execution capacity still gets the answer at its next turn.
            Err(err) if communication.trigger_turn => {
                tracing::debug!("answer could not start a turn in {ancestor}: {err}; queueing it");
                let mut communication = communication;
                communication.trigger_turn = false;
                self.send_inter_agent_communication(ancestor, communication, context, start_options)
                    .await?
            }
            Err(err) => return Err(err),
        };
        Ok(DeliveryReceipt {
            thread_id: ancestor,
            metadata: receiver,
            submission_id,
        })
    }
}
