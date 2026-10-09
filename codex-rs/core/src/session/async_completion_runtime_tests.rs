use super::*;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;

enum Rejection {
    Capacity,
    Metadata,
}

#[test_case::test_case(Rejection::Capacity; "capacity")]
#[test_case::test_case(Rejection::Metadata; "metadata")]
#[tokio::test]
async fn native_admission_rejects_before_side_effects(rejection: Rejection) {
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::ExecCommandHandler;
    use crate::tools::registry::ToolExecutor;
    let (session, mut turn) = make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .enable(codex_features::Feature::AsyncProcessCompletion);
    let (call_id, expected) = match rejection {
        Rejection::Capacity => ("rejected".to_owned(), "capacity exhausted"),
        Rejection::Metadata => {
            turn.sub_id = "\n".repeat(128);
            ("\"".repeat(128), "metadata exceeds")
        }
    };
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    for _ in 0..if matches!(rejection, Rejection::Capacity) {
        64
    } else {
        0
    } {
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
            call_id,
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
    assert!(message.contains(expected));
    assert!(!marker.exists());
}

#[tokio::test]
async fn history_append_is_retained_until_model_acceptance() {
    let (session, mut turn) = make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .enable(codex_features::Feature::AsyncProcessCompletion);
    turn.sub_id = "quoted\"turn\\\n".repeat(3);
    let original_turn = turn.sub_id.clone();
    let store = &session.services.async_completions;
    let mut yielded = store
        .reserve(
            session.thread_id(),
            &turn,
            "yielded",
            /*process_id*/ 1,
            /*cell_id*/ None,
        )
        .unwrap();
    yielded.register().publish(
        CompletionStatus::Exited(0),
        "\n\"\\世界".repeat(1000).as_bytes(),
        /*omitted_bytes*/ 0,
    );
    drop(yielded);
    let (claim, expected) = store.claim(session.thread_id(), &turn, &[]);
    drop(claim);
    let expected = expected
        .into_iter()
        .map(ResponseItemEnvelope::into_item)
        .collect::<Vec<_>>();
    turn.sub_id = "later-owner-turn".to_owned();
    assert!(record_ready(&session, &turn, &turn.capture_current_model_info()).await);
    assert!(!record_ready(&session, &turn, &turn.capture_current_model_info()).await);
    let history = session
        .clone_history()
        .await
        .for_prompt(&turn.capture_current_model_info().input_modalities);
    assert_eq!(history, expected);
    assert_eq!(history[0].turn_id(), Some(original_turn.as_str()));
    assert!(serde_json::to_vec(&history[0]).unwrap().len() <= MAX_FRAGMENT_BYTES);
    let ids = store.candidates(session.thread_id(), &turn, &history);
    assert_eq!(ids.len(), 1);
    store.accept(&ids);
    assert!(store.claim(session.thread_id(), &turn, &[]).1.is_empty());
}
