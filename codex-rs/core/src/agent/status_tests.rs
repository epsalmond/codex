use super::is_final;
use codex_protocol::protocol::AgentStatus;

#[test]
fn waiting_is_not_a_final_agent_status() {
    assert!(!is_final(&AgentStatus::Waiting));
}
