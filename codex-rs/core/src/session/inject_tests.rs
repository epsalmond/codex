use crate::session::tests::make_session_and_context_with_rx;
use crate::session::tests::raw_history_items;
use crate::state::ActiveTurn;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::models::ConfigurationReasoning;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use core_test_support::responses::strip_metadata_from_items;
use core_test_support::responses::strip_response_item_ids;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn inject_no_new_turn_preserves_history_when_a_reserved_turn_is_aborted() {
    let (session, turn_context, _rx_event) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "context injected during turn reservation".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    session
        .inject_no_new_turn(vec![item.clone()], Some(&turn_context))
        .await;
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;

    assert!(session.active_turn.lock().await.is_none());
    let history = session.clone_history().await;
    assert_eq!(
        strip_response_item_ids(&strip_metadata_from_items(&raw_history_items(&history))),
        strip_response_item_ids(&strip_metadata_from_items(&[item])),
    );
}

#[tokio::test]
async fn harness_authored_configuration_updates_preserve_metadata_and_resume() {
    let (session, turn_context, rx_event) = make_session_and_context_with_rx().await;
    assert!(!session.enabled(Feature::RetainClientDeveloperMessages));

    let mut expected = ResponseItemEnvelope {
        item: ResponseItem::ConfigurationUpdate {
            reasoning: ConfigurationReasoning {
                effort: ReasoningEffort::High,
            },
        },
        metadata: Some(CodexHarnessMetadata {
            harness_authored_configuration: true,
            ..Default::default()
        }),
    };
    session
        .record_annotated_conversation_items(
            &turn_context,
            turn_context.model_info(),
            vec![expected.clone()],
        )
        .await;

    expected.metadata.as_mut().unwrap().mcp_attribution = Some(
        session
            .services
            .executed_tool_calls
            .mcp_attribution_snapshot(),
    );

    let recorded = session.clone_history().await.into_annotated_items();
    assert_eq!(recorded, vec![expected.clone()]);
    let mut raw_items = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::RawResponseItem(event) = event.msg {
            raw_items.push(event.item);
        }
    }
    assert_eq!(raw_items, vec![expected.item]);

    let rollout_items = recorded
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(reconstructed.history, recorded);
}
