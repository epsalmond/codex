use super::super::TurnInput;
use super::deferred_coordinator_report;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::protocol::InterAgentCommunication;
use pretty_assertions::assert_eq;

#[test]
fn deferred_report_preserves_its_wake_request_until_history_is_recorded() {
    let communication = InterAgentCommunication {
        id: Some(ResponseItemId::new("amsg_test-report")),
        trigger_turn: true,
        ..InterAgentCommunication::new(
            AgentPath::try_from("/root/worker").expect("worker path"),
            AgentPath::root(),
            Vec::new(),
            "worker result".to_string(),
            /*trigger_turn*/ true,
        )
    };
    let input = TurnInput::InterAgentCommunication(communication.clone());

    assert_eq!(
        deferred_coordinator_report(&input),
        Some(TurnInput::InterAgentCommunication(communication))
    );
}
