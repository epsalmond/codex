use super::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn reasoning_content_preserves_omission_and_explicit_null_on_round_trip() {
    let omitted = json!({
        "type": "reasoning",
        "summary": [],
        "encrypted_content": "synthetic-reasoning"
    });
    let mut explicit_null = omitted.clone();
    explicit_null["content"] = serde_json::Value::Null;
    let mut visible_content = omitted.clone();
    visible_content["content"] = json!([
        {"type": "reasoning_text", "text": "synthetic visible reasoning"}
    ]);

    for wire_item in [omitted, explicit_null, visible_content] {
        let item: ResponseItem =
            serde_json::from_value(wire_item.clone()).expect("valid reasoning item");
        let round_trip = serde_json::to_value(item).expect("serializable reasoning item");
        assert_eq!(round_trip, wire_item);
    }
}
