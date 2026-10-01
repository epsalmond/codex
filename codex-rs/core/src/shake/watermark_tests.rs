use super::*;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
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

fn image_message(image_url: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "sealed image".to_string(),
            },
            ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: image_url.to_string(),
                },
                detail: None,
            },
        ],
        phase: None,
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

#[test]
fn media_preparation_migration_starts_a_new_epoch_but_corrupt_seals_stay_invalid() {
    let legacy_history = vec![image_message("https://example.invalid/legacy.png")];
    let valid_state = state_for_epoch("legacy-epoch".to_string(), 1, &legacy_history)
        .expect("legacy history has a valid seal");
    let prepared_history = vec![ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "sealed image".to_string(),
            },
            ContentItem::InputText {
                text: "remote image URLs are not supported".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })];

    let (migrated_state, migrated) =
        rebase_after_media_preparation(valid_state.clone(), true, &prepared_history);
    assert!(migrated);
    assert_ne!(migrated_state.epoch_id, valid_state.epoch_id);
    assert_eq!(migrated_state.watermark, 0);
    assert!(matches_history(&migrated_state, &prepared_history));

    let mut corrupt_state = valid_state;
    corrupt_state.sealed_prefix_digest = "0".repeat(SHA1_HEX_LENGTH);
    assert!(!matches_history(&corrupt_state, &legacy_history));
    let (preserved_state, migrated) =
        rebase_after_media_preparation(corrupt_state.clone(), false, &prepared_history);
    assert!(!migrated);
    assert_eq!(preserved_state, corrupt_state);
    assert!(!matches_history(&preserved_state, &prepared_history));
}

#[test]
fn sealed_boundary_moves_before_a_split_or_open_tool_call() {
    let items = vec![function_call("call-1"), function_output("call-1")];

    assert_eq!(close_over_tool_calls(&items, 1), 0);
    assert_eq!(close_over_tool_calls(&items, 2), 2);
    assert_eq!(close_over_tool_calls(&items[..1], 1), 0);
}

#[test]
fn unpaired_output_does_not_block_a_boundary() {
    let items = vec![function_output("external-call")];
    assert_eq!(close_over_tool_calls(&items, 1), 1);
}
