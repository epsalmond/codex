use super::*;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;

fn current_items(
    session: &Session,
    turn: &TurnContext,
) -> (CompletionClaim, Vec<ResponseItemEnvelope>) {
    session
        .services
        .async_completions
        .claim(session.thread_id(), turn)
}

fn reserve(
    session: &Session,
    turn: &TurnContext,
    call: &str,
) -> Result<CompletionReservation, &'static str> {
    session.services.async_completions.reserve(
        session.thread_id(),
        turn,
        call,
        /*process_id*/ 1,
        /*cell_id*/ None,
    )
}

#[rstest::rstest]
#[case(CompletionStatus::Exited(7))]
#[case(CompletionStatus::Failed)]
#[case(CompletionStatus::TimedOut)]
#[tokio::test]
async fn inline_and_initial_publication_races_have_one_consumer(#[case] status: CompletionStatus) {
    let (session, mut turn) = make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .enable(codex_features::Feature::AsyncProcessCompletion);
    let mut inline = reserve(&session, &turn, "inline").unwrap();
    inline.register().publish(
        CompletionStatus::Exited(0),
        b"inline",
        /*omitted_bytes*/ 0,
    );
    assert!(current_items(&session, &turn).1.is_empty());
    inline.release();
    assert!(current_items(&session, &turn).1.is_empty());
    let mut yielded = reserve(&session, &turn, "yielded").unwrap();
    let producer = yielded.register();
    producer.publish(status, b"first", /*omitted_bytes*/ 0);
    producer.publish(
        CompletionStatus::Failed,
        b"duplicate",
        /*omitted_bytes*/ 0,
    );
    assert!(current_items(&session, &turn).1.is_empty());
    drop(yielded);
    let (claim, items) = current_items(&session, &turn);
    assert_eq!(items.len(), 1);
    assert!(current_items(&session, &turn).1.is_empty());
    drop(claim);
    let (claim, retry) = current_items(&session, &turn);
    assert_eq!(items, retry);
    drop(claim);
    record_ready(&session, &turn, &turn.capture_current_model_info()).await;
    record_ready(&session, &turn, &turn.capture_current_model_info()).await;
    assert_eq!(
        session
            .clone_history()
            .await
            .for_prompt(&turn.capture_current_model_info().input_modalities)
            .len(),
        1
    );
    assert!(current_items(&session, &turn).1.is_empty());
}

#[tokio::test]
async fn admission_and_claims_keep_capacity_until_recorded() {
    let (session, turn) = make_session_and_context().await;
    for _ in 0..64 {
        let mut reservation = reserve(&session, &turn, "finite").unwrap();
        reservation.register().publish(
            CompletionStatus::Exited(0),
            b"done",
            /*omitted_bytes*/ 0,
        );
    }
    assert!(reserve(&session, &turn, "rejected").is_err());
    let (claim, items) = current_items(&session, &turn);
    assert_eq!(items.len(), 8);
    assert!(reserve(&session, &turn, "in-flight").is_err());
    claim.recorded();
    assert!(reserve(&session, &turn, "accepted").is_ok());
}

#[tokio::test]
async fn terminal_tail_is_bounded_and_assignment_replacements_are_held() {
    let (session, turn) = make_session_and_context().await;
    let assignment = AgentAssignmentId {
        thread_id: session.thread_id(),
        generation: Uuid::new_v4(),
    };
    turn.agent_assignment.set(assignment).unwrap();
    let mut reservation = reserve(&session, &turn, "cancelled").unwrap();
    reservation.register().publish(
        CompletionStatus::Cancelled,
        "世界".repeat(1000).as_bytes(),
        /*omitted_bytes*/ 99,
    );
    drop(reservation);
    let (claim, items) = current_items(&session, &turn);
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
    assert!(current_items(&session, &replacement).1.is_empty());
    assert_eq!(current_items(&session, &turn).1, items);
}

#[tokio::test]
async fn exhausted_admission_rejects_the_command_before_its_side_effect() {
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::ExecCommandHandler;
    use crate::tools::registry::ToolExecutor;
    let (session, mut turn) = make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .enable(codex_features::Feature::AsyncProcessCompletion);
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    for _ in 0..64 {
        let mut reservation = reserve(&session, &turn, "held").unwrap();
        reservation.register().publish(
            CompletionStatus::Exited(0),
            b"done",
            /*omitted_bytes*/ 0,
        );
    }
    let marker = turn.config.cwd.join("rejected-completion-marker");
    let result = ExecCommandHandler::default()
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn),
            step_context: crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
            call_id: "rejected".to_owned(),
            tool_name: codex_tools::ToolName::plain("exec_command"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: serde_json::json!({
                    "cmd": "echo side-effect > rejected-completion-marker", "tty": false,
                })
                .to_string(),
            },
        })
        .await;
    let Err(crate::function_tool::FunctionCallError::RespondToModel(message)) = result else {
        panic!("expected capacity rejection");
    };
    assert!(message.contains("capacity exhausted"));
    assert!(!marker.exists());
    session.services.async_completions.retire();
    assert!(reserve(&session, &turn, "closed").is_err());
    assert!(current_items(&session, &turn).1.is_empty());
}
