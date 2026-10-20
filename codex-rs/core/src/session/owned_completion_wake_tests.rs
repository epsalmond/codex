use super::async_completion::CompletionStatus;
use super::tests::make_session_and_context_with_auth_and_config_and_session_source_and_rx;
use crate::tasks::PendingWorkStartResult;
use codex_login::CodexAuth;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn native_only_root_is_registered_before_completion_dispatch() {
    let (session, _, _) = make_session_and_context_with_auth_and_config_and_session_source_and_rx(
        CodexAuth::from_api_key("test-key"),
        Vec::new(),
        SessionSource::Cli,
        |_| {},
    )
    .await;
    let runtime = &session.services.local_agent_runtime;
    runtime.enable_wake_mode();
    assert!(runtime.ensure_agent_known(session.thread_id()).is_err());
    runtime
        .begin_wake_assignment_for_turn(
            session.thread_id(),
            &SessionSource::Cli,
            "native-root",
            /*allow_new_generation*/ true,
        )
        .unwrap();
    let metadata = runtime.ensure_agent_known(session.thread_id()).unwrap();
    assert_eq!(metadata.agent_id, Some(session.thread_id()));
    assert!(
        metadata
            .agent_path
            .as_ref()
            .is_some_and(codex_protocol::AgentPath::is_root)
    );
}

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

#[test_case::test_case(SessionSource::Cli, true; "root_wakes")]
#[test_case::test_case(
    SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::Review),
    false;
    "unassigned_child_does_not_wake"
)]
#[tokio::test]
async fn pending_lists_yielded_records_with_wake_ownership(
    source: SessionSource,
    wakes_owner: bool,
) {
    let (session, turn, _) =
        make_session_and_context_with_auth_and_config_and_session_source_and_rx(
            CodexAuth::from_api_key("test-key"),
            Vec::new(),
            source.clone(),
            |_| {},
        )
        .await;
    let store = &session.services.async_completions;
    // As `Session::new` does; the test constructor skips it.
    store.attach_owner(&session, &source);
    let mut inline = store
        .reserve(
            session.thread_id(),
            &turn,
            "inline",
            /*process_id*/ 6,
            /*cell_id*/ None,
        )
        .unwrap();
    let _inline_publisher = inline.register();
    let mut background = store
        .reserve(
            session.thread_id(),
            &turn,
            "background",
            /*process_id*/ 7,
            /*cell_id*/ None,
        )
        .unwrap();
    background.set_command("sleep 60");
    let publisher = background.register();
    drop(background);

    let pending = store.pending();
    assert_eq!(pending.len(), 1, "only yielded records are pending");
    let record = &pending[0];
    assert_eq!(
        (
            record.call_id.as_str(),
            record.process_id,
            record.command.as_deref(),
            record.ready,
            record.wakes_owner,
        ),
        ("background", Some(7), Some("sleep 60"), false, wakes_owner)
    );
    assert!(record.created_at > 0);
    assert_eq!(
        session.list_pending_completions().await[0]
            .process_id
            .as_deref(),
        Some("7")
    );

    if !wakes_owner {
        publisher.publish(
            CompletionStatus::Exited(0),
            b"done",
            /*omitted_bytes*/ 0,
        );
        assert!(
            store.pending()[0].ready,
            "published results stay pending until accepted"
        );
    }
    drop(inline);
}
