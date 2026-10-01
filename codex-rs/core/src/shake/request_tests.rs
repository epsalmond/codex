use super::ShakeHistoryState;
use super::ShakeRequestCacheEpoch;
use super::ShakeRequestCheck;
use super::validate_shake_request_prefix_epoch;
use super::validate_shake_wire_prefix;
use codex_api::Reasoning;
use codex_api::ResponsesApiRequest;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::json;

fn message(text: &str) -> ResponseItem {
    serde_json::from_value(json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}]
    }))
    .expect("valid response item")
}

fn request(input: Vec<ResponseItem>) -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "gpt-test".to_string(),
        instructions: "instructions A".to_string(),
        input,
        tools: None,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: vec!["reasoning.encrypted_content".to_string()],
        service_tier: None,
        prompt_cache_key: Some("thread".to_string()),
        text: None,
        client_metadata: None,
        access_programs: None,
    }
}

fn check(prefix_items: Vec<ResponseItem>, epoch_id: &str) -> ShakeRequestCheck {
    let watermark = u64::try_from(prefix_items.len()).expect("test prefix length fits in u64");
    ShakeRequestCheck {
        history_state: ShakeHistoryState {
            epoch_id: epoch_id.to_string(),
            watermark,
            sealed_prefix_digest: "0".repeat(40),
        },
        prefix_items,
        base_instructions: "base instructions A".to_string(),
        provider_id: "openai".to_string(),
        input_modalities: Vec::new(),
        use_responses_lite: false,
    }
}

#[test]
fn request_prefix_must_extend_within_the_same_dynamic_cache_epoch() {
    let sealed = vec![message("sealed user"), message("sealed assistant")];
    let mut cache_epoch: Option<ShakeRequestCacheEpoch> = None;
    validate_shake_request_prefix_epoch(
        &mut cache_epoch,
        &request(sealed.clone()),
        Some(&check(sealed.clone(), "history-1")),
    )
    .expect("initial sealed prefix is accepted");

    let mut extended = sealed.clone();
    extended.push(message("new user turn"));
    validate_shake_request_prefix_epoch(
        &mut cache_epoch,
        &request(extended.clone()),
        Some(&check(sealed.clone(), "history-1")),
    )
    .expect("an append after the watermark preserves the baseline");

    let expanded_prefix = vec![
        sealed[0].clone(),
        sealed[1].clone(),
        message("unexpected item without watermark movement"),
    ];
    let mut expanded_check = check(expanded_prefix.clone(), "history-1");
    expanded_check.history_state.watermark = 2;
    assert!(
        validate_shake_request_prefix_epoch(
            &mut cache_epoch,
            &request(expanded_prefix),
            Some(&expanded_check),
        )
        .is_err()
    );

    let mut changed_prefix = extended.clone();
    changed_prefix[0] = message("changed sealed user");
    assert!(
        validate_shake_request_prefix_epoch(
            &mut cache_epoch,
            &request(changed_prefix),
            Some(&check(sealed, "history-1")),
        )
        .is_err()
    );
    assert_eq!(cache_epoch.as_ref().map(|epoch| epoch.watermark), Some(2));
}

#[test]
fn dynamic_request_changes_rotate_expectation_without_moving_history_watermark() {
    let sealed = vec![message("sealed user"), message("sealed assistant")];
    let mut cache_epoch: Option<ShakeRequestCacheEpoch> = None;
    validate_shake_request_prefix_epoch(
        &mut cache_epoch,
        &request(sealed.clone()),
        Some(&check(sealed.clone(), "history-1")),
    )
    .expect("initial sealed prefix is accepted");

    let mut changed_inputs = request(sealed.clone());
    changed_inputs.model = "gpt-next".to_string();
    changed_inputs.instructions = "instructions B".to_string();
    changed_inputs.reasoning = Some(Reasoning {
        effort: None,
        summary: None,
        context: None,
    });
    let mut changed_check = check(sealed, "history-1");
    changed_check.base_instructions = "base instructions B".to_string();
    validate_shake_request_prefix_epoch(&mut cache_epoch, &changed_inputs, Some(&changed_check))
        .expect("changed dynamic request inputs start a new cache expectation");
    assert_eq!(cache_epoch.as_ref().map(|epoch| epoch.watermark), Some(2));

    changed_inputs.input[1] = message("drift after request epoch changed");
    assert!(
        validate_shake_request_prefix_epoch(
            &mut cache_epoch,
            &changed_inputs,
            Some(&changed_check),
        )
        .is_err()
    );
}

#[test]
fn replaced_history_epoch_allows_a_new_prefix() {
    let first_prefix = vec![message("old sealed prefix")];
    let second_prefix = vec![message("new sealed prefix")];
    let mut cache_epoch: Option<ShakeRequestCacheEpoch> = None;
    validate_shake_request_prefix_epoch(
        &mut cache_epoch,
        &request(first_prefix.clone()),
        Some(&check(first_prefix, "history-1")),
    )
    .expect("initial sealed prefix is accepted");
    validate_shake_request_prefix_epoch(
        &mut cache_epoch,
        &request(second_prefix.clone()),
        Some(&check(second_prefix, "history-2")),
    )
    .expect("a replacement history epoch establishes a fresh prefix");
}

#[test]
fn canonical_request_bytes_ignore_nested_object_key_order() {
    let first: serde_json::Value =
        serde_json::from_str(r#"{"z":1,"nested":{"z":2,"a":3},"a":4}"#).expect("valid JSON");
    let second: serde_json::Value =
        serde_json::from_str(r#"{"a":4,"nested":{"a":3,"z":2},"z":1}"#).expect("valid JSON");

    assert_eq!(
        crate::shake::watermark::canonical_json_bytes(&first).unwrap(),
        crate::shake::watermark::canonical_json_bytes(&second).unwrap()
    );
}

#[test]
fn bounded_full_wire_input_must_preserve_the_sealed_prefix() {
    let sealed = vec![message("sealed prefix")];
    let request_check = check(sealed.clone(), "history-1");
    validate_shake_wire_prefix(&sealed, Some(&request_check))
        .expect("unchanged full wire prefix is accepted");
    assert!(
        validate_shake_wire_prefix(&[message("changed by bounding")], Some(&request_check))
            .is_err()
    );
}
