//! Reports rejected automatic startup separately from the retained assignment's terminal state.

use crate::TurnStartOptions;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::context::ContextualUserFragment;
use crate::context::InterAgentMessage;
use crate::context::InterAgentMessageType;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::AgentPath;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::InterAgentCommunication;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;
use tracing::warn;

const MAX_FAILURE_REASON_CHARS: usize = 256;
const MAX_FAILURE_MESSAGE_TOKENS: usize = 1_000;
const TRUNCATED_FAILURE_MESSAGE_TOKENS: usize = 900;

impl Session {
    pub(super) async fn report_failed_automatic_start(
        &self,
        turn_context: &TurnContext,
        initiator: Option<AgentPath>,
        error: &CodexErr,
    ) {
        let author = turn_context
            .session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        let parent = author
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| AgentPath::try_from(parent).ok());
        let control = self
            .services
            .local_agent_runtime
            .control(self.thread_id.into());
        let reason = error
            .to_string()
            .chars()
            .take(MAX_FAILURE_REASON_CHARS)
            .collect::<String>();
        let mut notified = Vec::new();
        // Root notification is an internal result wake, not a public followup targeting root.
        // Peer and nested-parent notifications remain queue-only, so a rejection cannot recurse.
        for recipient in [Some(AgentPath::root()), initiator, parent]
            .into_iter()
            .flatten()
        {
            if recipient == author || notified.contains(&recipient) {
                continue;
            }
            notified.push(recipient.clone());
            let target = match self
                .services
                .local_agent_runtime
                .resolve_agent_reference(
                    self.thread_id,
                    &turn_context.session_source,
                    recipient.as_str(),
                )
                .await
            {
                Ok(target) => target,
                Err(error) => {
                    warn!("failed to resolve automatic-start rejection recipient: {error}");
                    continue;
                }
            };
            // Refer to the target through the existing bounded communication attribution;
            // never echo the accepted task payload or create a coordinator terminal report.
            let content = InterAgentMessage::new(
                    InterAgentMessageType::Message,
                    recipient.clone(),
                    author.clone(),
                    format!("Automatic turn did not start: {reason}. Accepted messages remain queued; automatic wakeups are paused until an explicit follow-up or user turn. No task was dispatched or completed by this startup attempt."),
                ).render();
            // Paths are valid without a length limit. Bound the complete fragment, including
            // attribution, with room for the truncation marker just like completion notices.
            let content = if approx_token_count(&content) > MAX_FAILURE_MESSAGE_TOKENS {
                truncate_text(
                    &content,
                    TruncationPolicy::Tokens(TRUNCATED_FAILURE_MESSAGE_TOKENS),
                )
            } else {
                content
            };
            let communication = InterAgentCommunication::new(
                author.clone(),
                recipient.clone(),
                Vec::new(),
                content,
                recipient.is_root(),
            );
            if let Err(error) = control
                .send_inter_agent_communication(
                    target,
                    communication,
                    AgentCommunicationContext::new(AgentCommunicationKind::Result, self.thread_id),
                    TurnStartOptions::default(),
                )
                .await
            {
                warn!("failed to deliver automatic-start rejection: {error}");
            }
        }
    }
}
