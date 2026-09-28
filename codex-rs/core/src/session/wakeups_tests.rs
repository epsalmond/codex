use super::super::tests::make_session_and_context_with_session_source;
use codex_features::AgentPolling;
use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::OnceLock;

/// A MultiAgentV2 root keeps a child's wake only in wake mode, and an unresolved version strips
/// it. Outside MultiAgentV2 the wake is left alone.
#[tokio::test]
async fn child_report_wake_follows_root_version_and_mode() {
    let mut kept = Vec::new();
    for (version, agent_polling, session_source) in [
        (None, AgentPolling::Disabled, SessionSource::Cli),
        (
            Some(MultiAgentVersion::V2),
            AgentPolling::Enabled,
            SessionSource::Cli,
        ),
        (
            Some(MultiAgentVersion::V2),
            AgentPolling::Disabled,
            SessionSource::Cli,
        ),
        (
            Some(MultiAgentVersion::V2),
            AgentPolling::Disabled,
            SessionSource::Exec,
        ),
        (
            Some(MultiAgentVersion::V1),
            AgentPolling::Enabled,
            SessionSource::Cli,
        ),
    ] {
        let (mut session, _turn_context) =
            make_session_and_context_with_session_source(session_source).await;
        session.multi_agent_version = version.map(OnceLock::from).unwrap_or_default();
        let state = session.state.get_mut();
        let mut config = (*state.session_configuration.original_config_do_not_use).clone();
        config.multi_agent_v2.agent_polling = agent_polling;
        state.session_configuration.original_config_do_not_use = Arc::new(config);

        let mut report = InterAgentCommunication::new(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            Vec::new(),
            "done".to_string(),
            /*trigger_turn*/ true,
        );
        session.apply_child_report_mode(&mut report).await;
        kept.push(report.trigger_turn);
    }

    assert_eq!(kept, vec![false, false, true, false, true]);
}
