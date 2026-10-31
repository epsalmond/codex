use super::*;

#[test]
fn ready_event_after_admission_returns_is_retained_across_wake_handoff() {
    let mut wake = CompletionWake::default();
    let admission = wake.request().unwrap();
    // The helper already returned AlreadyActive. A second producer publishes after the
    // active turn's final check, before the original worker can release its scheduled bit.
    assert_eq!(wake.request(), None);
    assert!(wake.finish(admission));
    let new_event_attempt = wake.request().unwrap();
    assert!(!wake.finish(new_event_attempt));
    // Busy/deferred/paused admission alone cannot create another attempt.
    assert!(!wake.scheduled);
}
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

/// Returns the fragment body without its type markers.
fn fragment_text(item: &ResponseItemEnvelope) -> String {
    let item = serde_json::to_value(&item.item).unwrap();
    let (start, end) = AsyncToolCompletion::type_markers();
    item["content"][0]["text"]
        .as_str()
        .unwrap()
        .strip_prefix(start)
        .unwrap()
        .strip_suffix(end)
        .unwrap()
        .to_owned()
}

fn header_field<'a>(text: &'a str, key: &str) -> &'a str {
    let header = text.lines().next().unwrap();
    assert!(header.starts_with("id="));
    header
        .split(' ')
        .find_map(|field| field.strip_prefix(key)?.strip_prefix('='))
        .unwrap()
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
    turn.sub_id = "\u{1}".repeat(129);
    assert!(reserve(&store, &session, &turn, "long-origin").is_err());
    // Identifiers at the 128-byte bound made entirely of the widest JSON escapes cannot fit a
    // fragment, so admission rejects them before publication.
    turn.sub_id = "\u{1}".repeat(128);
    assert!(
        store
            .reserve(
                session.thread_id(),
                &turn,
                &"\u{1}".repeat(128),
                /*process_id*/ i32::MIN,
                Some(&"\u{1}".repeat(128)),
            )
            .is_err()
    );
    // Identifiers at the 128-byte bound still leave room for a tail.
    turn.sub_id = "t".repeat(128);
    let mut widest = store
        .reserve(
            session.thread_id(),
            &turn,
            &"c".repeat(128),
            /*process_id*/ i32::MIN,
            Some(&"p".repeat(128)),
        )
        .unwrap();
    widest.register().publish(
        CompletionStatus::Exited(i32::MIN),
        "\n\"\\世界".repeat(1000).as_bytes(),
        /*omitted_bytes*/ usize::MAX,
    );
    drop(widest);
    let (claim, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 1);
    let encoded = serde_json::to_vec(&items[0].item).unwrap().len();
    assert!(encoded <= MAX_FRAGMENT_BYTES);
    // Escaped output is trimmed by its encoded width, so the tail fills the remaining room.
    assert!(encoded > MAX_FRAGMENT_BYTES - 16);
    assert!(fragment_text(&items[0]).ends_with("世界"));
    drop(claim);
    store.accept(&[items[0].item.id().unwrap().clone()]);
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
    let mail = TurnInput::InterAgentCommunication(
        InterAgentCommunication::new(
            AgentPath::try_from("/root/worker").unwrap(),
            AgentPath::root(),
            Vec::new(),
            "recover".to_owned(),
            /*trigger_turn*/ true,
        )
        .into(),
    );
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

#[tokio::test]
async fn fragment_reports_truncated_bytes_and_uses_the_3072_byte_bound() {
    let (session, turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    let mut short = reserve(&store, &session, &turn, "short").unwrap();
    short.register().publish(
        CompletionStatus::Exited(0),
        b"whole output",
        /*omitted_bytes*/ 0,
    );
    drop(short);
    let output = format!("{}END", "a".repeat(10_000));
    let mut long = reserve(&store, &session, &turn, "long").unwrap();
    long.register().publish(
        CompletionStatus::Exited(1),
        output.as_bytes(),
        /*omitted_bytes*/ 5,
    );
    drop(long);
    let (_, items) = current_items(&store, &session, &turn);
    let texts = items.iter().map(fragment_text).collect::<Vec<_>>();
    let short = texts
        .iter()
        .find(|text| header_field(text, "call") == "short")
        .unwrap();
    assert_eq!(header_field(short, "truncated"), "0");
    assert!(short.contains("status=exited(0) truncated=0\nwhole output"));
    let (index, long) = texts
        .iter()
        .enumerate()
        .find(|(_, text)| header_field(text, "call") == "long")
        .unwrap();
    assert!(long.contains("kind=process process=1 cell=none"));
    let omitted: usize = header_field(long, "truncated").parse().unwrap();
    let tail = long.split_once('\n').unwrap().1;
    assert!(tail.ends_with("END"));
    assert_eq!(omitted + tail.len(), output.len() + 5);
    let encoded = serde_json::to_vec(&items[index].item).unwrap().len();
    assert!(encoded <= MAX_FRAGMENT_BYTES);
    // The tail fills the bound instead of stopping near the former 768-byte cap.
    assert!(encoded > MAX_FRAGMENT_BYTES - 16);
    assert!(tail.len() > MAX_FRAGMENT_BYTES * 3 / 4);
}

#[tokio::test]
async fn cell_fragment_has_cell_kind_and_bounded_tail() {
    let (session, turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    let id = "generation:exact-cell";
    let mut cell = store
        .reserve_cell(session.thread_id(), &turn, "cell-call")
        .unwrap();
    // An unbound cell has no correlatable identity, so it cannot publish.
    cell.register().publish(
        CompletionStatus::CellCompleted,
        b"unbound",
        /*omitted_bytes*/ 0,
    );
    assert!(current_items(&store, &session, &turn).1.is_empty());
    cell.bind_cell(id).unwrap();
    assert!(cell.bind_cell("rebound").is_err());
    let output = format!("{}cell-tail", "\\\"世界".repeat(2000));
    cell.register().publish(
        CompletionStatus::CellError,
        output.as_bytes(),
        /*omitted_bytes*/ 0,
    );
    drop(cell);
    let mut native = store
        .reserve(
            session.thread_id(),
            &turn,
            "native-call",
            /*process_id*/ 7,
            Some(id),
        )
        .unwrap();
    assert!(native.bind_cell(id).is_err());
    native.register().publish(
        CompletionStatus::Exited(3),
        b"native",
        /*omitted_bytes*/ 0,
    );
    drop(native);
    let (claim, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 2);
    let texts = items.iter().map(fragment_text).collect::<Vec<_>>();
    let cell = texts
        .iter()
        .find(|text| header_field(text, "call") == "cell-call")
        .unwrap();
    assert!(cell.contains(" kind=cell cell=generation:exact-cell assignment=none status=error "));
    assert!(!cell.contains("process="));
    let omitted: usize = header_field(cell, "truncated").parse().unwrap();
    let tail = cell.split_once('\n').unwrap().1;
    assert!(omitted > 0);
    assert!(tail.ends_with("cell-tail"));
    assert_eq!(omitted + tail.len(), output.len());
    let native = texts
        .iter()
        .find(|text| header_field(text, "call") == "native-call")
        .unwrap();
    assert!(native.contains(" kind=process process=7 cell=generation:exact-cell "));
    assert!(native.contains("status=exited(3) truncated=0\nnative"));
    for item in &items {
        assert!(serde_json::to_vec(&item.item).unwrap().len() <= MAX_FRAGMENT_BYTES);
    }
    drop(claim);
    let (other, _) = make_session_and_context().await;
    assert!(
        store
            .claim(
                other.thread_id(),
                &turn,
                CompletionDrain::ExplicitRootRecovery,
                &[]
            )
            .1
            .is_empty()
    );
    // Cells share the native reservation capacity.
    let mut held = vec![];
    for _ in 2..MAX_RESERVATIONS {
        held.push(
            store
                .reserve_cell(session.thread_id(), &turn, "pending")
                .unwrap(),
        );
    }
    assert!(reserve(&store, &session, &turn, "full-native").is_err());
    assert!(
        store
            .reserve_cell(session.thread_id(), &turn, "full-cell")
            .is_err()
    );
    drop(held);
    assert!(
        store
            .reserve_cell(session.thread_id(), &turn, "available")
            .is_ok()
    );
}

#[tokio::test]
async fn bind_cell_rejects_identifiers_over_128_bytes() {
    let (session, turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    let mut cell = store
        .reserve_cell(session.thread_id(), &turn, "cell-call")
        .unwrap();
    assert_eq!(
        cell.bind_cell(&"c".repeat(129)),
        Err("async completion identifiers exceed the bounded record limit")
    );
    // The rejected reservation stays unbound, so it still cannot publish.
    let publisher = cell.register();
    publisher.publish(
        CompletionStatus::CellCompleted,
        b"rejected",
        /*omitted_bytes*/ 0,
    );
    let bound = "\u{1}".repeat(128);
    cell.bind_cell(&bound).unwrap();
    publisher.publish(
        CompletionStatus::CollectionFailed,
        b"bound",
        /*omitted_bytes*/ 0,
    );
    drop(cell);
    let (_, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 1);
    let text = fragment_text(&items[0]);
    assert_eq!(header_field(&text, "cell"), bound);
    assert!(text.ends_with("status=collectionFailed truncated=0\nbound"));
}

#[test_case::test_case(CompletionStatus::Exited(0); "success")]
#[test_case::test_case(CompletionStatus::Exited(17); "nonzero")]
#[test_case::test_case(CompletionStatus::Cancelled; "cancelled")]
#[tokio::test]
async fn finite_lifecycle_holds_drain_through_recording_and_model_acceptance(
    status: CompletionStatus,
) {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let runtime = &session.services.local_agent_runtime;
    let observation = runtime.work_observation();
    observation.subscribe();
    let work = runtime
        .reserve_owned_work(&session, &turn, "finite", WorkKind::Task, None)
        .unwrap();
    work.mark_started();
    let store = Arc::clone(&session.services.async_completions);
    let mut reservation = store
        .reserve_with_lifecycle(session.thread_id(), &turn, "finite", 41, None, &work)
        .unwrap();
    let publisher = reservation.register();
    work.mark_yielded(true);
    drop(reservation);
    assert_eq!(observation.snapshot().running_finite_work, 1);
    publisher.publish(status, b"terminal tail", 0);
    work.mark_terminal();
    drop(work);
    let terminal = observation.snapshot();
    assert_eq!(terminal.running_finite_work, 0);
    assert_eq!(terminal.pending_notifications, 1);
    assert!(!terminal.quiescent);
    let (claim, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 1);
    claim.recorded();
    assert!(!observation.snapshot().quiescent);
    let input = items
        .into_iter()
        .map(ResponseItemEnvelope::into_item)
        .collect::<Vec<_>>();
    let accepted = store.candidates(
        session.thread_id(),
        &turn,
        CompletionDrain::ExactAssignment,
        &input,
    );
    assert_eq!(accepted.len(), 1);
    store.accept(&accepted);
    assert!(observation.snapshot().quiescent);
    assert!(!runtime.has_live_owned_work(session.thread_id()));
    assert!(current_items(&store, &session, &turn).1.is_empty());
}

#[tokio::test]
async fn yielded_cell_is_visible_but_not_deliverable_until_native_close_then_acceptance() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let runtime = &session.services.local_agent_runtime;
    let observation = runtime.work_observation();
    observation.subscribe();
    let work = runtime
        .reserve_owned_work(&session, &turn, "event-held", WorkKind::Cell, None)
        .unwrap();
    work.mark_started();
    work.mark_yielded(true);
    let store = Arc::clone(&session.services.async_completions);
    let mut reservation = store
        .reserve_cell_with_lifecycle(session.thread_id(), &turn, "event-held", &work)
        .unwrap();
    reservation.bind_cell("native-cell").unwrap();
    assert!(store.pending().is_empty());
    let publisher = reservation.register();
    assert!(store.pending().is_empty());
    reservation.mark_cell_yielded_for_visibility();
    let pending = store.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].cell_id.as_deref(), Some("native-cell"));
    assert!(!pending[0].ready);
    assert!(!store.has_ready(session.thread_id(), None));
    assert!(current_items(&store, &session, &turn).1.is_empty());
    assert!(runtime.has_live_owned_work(session.thread_id()));
    publisher.publish(CompletionStatus::CellCompleted, b"done", 0);
    assert_eq!(store.published_texts().len(), 1);
    assert_eq!(store.pending(), pending);
    assert!(!store.has_ready(session.thread_id(), None));
    assert!(current_items(&store, &session, &turn).1.is_empty());
    // Native close releases the delivery fence, leaving the existing record as the final hold.
    work.mark_terminal();
    drop(work);
    drop(reservation);
    assert!(runtime.has_live_owned_work(session.thread_id()));
    assert!(!observation.snapshot().quiescent);
    assert!(store.pending()[0].ready);
    assert!(store.has_ready(session.thread_id(), None));
    let (claim, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 1);
    claim.recorded();
    let input = items
        .into_iter()
        .map(ResponseItemEnvelope::into_item)
        .collect::<Vec<_>>();
    let accepted = store.candidates(
        session.thread_id(),
        &turn,
        CompletionDrain::ExactAssignment,
        &input,
    );
    assert_eq!(accepted.len(), 1);
    store.accept(&accepted);
    assert!(store.pending().is_empty());
    assert!(!runtime.has_live_owned_work(session.thread_id()));
    assert!(observation.snapshot().quiescent);
}

#[derive(Default)]
struct NativeTerminalEventGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl codex_extension_api::TurnLifecycleContributor for NativeTerminalEventGate {
    fn on_item_completed<'a>(
        &'a self,
        _thread_store: &'a codex_extension_api::ExtensionData,
        _turn_store: &'a codex_extension_api::ExtensionData,
        item: &'a codex_protocol::items::TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if matches!(item, codex_protocol::items::TurnItem::CommandExecution(_)) {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}

#[test_case::test_case(0; "success")]
#[test_case::test_case(17; "nonzero")]
#[tokio::test]
async fn native_watcher_closes_terminal_events_before_child_acceptance(
    exit_code: i32,
) -> anyhow::Result<()> {
    use crate::agent::control::AssignmentPhase;
    use crate::agent::control::TurnEndDisposition;
    use crate::unified_exec::UnifiedExecContext;
    use crate::unified_exec::async_watcher::spawn_exit_watcher;
    use crate::unified_exec::async_watcher::start_streaming_output;
    use crate::unified_exec::async_watcher::tests::StreamingOutputHarness;
    use crate::unified_exec::async_watcher::tests::streaming_output_harness_with_context;
    use crate::unified_exec::async_watcher::tests::wait_for_exit_watcher;
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use tokio::time::Duration;
    use tokio::time::Instant;
    use tokio::time::timeout;

    timeout(Duration::from_secs(5), async {
        let (mut session, mut turn) = make_session_and_context().await;
        let gate = Arc::new(NativeTerminalEventGate::default());
        let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
        extensions.turn_lifecycle_contributor(gate.clone());
        session.services.extensions = Arc::new(extensions.build());
        let (tx_event, rx_event) = async_channel::unbounded();
        session.tx_event = tx_event;
        let runtime = &session.services.local_agent_runtime;
        runtime.enable_wake_mode();
        let parent_thread_id = ThreadId::new();
        runtime
            .begin_wake_assignment_for_turn(
                parent_thread_id,
                &SessionSource::Cli,
                "parent-turn",
                true,
            )
            .unwrap();
        turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        });
        let assignment = runtime
            .begin_wake_assignment_for_turn(
                session.thread_id(),
                &turn.session_source,
                &turn.sub_id,
                true,
            )
            .unwrap()
            .unwrap();
        turn.agent_assignment.set(assignment.clone()).unwrap();
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let context = UnifiedExecContext::new(
            Arc::clone(&session),
            crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
            tokio_util::sync::CancellationToken::new(),
            "native-event-held".to_owned(),
        );
        let StreamingOutputHarness {
            process,
            stdout_tx,
            exit_tx,
            output_buffer,
            context,
            rx_event,
        } = streaming_output_harness_with_context(context, rx_event).await?;
        let runtime = &session.services.local_agent_runtime;
        let work = runtime
            .reserve_owned_work(&session, &turn, &context.call_id, WorkKind::Task, None)
            .unwrap();
        work.mark_started();
        work.mark_yielded(true);
        let store = Arc::clone(&session.services.async_completions);
        let mut reservation = store
            .reserve_with_lifecycle(
                session.thread_id(),
                &turn,
                &context.call_id,
                123,
                None,
                &work,
            )
            .unwrap();
        let publisher = reservation.register();
        drop(reservation);
        assert_eq!(
            runtime
                .classify_wake_turn_end(&assignment, &turn.sub_id, TurnEndDisposition::Succeeded)
                .await,
            Ok(AssignmentPhase::Waiting)
        );
        start_streaming_output(&process, &context);
        #[allow(deprecated)]
        let cwd = turn.cwd.clone().into();
        spawn_exit_watcher(
            Arc::clone(&process),
            &context,
            vec!["native-event-held".to_owned()],
            cwd,
            123,
            None,
            output_buffer,
            Instant::now(),
            None,
            None,
            Some(publisher),
            Some(work),
        );
        stdout_tx.send(b"terminal tail".to_vec())?;
        drop(stdout_tx);
        exit_tx.send(exit_code).expect("send native exit");
        gate.entered.notified().await;
        // This barrier is inside the actual awaited terminal event path. The old
        // watcher had already published its result here and could strand Waiting.
        assert!(runtime.has_live_owned_work(session.thread_id()));
        assert!(!store.has_ready(session.thread_id(), Some(&assignment)));
        assert!(current_items(&store, &session, &turn).1.is_empty());
        assert!(
            store
                .candidates(
                    session.thread_id(),
                    &turn,
                    CompletionDrain::ExactAssignment,
                    &[]
                )
                .is_empty()
        );
        assert!(
            runtime
                .continue_owned_completion(&assignment, "premature")
                .is_err()
        );
        gate.release.notify_one();
        loop {
            if let EventMsg::ItemCompleted(completed) = rx_event.recv().await?.msg
                && let TurnItem::CommandExecution(item) = completed.item
            {
                assert_eq!(item.exit_code, Some(exit_code));
                assert_eq!(item.aggregated_output.as_deref(), Some("terminal tail"));
                break;
            }
        }
        // The interaction lock is held across terminal forwarding and publication.
        // Its release confirms the real watcher has finished every awaited event.
        wait_for_exit_watcher(&process).await;
        assert!(store.has_ready(session.thread_id(), Some(&assignment)));
        runtime
            .continue_owned_completion(&assignment, "automatic")
            .unwrap();
        let (claim, items) = current_items(&store, &session, &turn);
        assert_eq!(items.len(), 1);
        assert_eq!(
            header_field(&fragment_text(&items[0]), "status"),
            format!("exited({exit_code})")
        );
        assert!(fragment_text(&items[0]).ends_with("\nterminal tail"));
        claim.recorded();
        let input = items
            .into_iter()
            .map(ResponseItemEnvelope::into_item)
            .collect::<Vec<_>>();
        let accepted = store.candidates(
            session.thread_id(),
            &turn,
            CompletionDrain::ExactAssignment,
            &input,
        );
        assert_eq!(accepted.len(), 1);
        store.accept(&accepted);
        assert!(!runtime.has_live_owned_work(session.thread_id()));
        assert_eq!(
            runtime
                .classify_wake_turn_end(&assignment, "automatic", TurnEndDisposition::Succeeded)
                .await,
            Ok(AssignmentPhase::Completed)
        );
        assert!(!store.has_ready(session.thread_id(), Some(&assignment)));
        assert!(!runtime.has_ready_owned_completions(&assignment));
        assert!(
            runtime
                .continue_owned_completion(&assignment, "extra")
                .is_err()
        );
        assert!(current_items(&store, &session, &turn).1.is_empty());
        // ItemCompleted can also emit its normal legacy terminal event.
        let remaining_events = std::iter::from_fn(|| rx_event.try_recv().ok()).collect::<Vec<_>>();
        assert!(remaining_events.len() <= 1);
        assert!(
            remaining_events
                .iter()
                .all(|event| matches!(event.msg, EventMsg::ExecCommandEnd(_)))
        );
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn service_result_is_bounded_and_retained_without_finite_assignment_ownership() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let runtime = &session.services.local_agent_runtime;
    let observation = runtime.work_observation();
    observation.subscribe();
    let service = runtime
        .reserve_owned_work(&session, &turn, "service", WorkKind::Service, None)
        .unwrap();
    service.mark_started();
    service.mark_yielded(true);
    let store = Arc::clone(&session.services.async_completions);
    let mut reservation = store
        .reserve_with_lifecycle(session.thread_id(), &turn, "service", 77, None, &service)
        .unwrap();
    reservation
        .register()
        .publish(CompletionStatus::Failed, &vec![b'x'; 10_000], 0);
    drop(reservation);
    service.mark_terminal();
    drop(service);
    assert!(runtime.has_live_owned_work(session.thread_id()));
    assert!(observation.snapshot().quiescent);
    let (claim, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 1);
    assert_eq!(
        header_field(&fragment_text(&items[0]), "work_kind"),
        "service"
    );
    assert!(serde_json::to_vec(&items[0].item).unwrap().len() <= MAX_FRAGMENT_BYTES);
    claim.recorded();
    store.accept(&[items[0].item.id().unwrap().clone()]);
    assert!(!runtime.has_live_owned_work(session.thread_id()));
}

#[tokio::test]
async fn release_and_retire_drop_attached_registrations_after_extracting_records() {
    for retire in [false, true] {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let runtime = &session.services.local_agent_runtime;
        let work = runtime
            .reserve_owned_work(&session, &turn, "release", WorkKind::Task, None)
            .unwrap();
        let store = Arc::clone(&session.services.async_completions);
        let mut reservation = reserve(&store, &session, &turn, "release").unwrap();
        reservation.attach_lifecycle(&work).unwrap();
        assert!(reservation.attach_lifecycle(&work).is_err());
        drop(work);
        assert!(runtime.has_live_owned_work(session.thread_id()));
        if retire {
            store.retire();
            drop(reservation);
        } else {
            reservation.release();
        }
        assert!(!runtime.has_live_owned_work(session.thread_id()));
        assert!(!store.has_owned_work());
    }
}

#[tokio::test]
async fn lifecycle_attachment_rejects_different_call_and_owner_before_reservation() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let work = session
        .services
        .local_agent_runtime
        .reserve_owned_work(&session, &turn, "exact", WorkKind::Task, None)
        .unwrap();
    let store = Arc::clone(&session.services.async_completions);
    assert!(
        store
            .reserve_with_lifecycle(session.thread_id(), &turn, "other-call", 1, None, &work)
            .is_err()
    );
    assert!(
        store
            .reserve_with_lifecycle(ThreadId::new(), &turn, "exact", 1, None, &work)
            .is_err()
    );
    assert!(!store.has_owned_work());
    let mut wrong = reserve(&store, &session, &turn, "other-call").unwrap();
    assert!(wrong.attach_lifecycle(&work).is_err());
    wrong.release();
    drop(work);
    assert!(
        !session
            .services
            .local_agent_runtime
            .has_live_owned_work(session.thread_id())
    );
}

#[tokio::test]
async fn collected_cell_identity_survives_acceptance_without_holding_work_or_delivery() {
    let (session, turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    let mut reservation = store
        .reserve_cell(session.thread_id(), &turn, "cell")
        .unwrap();
    reservation.bind_cell("generation:cell").unwrap();
    assert_eq!(store.collected_cell("generation:cell"), None);
    let publisher = reservation.register();
    publisher.publish(CompletionStatus::CellCompleted, b"terminal", 0);
    let id = store.collected_cell("generation:cell").unwrap();
    publisher.publish(CompletionStatus::CellCompleted, b"duplicate", 0);
    assert_eq!(store.collected_cell("generation:cell"), Some(id.clone()));
    assert_eq!(store.state().collected_cells.len(), 1);
    // Identity does not release the native-close fence or claim the terminal record.
    assert!(current_items(&store, &session, &turn).1.is_empty());
    drop(reservation);
    let (claim, items) = current_items(&store, &session, &turn);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].item.id(), Some(&id));
    claim.recorded();
    store.accept(&[id.clone()]);
    assert!(!store.has_owned_work());
    assert!(current_items(&store, &session, &turn).1.is_empty());
    assert_eq!(store.collected_cell("generation:cell"), Some(id));
    assert_eq!(store.collected_cell("unknown"), None);
    // Retained identity does not reserve capacity for new producers.
    let reservations = (0..MAX_RESERVATIONS)
        .map(|_| reserve(&store, &session, &turn, "new").unwrap())
        .collect::<Vec<_>>();
    drop(reservations);
    store.retire();
    assert_eq!(store.collected_cell("generation:cell"), None);
}

#[tokio::test]
async fn collected_cell_history_is_bounded_and_live_records_remain_discoverable() {
    let (session, turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    let mut live = store
        .reserve_cell(session.thread_id(), &turn, "live")
        .unwrap();
    live.bind_cell("live").unwrap();
    let publisher = live.register();
    publisher.publish(CompletionStatus::CellCompleted, b"live", 0);
    let live_id = store.collected_cell("live").unwrap();
    for index in 0..=MAX_RESERVATIONS {
        let cell_id = format!("cell-{index}");
        let mut reservation = store
            .reserve_cell(session.thread_id(), &turn, "cell")
            .unwrap();
        reservation.bind_cell(&cell_id).unwrap();
        reservation
            .register()
            .publish(CompletionStatus::CellCompleted, b"done", 0);
        let id = store.collected_cell(&cell_id).unwrap();
        drop(reservation);
        let (claim, items) = current_items(&store, &session, &turn);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.id(), Some(&id));
        claim.recorded();
        store.accept(&[id]);
    }
    assert_eq!(store.state().collected_cells.len(), MAX_RESERVATIONS);
    assert_eq!(store.collected_cell("cell-0"), None);
    assert!(
        store
            .collected_cell(&format!("cell-{MAX_RESERVATIONS}"))
            .is_some()
    );
    assert_eq!(store.collected_cell("live"), Some(live_id));
    drop(live);
    store.retire();
}

#[test_case::test_case(true, true; "accepted_old_record")]
#[test_case::test_case(false, true; "pending_old_record")]
#[test_case::test_case(false, false; "old_record_publishes_after_rebind")]
#[tokio::test]
async fn binding_a_reused_cell_id_clears_its_old_collected_identity(
    accept_first: bool,
    publish_first: bool,
) {
    let (session, turn) = make_session_and_context().await;
    let store = Arc::new(AsyncCompletions::default());
    let mut first = store
        .reserve_cell(session.thread_id(), &turn, "first")
        .unwrap();
    first.bind_cell("reused").unwrap();
    let first_publisher = first.register();
    let first_id = first.id.clone();
    if publish_first {
        first_publisher.publish(CompletionStatus::CellCompleted, b"first", 0);
        assert_eq!(store.collected_cell("reused"), Some(first_id.clone()));
    }
    drop(first);
    if accept_first {
        store.accept(&[first_id.clone()]);
    }
    let mut second = store
        .reserve_cell(session.thread_id(), &turn, "second")
        .unwrap();
    second.bind_cell("reused").unwrap();
    assert_eq!(store.collected_cell("reused"), None);
    first_publisher.publish(CompletionStatus::CellCompleted, b"first", 0);
    assert_eq!(store.collected_cell("reused"), None);
    let second_id = second.id.clone();
    second
        .register()
        .publish(CompletionStatus::CellCompleted, b"second", 0);
    assert_eq!(store.collected_cell("reused"), Some(second_id.clone()));
    drop(second);
    store.accept(&[second_id.clone()]);
    assert_eq!(store.collected_cell("reused"), Some(second_id));
    // Superseding identity does not remove the old cell's pending payload.
    assert_eq!(store.pending().len(), usize::from(!accept_first));
    store.accept(&[first_id]);
    assert!(!store.has_owned_work());
}
