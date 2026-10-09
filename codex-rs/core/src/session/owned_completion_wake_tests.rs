use super::async_completion::CompletionStatus;
use super::tests::make_session_and_context_with_auth_and_config_and_session_source_and_rx;
use crate::tasks::PendingWorkStartResult;
use codex_login::CodexAuth;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn rejected_owned_completion_start_without_mail_pauses_and_retains_result() {
    let (session, turn, _) =
        make_session_and_context_with_auth_and_config_and_session_source_and_rx(
            CodexAuth::from_api_key("test-key"),
            Vec::new(),
            SessionSource::Cli,
            |config| {
                config
                    .features
                    .enable(codex_features::Feature::MultiAgentV2)
                    .unwrap();
                config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Disabled;
            },
        )
        .await;
    assert_eq!(session.multi_agent_version(), Some(MultiAgentVersion::V2));
    session.services.local_agent_runtime.enable_wake_mode();
    let mut reservation = session
        .services
        .async_completions
        .reserve(
            session.thread_id(),
            &turn,
            "finite",
            /*process_id*/ 1,
            /*cell_id*/ None,
        )
        .unwrap();
    reservation.register().publish(
        CompletionStatus::Exited(0),
        b"retained",
        /*omitted_bytes*/ 0,
    );
    drop(reservation);
    assert_eq!(
        session
            .maybe_start_turn_for_pending_work_with_sub_id("rejected".to_owned())
            .await,
        PendingWorkStartResult::Stale
    );
    assert!(session.input_queue.wakeups_paused());
    assert!(
        session
            .services
            .async_completions
            .has_ready(session.thread_id(), None)
    );
    assert!(session.active_turn.lock().await.is_none());
    assert!(!session.input_queue.has_pending_mailbox_items().await);
    assert_eq!(
        session.maybe_start_turn_for_pending_work().await,
        PendingWorkStartResult::Stale
    );
}

#[test_case::test_case(MultiAgentVersion::V1; "legacy_child")]
#[test_case::test_case(MultiAgentVersion::V2; "polling_v2_child")]
#[tokio::test]
async fn unassigned_child_retains_explicit_completion_retrieval(version: MultiAgentVersion) {
    let source = SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::Review);
    let (session, turn, _) =
        make_session_and_context_with_auth_and_config_and_session_source_and_rx(
            CodexAuth::from_api_key("test-key"),
            Vec::new(),
            source,
            |config| {
                config
                    .features
                    .enable(codex_features::Feature::Collab)
                    .unwrap();
                config
                    .features
                    .set_enabled(
                        codex_features::Feature::MultiAgentV2,
                        version == MultiAgentVersion::V2,
                    )
                    .unwrap();
                config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Enabled;
            },
        )
        .await;
    assert_eq!(session.multi_agent_version(), Some(version));
    let mut reservation = session
        .services
        .async_completions
        .reserve(
            session.thread_id(),
            &turn,
            "finite",
            /*process_id*/ 1,
            /*cell_id*/ None,
        )
        .unwrap();
    reservation.register().publish(
        CompletionStatus::Exited(0),
        b"explicitly retained",
        /*omitted_bytes*/ 0,
    );
    drop(reservation);
    assert_eq!(
        session.maybe_start_turn_for_pending_work().await,
        PendingWorkStartResult::Stale
    );
    assert!(
        session
            .services
            .async_completions
            .has_ready(session.thread_id(), None)
    );
    assert!(session.active_turn.lock().await.is_none());
}
