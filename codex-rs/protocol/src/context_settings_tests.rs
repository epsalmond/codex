use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn sparse_context_choices_have_unambiguous_tagged_thresholds() {
    let state: ContextSettingsState = serde_json::from_value(json!({
        "overrides": { "shake_threshold": { "type": "off" },
            "compaction_threshold": { "type": "modelDefault" } }
    }))
    .unwrap();
    assert_eq!(
        state,
        ContextSettingsState {
            overrides: ContextSettingsOverrides {
                shake_threshold: Some(ShakeThreshold::Off),
                compaction_threshold: Some(CompactionThreshold::ModelDefault),
                ..Default::default()
            },
            inherited: None
        }
    );
    assert_eq!(
        serde_json::to_value(ShakeThreshold::Percent { percent: 40 }).unwrap(),
        json!({"type":"percent","percent":40})
    );
    assert_eq!(
        serde_json::to_value(CompactionThreshold::Tokens { tokens: 123 }).unwrap(),
        json!({"type":"tokens","tokens":123})
    );
    assert_eq!(
        serde_json::from_value::<ContextSettingsState>(json!({})).unwrap(),
        ContextSettingsState::default()
    );
}
