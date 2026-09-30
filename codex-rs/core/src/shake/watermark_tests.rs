use super::*;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn function_call(call_id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "shell".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn function_output(call_id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text("done".to_string()),
        internal_chat_message_metadata_passthrough: None,
    })
}

#[test]
fn history_state_binds_only_the_sealed_prefix() {
    let mut items = vec![function_call("call-1"), function_output("call-1")];
    let state = state_for_epoch("epoch-1".to_string(), 1, &items).expect("valid watermark");

    assert!(matches_history(&state, &items));
    items[1] = function_output("call-2");
    assert!(matches_history(&state, &items));
    items[0] = function_call("call-2");
    assert!(!matches_history(&state, &items));
}

#[test]
fn history_digest_is_stable_when_checkpoint_fills_default_metadata_sidecars() {
    let without_metadata = function_call("call-1");
    let mut with_default_metadata = without_metadata.clone();
    with_default_metadata.metadata = Some(codex_history::CodexHarnessMetadata::default());

    assert_eq!(
        history_prefix_digest(&[without_metadata]).expect("serializable envelope"),
        history_prefix_digest(&[with_default_metadata]).expect("serializable envelope")
    );
}

#[test]
fn history_state_rejects_a_watermark_outside_the_history() {
    let items = vec![function_call("call-1")];

    assert!(state_for_epoch("epoch-1".to_string(), 2, &items).is_err());
}
