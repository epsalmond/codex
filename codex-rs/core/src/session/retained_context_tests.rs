use std::sync::Arc;
use std::time::Duration;

use codex_history::RetainedContextEntry;
use codex_history::RetainedUserMessage;
use pretty_assertions::assert_eq;
use test_case::test_case;

use super::thread_settings;
use crate::session::tests::make_session_and_context;

fn delivered_message() -> RetainedUserMessage {
    RetainedUserMessage {
        origin: codex_history::UserInputOrigin::User,
        turn_id: "turn".to_owned(),
        message_id: Some("send".to_owned()),
        text: "May I publish?".to_owned(),
        complete: true,
        phase: None,
    }
}

#[tokio::test]
async fn shutdown_retains_an_admitted_delivery_until_persistence_finishes() {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let checkpoint = thread_settings::acquire_persistence_lock(&session).await;
    let dispatch = session.track_code_mode_message().expect("open dispatch");
    let mut shutdown = Box::pin(session.drain_code_mode_messages());
    assert!(futures::poll!(shutdown.as_mut()).is_pending());
    assert!(session.track_code_mode_message().is_none());

    let message = delivered_message();
    let (admitted, receiver) = tokio::sync::oneshot::channel();
    let outer_session = Arc::clone(&session);
    let outer_message = message.clone();
    let outer = tokio::spawn(async move {
        let (_, completion) = outer_session.record_delivered_assistant_message(outer_message);
        admitted
            .send(completion)
            .expect("receive recording admission");
        futures::future::pending::<()>().await;
    });
    let mut completion = receiver.await.expect("recording admitted");
    outer.abort();
    assert!(
        outer
            .await
            .expect_err("outer call interrupted")
            .is_cancelled()
    );
    drop(dispatch);
    let live_messages = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            let history = session.clone_history().await;
            let messages = history
                .retained_context()
                .ordered_entries()
                .filter_map(|(_, entry)| match entry {
                    RetainedContextEntry::AssistantMessage(message) => Some(message.clone()),
                    RetainedContextEntry::UserMessage(_)
                    | RetainedContextEntry::VerifiedAnswer(_) => None,
                })
                .collect::<Vec<_>>();
            if !messages.is_empty() {
                break messages;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("live admission does not wait for the checkpoint");
    assert_eq!(live_messages, vec![message]);
    assert!(futures::poll!(shutdown.as_mut()).is_pending());
    assert!(!completion.has_changed().expect("recording remains open"));

    drop(checkpoint);
    shutdown.await;
    assert!(completion.changed().await.is_err());
}

#[test_case(false; "free state")]
#[test_case(true; "busy state")]
#[tokio::test]
async fn confirmed_delivery_reserves_order_and_persists_before_a_fast_user_reply(busy: bool) {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let checkpoint = thread_settings::acquire_persistence_lock(&session).await;
    let state = if busy {
        Some(session.state.lock().await)
    } else {
        None
    };
    let (recording, _) = session.record_delivered_assistant_message(delivered_message());
    let reply_session = Arc::clone(&session);
    let mut reply = tokio::spawn(async move { reply_session.reserve_user_input_order().await });
    drop(state);
    assert!(
        tokio::time::timeout(Duration::from_millis(/*millis*/ 100), &mut reply)
            .await
            .is_err()
    );
    drop(checkpoint);
    let reply_order = reply
        .await
        .expect("reply accepted after delivery persistence");
    recording.await.expect("confirmed delivery recorded");
    let assistant_order = session
        .clone_history()
        .await
        .retained_context()
        .ordered_entries()
        .find_map(|(order, entry)| {
            matches!(entry, RetainedContextEntry::AssistantMessage(_)).then_some(order)
        })
        .expect("confirmed delivery order");
    assert!(assistant_order < codex_history::RetainedContextOrder::Local(reply_order));
}

#[test_case("pending_recording"; "code_mode_recording_wait")]
#[test_case("turn_state"; "recovery_owner_lock")]
#[tokio::test]
async fn cancelled_second_recording_preserves_deferred_report_and_replacement_mail(barrier: &str) {
    use crate::session::TurnInput;
    use crate::session::multi_agents::ChildReportMode;
    use crate::state::ActiveTurn;
    use codex_protocol::AgentPath;
    use codex_protocol::protocol::InterAgentCommunication;
    use codex_protocol::protocol::TurnAbortReason;

    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let old_turn = ActiveTurn::default();
    let owner = old_turn.turn_state.clone();
    *session.active_turn.lock().await = Some(old_turn);
    let mail = |content: &str| -> crate::PendingAgentMessage {
        InterAgentCommunication::new(
            AgentPath::root().join("worker").unwrap(),
            AgentPath::root(),
            Vec::new(),
            content.into(),
            false,
        )
        .into()
    };
    let first = mail("deferred report");
    let second = mail("second report");
    let batch = vec![
        TurnInput::InterAgentCommunication(first),
        TurnInput::InterAgentCommunication(second.clone()),
    ];
    session
        .input_queue
        .extend_pending_input_for_turn_state(&owner, batch.clone())
        .await;
    let (drained, captured_owner) = session
        .input_queue
        .drain_pending_input_with_recovery_owner(&session.active_turn)
        .await;
    assert!(Arc::ptr_eq(captured_owner.as_ref().unwrap(), &owner));
    // A fresh delivery with identical content is still owned by the replacement turn.
    let replacement = ActiveTurn::default();
    let replacement_state = replacement.turn_state.clone();
    let replacement_mail = vec![TurnInput::InterAgentCommunication(mail("second report"))];
    session
        .input_queue
        .extend_pending_input_for_turn_state(&replacement_state, replacement_mail.clone())
        .await;
    let old_turn = session
        .active_turn
        .lock()
        .await
        .replace(replacement)
        .unwrap();
    let (recorded, recording) = tokio::sync::watch::channel(());
    if barrier == "pending_recording" {
        session
            .code_mode_message_tasks
            .pending_persistence
            .lock()
            .unwrap()
            .push(recording);
    }
    let state_guard = if barrier == "turn_state" {
        Some(owner.lock().await)
    } else {
        None
    };
    {
        // R1 is deferred in the batch while R2 waits at its actual recording barrier.
        let recording = session.record_inter_agent_communication_with_pending_input(
            &turn,
            turn.model_info(),
            second,
            captured_owner.as_deref(),
        );
        tokio::pin!(recording);
        assert!(futures::poll!(recording.as_mut()).is_pending());
    }
    drop(state_guard);
    drop(recorded);
    assert!(session.clone_history().await.raw_items().next().is_none());
    assert!(
        session
            .input_queue
            .clear_pending(
                &old_turn,
                &TurnAbortReason::Replaced,
                ChildReportMode::WaitAgent
            )
            .await
    );
    let (recovered, _) = session.input_queue.drain_mailbox_input_items().await;
    assert_eq!(
        recovered, drained,
        "both reports remain ordered after cancellation"
    );
    assert_eq!(
        session
            .input_queue
            .take_pending_input_for_turn_state(&replacement_state)
            .await,
        replacement_mail
    );
    assert!(
        session
            .input_queue
            .drain_mailbox_input_items()
            .await
            .0
            .is_empty()
    );
}
