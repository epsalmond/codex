use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

use crate::context::ContextualUserFragment;
use crate::context::InterAgentCompletionMessage;

const COMPLETION_MESSAGE_MAX_TOKENS: usize = 1_000;
const COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE: usize = 100;
const ERROR_MAX_TOKENS: usize =
    COMPLETION_MESSAGE_MAX_TOKENS - COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE;
const ERROR_NEXT_ACTION: &str = "This agent's turn failed. If you still need this agent, use the available collaboration tools to give it another task.";
const GUARDIAN_NEXT_ACTION: &str = "Tell the user this agent stopped after repeated Guardian denials. Do not resume this agent or retry its blocked work until the user explicitly confirms that it should continue.";

// Helpers for model-visible session state markers that are stored in user-role
// messages but are not user intent.

// TODO(jif) unify with structured schema
pub(crate) fn format_inter_agent_completion_message(
    task_name: AgentPath,
    sender: AgentPath,
    status: &AgentStatus,
) -> Option<String> {
    let payload = match status {
        AgentStatus::Completed(Some(message)) => {
            truncate_text(message, TruncationPolicy::Tokens(ERROR_MAX_TOKENS))
        }
        AgentStatus::Completed(None) => String::new(),
        AgentStatus::Errored(error) => {
            let error = truncate_text(error, TruncationPolicy::Tokens(ERROR_MAX_TOKENS));
            format!("Agent errored: {error}\n\n{ERROR_NEXT_ACTION}")
        }
        AgentStatus::Shutdown => "Agent shut down.".to_string(),
        AgentStatus::Interrupted => "Agent was interrupted.".to_string(),
        AgentStatus::NotFound => "Agent was not found.".to_string(),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Waiting => return None,
    };
    let message = InterAgentCompletionMessage::new(task_name, sender, payload)
        .render()
        .into_text()
        .unwrap_or_default();
    if approx_token_count(&message) > COMPLETION_MESSAGE_MAX_TOKENS {
        Some(truncate_text(
            &message,
            TruncationPolicy::Tokens(ERROR_MAX_TOKENS),
        ))
    } else {
        Some(message)
    }
}

const BLOCKED_NEXT_ACTION: &str =
    "This agent stopped and is waiting for instructions. Reply with followup_task to resume it.";

/// Renders a subagent's `report_to_parent(blocked)` for its parent.
pub(crate) fn format_inter_agent_blocked_message(
    task_name: AgentPath,
    sender: AgentPath,
    message: &str,
) -> String {
    let message = truncate_text(message, TruncationPolicy::Tokens(ERROR_MAX_TOKENS));
    InterAgentCompletionMessage::blocked(
        task_name,
        sender,
        format!("{message}\n\n{BLOCKED_NEXT_ACTION}"),
    )
    .render()
    .into_text()
    .unwrap_or_default()
}

/// Explains why a child that was interrupted for its waiting parent has no result.
pub(crate) const UNSTARTED_CHILD_NOTE: &str = "Its turn did not start, so it did no work.";
pub(crate) const CLOSED_CHILD_NOTE: &str = "It was closed before it reported a result.";

/// Renders the report of a child interrupted for its waiting parent, with a note saying why.
pub(crate) fn format_inter_agent_interruption_message(
    task_name: AgentPath,
    sender: AgentPath,
    note: &str,
) -> String {
    InterAgentCompletionMessage::new(task_name, sender, format!("Agent was interrupted. {note}"))
        .render()
        .into_text()
        .unwrap_or_default()
}

pub(crate) fn format_guardian_interruption_message(
    task_name: AgentPath,
    sender: AgentPath,
    error: &str,
) -> String {
    let error = truncate_text(error, TruncationPolicy::Tokens(ERROR_MAX_TOKENS));
    let payload = format!("Agent interrupted by Guardian: {error}\n\n{GUARDIAN_NEXT_ACTION}");
    InterAgentCompletionMessage::new(task_name, sender, payload)
        .render()
        .into_text()
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "session_prefix_tests.rs"]
mod tests;

pub(crate) fn format_subagent_context_line(
    agent_reference: &str,
    agent_nickname: Option<&str>,
) -> String {
    match agent_nickname.filter(|nickname| !nickname.is_empty()) {
        Some(agent_nickname) => format!("- {agent_reference}: {agent_nickname}"),
        None => format!("- {agent_reference}"),
    }
}
