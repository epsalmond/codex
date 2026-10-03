use super::*;
use pretty_assertions::assert_eq;

#[test]
fn subagent_context_limit_message_names_the_parents_messaging_tool() {
    assert_eq!(
        [
            MultiAgentVersion::V2,
            MultiAgentVersion::V1,
            MultiAgentVersion::Disabled,
        ]
        .map(|version| subagent_context_limit_message(Some(244_800), version)),
        [
            "This subagent's turn ended because its context was still over its 244800-token context limit after compaction. Use `followup_task` to ask it for a brief report of its partial results, then delegate the remaining work as smaller tasks.",
            "This subagent's turn ended because its context was still over its 244800-token context limit after compaction. Use `send_input` to ask it for a brief report of its partial results, then delegate the remaining work as smaller tasks.",
            "This subagent's turn ended because its context was still over its 244800-token context limit after compaction. Ask it for a brief report of its partial results, then delegate the remaining work as smaller tasks.",
        ]
        .map(str::to_string),
    );
}
