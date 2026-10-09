use super::*;
use codex_protocol::AgentPath;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
/// Opts the root into observation, as an Exec root with the drain enabled does at startup.
fn observe_root(session: &Session) -> crate::WorkObservation {
    let runtime = &session.services.local_agent_runtime;
    runtime.enable_wake_mode();
    runtime.enable_work_observation();
    runtime.work_observation()
}

async fn wait_for_rejection_event(
    events: &async_channel::Receiver<Event>,
    expected_id: &str,
) -> Event {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("session event stream should remain open");
            if event.id == expected_id && matches!(event.msg, EventMsg::Error(_)) {
                return event;
            }
        }
    })
    .await
    .expect("closed root should reject before launching work")
}

#[tokio::test]
async fn closed_root_rejects_review_and_standalone_shell_admission() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let observation = observe_root(&session);
    let (snapshot, _updates) = observation.subscribe();
    assert!(snapshot.quiescent);
    assert!(matches!(
        observation.shutdown_if_quiescent(&snapshot.revision),
        crate::GuardedShutdownOutcome::Closed(_)
    ));

    crate::session::handlers::review(
        &session,
        &turn_context.config,
        "closed-review".to_string(),
        ReviewRequest {
            target: ReviewTarget::Custom {
                instructions: "must not start after close".to_string(),
            },
            user_facing_hint: None,
        },
    )
    .await;
    let review_rejection = wait_for_rejection_event(&events, "closed-review").await;
    assert!(matches!(review_rejection.msg, EventMsg::Error(_)));

    crate::session::handlers::run_user_shell_command(
        &session,
        "closed-shell".to_string(),
        "printf should-not-run".to_string(),
        None,
    )
    .await;
    let shell_rejection = wait_for_rejection_event(&events, "closed-shell").await;
    assert!(matches!(shell_rejection.msg, EventMsg::Error(_)));
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn closed_root_rejects_manual_compact_before_aborting_or_spawning() {
    let (session, _turn_context, events) = make_session_and_context_with_rx().await;
    let observation = observe_root(&session);
    let (snapshot, _updates) = observation.subscribe();
    assert!(matches!(
        observation.shutdown_if_quiescent(&snapshot.revision),
        crate::GuardedShutdownOutcome::Closed(_)
    ));

    crate::session::handlers::compact(&session, "closed-compact".to_string()).await;
    let rejection = wait_for_rejection_event(&events, "closed-compact").await;
    assert!(matches!(rejection.msg, EventMsg::Error(_)));
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn closed_root_rejects_task_spawn_before_replacing_active_work() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let observation = observe_root(&session);
    let (snapshot, _updates) = observation.subscribe();
    assert!(matches!(
        observation.shutdown_if_quiescent(&snapshot.revision),
        crate::GuardedShutdownOutcome::Closed(_)
    ));

    session
        .spawn_task(turn_context, Vec::new(), crate::tasks::RegularTask::new())
        .await;
    let rejection = wait_for_rejection_event(&events, "turn_id").await;
    assert!(matches!(rejection.msg, EventMsg::Error(_)));
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn closed_root_keeps_pending_wakeup_mail_queued() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    session
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::root(),
                AgentPath::root(),
                Vec::new(),
                "pending trigger".to_string(),
                /*trigger_turn*/ true,
            ),
            Default::default(),
        )
        .await;
    let observation = observe_root(&session);
    let (snapshot, _updates) = observation.subscribe();
    assert!(matches!(
        observation.shutdown_if_quiescent(&snapshot.revision),
        crate::GuardedShutdownOutcome::Closed(_)
    ));

    session
        .maybe_start_turn_for_pending_work_with_sub_id("closed-wakeup".to_string())
        .await;
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(session.input_queue.trigger_turn_mailbox_count().await, 1);
}

#[tokio::test]
async fn closed_root_rejects_regular_turn_before_task_spawn() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let observation = observe_root(&session);
    let (snapshot, _updates) = observation.subscribe();
    assert!(matches!(
        observation.shutdown_if_quiescent(&snapshot.revision),
        crate::GuardedShutdownOutcome::Closed(_)
    ));

    let submission = crate::session::turn_input::handle(
        &session,
        codex_protocol::turn_input::TurnInputRequest::user_input(vec![UserInput::Text {
            text: "must not start after close".to_string(),
            text_elements: Vec::new(),
        }]),
        TurnInputMode::StartOrSteer,
        "closed-turn".to_string(),
    )
    .await
    .expect("closed root should be rejected as a typed admission result");
    assert_eq!(
        submission,
        TurnInputSubmission::NotSubmitted {
            reason: codex_protocol::turn_input::NotSubmittedReason::ServerDraining,
        }
    );
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn terminal_root_turn_stays_busy_until_output_is_forwarded() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let observation = observe_root(&session);
    observation.subscribe();
    session
        .register_root_turn_lifecycle(&turn_context)
        .expect("root turn should be admitted");

    session
        .emit_turn_started(&turn_context, crate::state::TaskKind::Regular)
        .await;
    let started = events
        .recv()
        .await
        .expect("turn started event should be forwarded");
    assert!(matches!(started.msg, EventMsg::TurnStarted(_)));

    session
        .send_event(
            &turn_context,
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn_context.sub_id.clone(),
                last_agent_message: Some("final answer".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                root_turn_id: None,
            }),
        )
        .await;
    let completed = events
        .recv()
        .await
        .expect("terminal event should be forwarded to the listener");
    assert!(matches!(completed.msg, EventMsg::TurnComplete(_)));
    assert!(!observation.snapshot().quiescent);

    session
        .services
        .local_agent_runtime
        .root_turn_output_forwarded(&turn_context.sub_id);
    assert!(observation.snapshot().quiescent);
}

#[tokio::test]
async fn rejected_wake_bind_releases_its_root_turn() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let observation = observe_root(&session);
    observation.subscribe();
    // Another turn already runs the root assignment, so binding the wake turn fails after
    // admission registered it.
    session
        .services
        .local_agent_runtime
        .begin_wake_assignment_for_turn(
            session.thread_id,
            &SessionSource::Exec,
            "other-turn",
            /*allow_new_generation*/ true,
        )
        .expect("root assignment should start");
    session
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::root(),
                AgentPath::root(),
                Vec::new(),
                "pending trigger".to_string(),
                /*trigger_turn*/ true,
            ),
            Default::default(),
        )
        .await;

    assert_eq!(
        session
            .maybe_start_turn_for_pending_work_with_sub_id("rejected-wake".to_string())
            .await,
        crate::tasks::PendingWorkStartResult::Stale,
    );
    let snapshot = observation.snapshot();
    assert_eq!(
        (
            snapshot.active_root_turns,
            snapshot.pending_terminal_outputs
        ),
        (0, 0)
    );
}

#[tokio::test]
async fn unobserved_root_tracks_no_root_turns() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let runtime = &session.services.local_agent_runtime;
    // Admitted turns that never report a terminal event must not fill the tracked-turn cap.
    for index in 0..40 {
        session
            .register_root_turn_lifecycle_id(&format!("turn-{index}"), &SessionSource::Exec)
            .expect("an unobserved root admits every turn");
    }
    let snapshot = runtime.work_observation().snapshot();
    assert_eq!(
        (
            snapshot.active_root_turns,
            snapshot.pending_terminal_outputs
        ),
        (0, 0)
    );
}
