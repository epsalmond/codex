use super::is_final_agent_status;
use codex_protocol::protocol::AgentStatus;

#[test]
fn waiting_agent_is_not_final_for_memory_phase_two() {
    assert!(!is_final_agent_status(&AgentStatus::Waiting));
}
