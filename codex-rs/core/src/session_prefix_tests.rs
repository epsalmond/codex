use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::approx_token_count;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
use super::format_inter_agent_completion_message;

#[test]
fn error_completion_message_stays_below_manual_review_threshold() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored("stream disconnected ".repeat(1_000)),
    )
    .expect("error status should produce a completion message");

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(ERROR_NEXT_ACTION));
}

#[test]
fn completed_message_bounds_long_unicode_payload_with_its_envelope() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Completed(Some(format!(
            "worker result marker {} final result marker",
            "🧪résultat ".repeat(10_000)
        ))),
    )
    .expect("completed status should produce a completion message");

    assert!(approx_token_count(&message) <= COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains("worker result marker"));
    assert!(message.contains("final result marker"));
    assert!(message.contains("tokens truncated"));
}
