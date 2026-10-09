//! Excludes polling completion delivery during intentional archive or close.

use super::*;

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
            || self.runtime.registry.completion_delivery_closed(parent_thread_id)
        {
            return Err(CodexErr::InvalidRequest("completion target is closing or archiving".into()));
        }
        let context = AgentCommunicationContext::new(AgentCommunicationKind::Result, sender_thread_id);
        self.send_inter_agent_communication(
            parent_thread_id, communication, context, TurnStartOptions::default(),
        ).await.map(|_| ())
    }
}
