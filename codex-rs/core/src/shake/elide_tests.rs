use super::shake_elide_watermarked;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

#[test]
fn failed_output_save_stops_before_its_call() {
    let items = vec![
        ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            encrypted_function_args: None,
            call_id: "call-1".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }),
        ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call-1".to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text("large output ".repeat(2_000)),
            internal_chat_message_metadata_passthrough: None,
        }),
    ];
    let mut attempted = items.clone();
    let (result, watermark) = shake_elide_watermarked(
        &mut attempted,
        /*start*/ 0,
        /*requested_end*/ items.len(),
        /*protect_tokens*/ 0,
        &mut |_, _| None,
    );

    assert!(result.is_noop());
    assert_eq!(watermark, 0);
    assert_eq!(attempted, items);
}
