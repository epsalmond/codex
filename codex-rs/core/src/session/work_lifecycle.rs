use super::Session;
use super::TurnContext;
use crate::agent::control::WorkAdmissionError;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;

impl Session {
    pub(crate) fn register_root_turn_lifecycle(
        &self,
        turn_context: &TurnContext,
    ) -> Result<(), WorkAdmissionError> {
        self.register_root_turn_lifecycle_id(&turn_context.sub_id, &turn_context.session_source)
    }

    pub(crate) fn register_root_turn_lifecycle_id(
        &self,
        turn_id: &str,
        session_source: &SessionSource,
    ) -> Result<(), WorkAdmissionError> {
        if session_source.is_non_root_agent() {
            return Ok(());
        }
        self.services.local_agent_runtime.root_turn_started(turn_id)
    }

    pub(crate) async fn register_root_turn_lifecycle_for_session(
        &self,
        turn_id: &str,
    ) -> Result<(), WorkAdmissionError> {
        let session_source = {
            self.state
                .lock()
                .await
                .session_configuration
                .session_source
                .clone()
        };
        self.register_root_turn_lifecycle_id(turn_id, &session_source)
    }

    /// Releases an admitted root turn that failed before `TurnStarted`, which would otherwise
    /// hold the drain open with no terminal event to clear it.
    pub(crate) fn abandon_root_turn_lifecycle(&self, turn_id: &str) {
        self.services
            .local_agent_runtime
            .root_turn_abandoned(turn_id);
    }

    pub(crate) async fn admit_root_task_or_emit_error(
        &self,
        turn_context: &TurnContext,
    ) -> Result<(), WorkAdmissionError> {
        if let Err(error) = self.register_root_turn_lifecycle(turn_context) {
            tracing::warn!(%error, "root task admission rejected by lifecycle coordinator");
            self.send_event(
                turn_context,
                EventMsg::Error(ErrorEvent {
                    misalignment: None,
                    message: "The execution scope is shutting down".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            )
            .await;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn update_root_turn_lifecycle(
        &self,
        turn_context: &TurnContext,
        event: &EventMsg,
    ) -> Result<(), WorkAdmissionError> {
        if turn_context.session_source.is_non_root_agent() {
            return Ok(());
        }
        match event {
            EventMsg::TurnStarted(TurnStartedEvent { .. }) => {
                self.register_root_turn_lifecycle(turn_context)
            }
            EventMsg::TurnComplete(TurnCompleteEvent { .. })
            | EventMsg::TurnAborted(TurnAbortedEvent { .. }) => {
                self.services
                    .local_agent_runtime
                    .root_turn_terminal(&turn_context.sub_id);
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
