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
    store.claim(session.thread_id(), turn, &[])
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
            .claim(session.thread_id(), &turn, &history)
            .1
            .is_empty()
    );
    let ids = store.candidates(session.thread_id(), &turn, &history);
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
    assert!(store.claim(session.thread_id(), &turn, &input).1.is_empty());
    let (replay, repeated) = current_items(&store, &session, &turn);
    assert_eq!(repeated.len(), 8);
    drop(replay);
    store.accept(&store.candidates(session.thread_id(), &turn, &input));
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
