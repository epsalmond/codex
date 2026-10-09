use super::super::tests::make_session_and_context_with_rx;
use super::super::tests::make_session_and_context_with_session_source;
use crate::state::ActiveTurn;
use codex_features::AgentPolling;
use codex_protocol::AgentPath;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::turn_input::TurnStartOptions;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::OnceLock;

async fn paused_wake_session() -> Arc<super::super::session::Session> {
    let (mut session, _) = make_session_and_context_with_session_source(SessionSource::Cli).await;
    session.multi_agent_version = OnceLock::from(MultiAgentVersion::V2);
    let state = session.state.get_mut();
    let mut config = (*state.session_configuration.original_config_do_not_use).clone();
    config.multi_agent_v2.agent_polling = AgentPolling::Disabled;
    state.session_configuration.original_config_do_not_use = Arc::new(config);
    session.services.local_agent_runtime.enable_wake_mode();
    session.input_queue.pause_wakeups();
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    Arc::new(session)
}

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
        if version == Some(MultiAgentVersion::V2) && agent_polling == AgentPolling::Disabled {
            session.services.local_agent_runtime.enable_wake_mode();
        }

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

#[tokio::test]
async fn explicit_followup_clears_the_wake_pause_before_queuing_its_turn() {
    let session = paused_wake_session().await;
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("worker path"),
        Vec::new(),
        "explicit followup".to_string(),
        /*trigger_turn*/ true,
    );

    crate::session::handlers::inter_agent_communication(
        &session,
        "followup-sub-id".to_string(),
        communication,
        TurnStartOptions {
            parent_turn_id: Some("parent-turn".to_string()),
            ..Default::default()
        },
    )
    .await;

    assert!(!session.input_queue.wakeups_paused());
    assert!(session.input_queue.has_trigger_turn_mailbox_items().await);
}

#[tokio::test]
async fn queue_only_or_automatic_mail_does_not_clear_the_wake_pause() {
    for (trigger_turn, parent_turn_id) in [(false, None), (true, None)] {
        let session = paused_wake_session().await;
        let communication = InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("worker path"),
            Vec::new(),
            "queued message".to_string(),
            trigger_turn,
        );

        crate::session::handlers::inter_agent_communication(
            &session,
            "queued-sub-id".to_string(),
            communication,
            TurnStartOptions {
                parent_turn_id,
                ..Default::default()
            },
        )
        .await;

        assert!(session.input_queue.wakeups_paused());
    }
}

#[tokio::test]
async fn stale_failure_finalizer_does_not_pause_newer_generation() {
    let (session, context, _receiver) = make_session_and_context_with_rx().await;
    let runtime = &session.services.local_agent_runtime;
    runtime.enable_wake_mode();
    let old = runtime
        .begin_wake_assignment_for_turn(
            session.thread_id,
            &SessionSource::Exec,
            &context.sub_id,
            /*allow_new_generation*/ true,
        )
        .unwrap()
        .unwrap();
    context.agent_assignment.set(old).unwrap();
    let event = EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: context.sub_id.clone(),
        last_agent_message: None,
        error: Some(ErrorEvent {
            message: "old failure".to_owned(),
            codex_error_info: None,
            misalignment: None,
        }),
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
        root_turn_id: None,
    });
    session.classify_wake_turn_end(&context, &event).await;
    assert!(session.input_queue.wakeups_paused());
    session.input_queue.resume_wakeups();
    runtime
        .begin_wake_assignment_for_turn(
            session.thread_id,
            &SessionSource::Exec,
            "newer-turn",
            /*allow_new_generation*/ true,
        )
        .unwrap()
        .unwrap();
    session.classify_wake_turn_end(&context, &event).await;
    assert!(!session.input_queue.wakeups_paused());
}
