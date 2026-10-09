use super::*;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;
use uuid::Uuid;

fn current_items(
    store: &Arc<AsyncCompletions>,
    session: &Session,
    turn: &TurnContext,
) -> (CompletionClaim, Vec<ResponseItemEnvelope>) {
    store.claim(
        session.thread_id(),
        turn,
        CompletionDrain::ExactAssignment,
        &[],
    )
}

fn reserve(
    store: &Arc<AsyncCompletions>,
    session: &Session,
    turn: &TurnContext,
    call: &str,
) -> Result<CompletionReservation, &'static str> {
    store.reserve(
        session.thread_id(),
        turn,
        call,
        /*process_id*/ 1,
        /*cell_id*/ None,
    )
}

#[test_case::test_case(CompletionStatus::Exited(7); "nonzero")]
#[test_case::test_case(CompletionStatus::Failed; "failed")]
#[test_case::test_case(CompletionStatus::TimedOut; "timed_out")]
#[tokio::test]
async fn inline_and_initial_publication_races_have_one_consumer(status: CompletionStatus) {
    let (session, turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    let mut inline = reserve(&store, &session, &turn, "inline").unwrap();
    inline.register().publish(
        CompletionStatus::Exited(0),
        b"inline",
        /*omitted_bytes*/ 0,
    );
    assert!(current_items(&store, &session, &turn).1.is_empty());
    inline.release();
    assert!(current_items(&store, &session, &turn).1.is_empty());
    let mut yielded = reserve(&store, &session, &turn, "yielded").unwrap();
    let producer = yielded.register();
    producer.publish(status, b"first", /*omitted_bytes*/ 0);
    producer.publish(
        CompletionStatus::Failed,
        b"duplicate",
        /*omitted_bytes*/ 0,
    );
    assert!(current_items(&store, &session, &turn).1.is_empty());
    drop(yielded);
    let (claim, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 1);
    assert!(current_items(&store, &session, &turn).1.is_empty());
    drop(claim);
    let (claim, retry) = current_items(&store, &session, &turn);
    assert_eq!(items, retry);
    claim.recorded();
    let history = retry
        .into_iter()
        .map(ResponseItemEnvelope::into_item)
        .collect::<Vec<_>>();
    assert!(
        store
            .claim(
                session.thread_id(),
                &turn,
                CompletionDrain::ExactAssignment,
                &history
            )
            .1
            .is_empty()
    );
    let ids = store.candidates(
        session.thread_id(),
        &turn,
        CompletionDrain::ExactAssignment,
        &history,
    );
    assert_eq!(ids.len(), 1);
    store.accept(&ids);
    assert!(current_items(&store, &session, &turn).1.is_empty());
}

#[tokio::test]
async fn admission_and_claims_keep_capacity_until_accepted() {
    let (session, turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    for _ in 0..64 {
        let mut reservation = reserve(&store, &session, &turn, "finite").unwrap();
        reservation.register().publish(
            CompletionStatus::Exited(0),
            b"done",
            /*omitted_bytes*/ 0,
        );
    }
    assert!(reserve(&store, &session, &turn, "rejected").is_err());
    let (claim, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 8);
    assert!(reserve(&store, &session, &turn, "in-flight").is_err());
    claim.recorded();
    assert!(reserve(&store, &session, &turn, "recorded-unaccepted").is_err());
    let input = items
        .into_iter()
        .map(ResponseItemEnvelope::into_item)
        .collect::<Vec<_>>();
    assert!(
        store
            .claim(
                session.thread_id(),
                &turn,
                CompletionDrain::ExactAssignment,
                &input
            )
            .1
            .is_empty()
    );
    let (replay, repeated) = current_items(&store, &session, &turn);
    assert_eq!(repeated.len(), 8);
    drop(replay);
    store.accept(&store.candidates(
        session.thread_id(),
        &turn,
        CompletionDrain::ExactAssignment,
        &input,
    ));
    assert!(reserve(&store, &session, &turn, "accepted").is_ok());
    store.retire();
    assert!(reserve(&store, &session, &turn, "closed").is_err());
    assert!(current_items(&store, &session, &turn).1.is_empty());
}

#[tokio::test]
async fn terminal_tail_is_bounded_and_assignment_replacements_are_held() {
    let (session, turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    let assignment = AgentAssignmentId {
        thread_id: session.thread_id(),
        generation: Uuid::new_v4(),
    };
    turn.agent_assignment.set(assignment).unwrap();
    let mut reservation = reserve(&store, &session, &turn, "cancelled").unwrap();
    reservation.register().publish(
        CompletionStatus::Cancelled,
        "世界".repeat(1000).as_bytes(),
        /*omitted_bytes*/ 99,
    );
    drop(reservation);
    let (claim, items) = current_items(&store, &session, &turn);
    let item = serde_json::to_value(&items[0].item).unwrap();
    let text = item["content"][0]["text"].as_str().unwrap();
    assert!(text.len() <= MAX_FRAGMENT_BYTES);
    assert!(text.contains("status=cancelled"));
    assert!(text.contains("call=cancelled"));
    assert!(text.contains("truncated"));
    drop(claim);
    let (_, replacement) = make_session_and_context().await;
    replacement
        .agent_assignment
        .set(AgentAssignmentId {
            thread_id: session.thread_id(),
            generation: Uuid::new_v4(),
        })
        .unwrap();
    assert!(current_items(&store, &session, &replacement).1.is_empty());
    assert_eq!(current_items(&store, &session, &turn).1, items);
}

#[tokio::test]
async fn encoded_metadata_is_admitted_before_publication_and_output_is_bounded() {
    let (session, mut turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    turn.sub_id.clear();
    assert!(reserve(&store, &session, &turn, "empty-origin").is_err());
    turn.sub_id = "\n".repeat(128);
    assert!(
        store
            .reserve(
                session.thread_id(),
                &turn,
                &"\"".repeat(128),
                /*process_id*/ 1,
                Some(&"\\".repeat(128))
            )
            .is_err()
    );
    turn.sub_id = "bounded-turn".to_owned();
    let mut reservation = reserve(&store, &session, &turn, "bounded-call").unwrap();
    reservation.register().publish(
        CompletionStatus::Exited(i32::MIN),
        "\n\"\\世界".repeat(1000).as_bytes(),
        /*omitted_bytes*/ usize::MAX,
    );
    drop(reservation);
    let (_, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].item.turn_id(), Some("bounded-turn"));
    assert!(
        serde_json::to_value(&items[0].item).unwrap()["internal_chat_message_metadata_passthrough"]
            ["create_time"]
            .is_number()
    );
    assert!(serde_json::to_vec(&items[0].item).unwrap().len() <= MAX_FRAGMENT_BYTES);
}

#[tokio::test]
async fn explicit_root_recovery_uses_the_same_owner_policy_for_claim_and_acceptance() {
    use crate::session::input_queue::UserInputMetadata;
    use codex_protocol::AgentPath;
    use codex_protocol::protocol::InterAgentCommunication;
    use codex_protocol::protocol::InternalSessionSource;
    use codex_protocol::protocol::SubAgentSource;

    let (session, turn) = make_session_and_context().await;
    let (_, replacement) = make_session_and_context().await;
    for context in [&turn, &replacement] {
        context
            .agent_assignment
            .set(AgentAssignmentId {
                thread_id: session.thread_id(),
                generation: Uuid::new_v4(),
            })
            .unwrap();
    }
    let store = Arc::new(AsyncCompletions::default());
    let mut reservation = reserve(&store, &session, &turn, "original").unwrap();
    reservation.register().publish(
        CompletionStatus::Exited(0),
        b"done",
        /*omitted_bytes*/ 0,
    );
    drop(reservation);
    let (claim, original) = current_items(&store, &session, &turn);
    claim.recorded();
    let user = |content| TurnInput::UserInput {
        content,
        client_id: None,
        metadata: UserInputMetadata::default(),
    };
    let explicit = vec![user(vec![UserInput::Text {
        text: "recover".to_owned(),
        text_elements: Vec::new(),
    }])];
    let mail = TurnInput::InterAgentCommunication(InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").unwrap(),
        AgentPath::root(),
        Vec::new(),
        "recover".to_owned(),
        /*trigger_turn*/ true,
    ));
    let response = TurnInput::ResponseItem(original[0].clone());
    let function_output = TurnInput::FunctionCallOutput(ResponseItemEnvelope::new(
        serde_json::from_value(serde_json::json!({
            "type": "function_call_output", "call_id": "injected", "output": "recover",
        }))
        .unwrap(),
    ));
    let mut heartbeat = explicit[0].clone();
    if let TurnInput::UserInput { metadata, .. } = &mut heartbeat {
        metadata.origin = UserInputOrigin::Heartbeat;
    }
    let empty = vec![user(vec![])];
    let blank = vec![user(vec![UserInput::Text {
        text: " \n".to_owned(),
        text_elements: Vec::new(),
    }])];
    for (source, input) in [
        (SessionSource::Cli, vec![]),
        (SessionSource::Cli, empty),
        (SessionSource::Cli, blank),
        (SessionSource::Cli, vec![response]),
        (SessionSource::Cli, vec![function_output]),
        (SessionSource::Cli, vec![mail]),
        (SessionSource::Cli, vec![heartbeat.clone()]),
        (
            SessionSource::SubAgent(SubAgentSource::Review),
            explicit.clone(),
        ),
        (
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: session.thread_id(),
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            explicit.clone(),
        ),
        (
            SessionSource::Internal(InternalSessionSource::Guardian),
            explicit.clone(),
        ),
    ] {
        let drain = CompletionDrain::for_original_input(&source, &input);
        assert_eq!(drain, CompletionDrain::ExactAssignment);
        assert!(
            store
                .claim(session.thread_id(), &replacement, drain, &[])
                .1
                .is_empty()
        );
        assert!(
            store
                .candidates(
                    session.thread_id(),
                    &replacement,
                    drain,
                    &[original[0].item.clone()]
                )
                .is_empty()
        );
    }
    let drain = CompletionDrain::for_original_input(&SessionSource::Cli, &explicit);
    assert_eq!(drain, CompletionDrain::ExplicitRootRecovery);
    let mut mixed = explicit.clone();
    mixed.push(heartbeat);
    assert_eq!(
        CompletionDrain::for_original_input(&SessionSource::Cli, &mixed),
        drain
    );
    let (foreign, _) = make_session_and_context().await;
    assert!(
        store
            .claim(foreign.thread_id(), &replacement, drain, &[])
            .1
            .is_empty()
    );
    let (claim, recovered) = store.claim(session.thread_id(), &replacement, drain, &[]);
    assert_eq!(recovered, original);
    claim.recorded();
    let input = recovered
        .into_iter()
        .map(ResponseItemEnvelope::into_item)
        .collect::<Vec<_>>();
    assert!(
        store
            .candidates(foreign.thread_id(), &replacement, drain, &input)
            .is_empty()
    );
    let ids = store.candidates(session.thread_id(), &replacement, drain, &input);
    assert_eq!(ids, vec![input[0].id().unwrap().clone()]);
    store.accept(&ids);
    assert!(
        store
            .claim(session.thread_id(), &replacement, drain, &[])
            .1
            .is_empty()
    );
}
