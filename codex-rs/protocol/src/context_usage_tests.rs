use super::AgentContextUsage;
use crate::protocol::EventMsg;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn legacy_token_events_without_context_round_trip_without_manufactured_metadata() {
    let legacy = json!({"type": "token_count", "info": null, "rate_limits": null});
    for context_usage in [None, Some(serde_json::Value::Null)] {
        let mut saved = legacy.clone();
        if let Some(context_usage) = context_usage {
            saved["context_usage"] = context_usage;
        }
        let event: EventMsg = serde_json::from_value(saved).expect("legacy event");
        assert!(matches!(&event, EventMsg::TokenCount(event) if event.context_usage.is_none()));
        assert_eq!(serde_json::to_value(event).expect("event JSON"), legacy);
    }
}

#[test]
fn older_context_objects_keep_unknown_policy_and_times_unavailable() {
    let legacy = json!({"active_tokens": 42, "basis": "estimate", "last_reduction": null});
    let context: AgentContextUsage = serde_json::from_value(legacy.clone()).expect("legacy context");
    assert_eq!(serde_json::to_value(&context).expect("context JSON"), legacy);
    assert_eq!((context.observed_at, context.provider_usage_at, context.selected_model), (None, None, None));
}
