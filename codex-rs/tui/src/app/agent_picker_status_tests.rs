use super::*;
use codex_app_server_protocol::TurnItemsView;
use pretty_assertions::assert_eq;

fn agent_message(text: &str, phase: Option<MessagePhase>) -> ThreadItem {
    ThreadItem::AgentMessage {
        id: text.to_string(),
        text: text.to_string(),
        phase,
        memory_citation: None,
        delivery: None,
        questions: None,
    }
}

fn turn(id: &str, status: TurnStatus, items: Vec<ThreadItem>) -> Turn {
    Turn {
        id: id.to_string(),
        items,
        items_view: TurnItemsView::Full,
        status,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

#[test]
fn response_preview_prefers_final_then_legacy_and_skips_commentary() {
    let final_precedence_turn = turn(
        "turn",
        TurnStatus::Completed,
        vec![
            agent_message("first final", Some(MessagePhase::FinalAnswer)),
            agent_message("commentary", Some(MessagePhase::Commentary)),
            agent_message("legacy after final", /*phase*/ None),
        ],
    );
    assert_eq!(
        response_preview_for_turn(&final_precedence_turn),
        Some("first final".to_string())
    );

    let legacy = turn(
        "legacy",
        TurnStatus::Completed,
        vec![
            agent_message("earlier", /*phase*/ None),
            agent_message("commentary", Some(MessagePhase::Commentary)),
            agent_message("latest legacy", /*phase*/ None),
        ],
    );
    assert_eq!(
        response_preview_for_turn(&legacy),
        Some("latest legacy".to_string())
    );

    let commentary_after_legacy = turn(
        "commentary-after-legacy",
        TurnStatus::Completed,
        vec![
            agent_message("legacy draft", /*phase*/ None),
            agent_message("newer commentary", Some(MessagePhase::Commentary)),
        ],
    );
    assert_eq!(response_preview_for_turn(&commentary_after_legacy), None);
}

#[test]
fn response_preview_uses_newest_completed_turn_with_an_eligible_message() {
    let turns = vec![
        turn(
            "active",
            TurnStatus::InProgress,
            vec![agent_message(
                "active draft",
                Some(MessagePhase::FinalAnswer),
            )],
        ),
        turn(
            "new",
            TurnStatus::Interrupted,
            vec![agent_message("new answer", Some(MessagePhase::FinalAnswer))],
        ),
        turn("empty", TurnStatus::Failed, Vec::new()),
        turn(
            "old",
            TurnStatus::Completed,
            vec![agent_message("old answer", Some(MessagePhase::FinalAnswer))],
        ),
    ];
    assert_eq!(
        latest_completed_response_preview(&turns),
        Some("new answer".to_string())
    );

    let newer_turn_without_response = vec![
        turn("empty", TurnStatus::Failed, Vec::new()),
        turn(
            "old",
            TurnStatus::Completed,
            vec![agent_message(
                "old eligible answer",
                Some(MessagePhase::FinalAnswer),
            )],
        ),
    ];
    assert_eq!(
        latest_completed_response_preview(&newer_turn_without_response),
        Some("old eligible answer".to_string())
    );
}

#[test]
fn newer_legacy_response_precedes_an_older_explicit_final() {
    let turns = vec![
        turn(
            "newer-legacy",
            TurnStatus::Completed,
            vec![agent_message("newer legacy", /*phase*/ None)],
        ),
        turn(
            "older-final",
            TurnStatus::Completed,
            vec![agent_message(
                "older final",
                Some(MessagePhase::FinalAnswer),
            )],
        ),
    ];

    assert_eq!(
        latest_completed_response_preview(&turns),
        Some("newer legacy".to_string())
    );
}

#[test]
fn response_preview_normalizes_and_bounds_unicode_text() {
    let normalized = turn(
        "spaces",
        TurnStatus::Completed,
        vec![agent_message(
            "  hello\n\tworld  ",
            Some(MessagePhase::FinalAnswer),
        )],
    );
    assert_eq!(
        response_preview_for_turn(&normalized),
        Some("hello world".to_string())
    );

    let long = turn(
        "long",
        TurnStatus::Completed,
        vec![agent_message(
            &"界".repeat(/*n*/ 513),
            Some(MessagePhase::FinalAnswer),
        )],
    );
    let preview = response_preview_for_turn(&long).expect("preview");
    assert_eq!(preview.chars().count(), MAX_AGENT_PICKER_PREVIEW_CHARS);
    assert!(preview.ends_with('…'));
}

#[test]
fn context_description_uses_observed_last_count_and_rounds_percent() {
    assert_eq!(context_description(/*context*/ None), "context ?");
    assert_eq!(
        context_description(Some(&AgentPickerContextUsage {
            last_tokens: 0,
            model_context_window: Some(/*value*/ 100),
        })),
        "context 0 / 100 (0%)"
    );
    assert_eq!(
        context_description(Some(&AgentPickerContextUsage {
            last_tokens: 126,
            model_context_window: Some(/*value*/ 100),
        })),
        "context 126 / 100 (126%)"
    );
    assert_eq!(
        context_description(Some(&AgentPickerContextUsage {
            last_tokens: 42,
            model_context_window: Some(/*value*/ 0),
        })),
        "context 42 / ?"
    );
    assert_eq!(
        context_description(Some(&AgentPickerContextUsage {
            last_tokens: -1,
            model_context_window: Some(/*value*/ 100),
        })),
        "context ?"
    );
}

#[test]
fn picker_status_prioritizes_closed_and_running_states() {
    assert_eq!(picker_status_label(false, true, true), "mid-turn");
    assert_eq!(picker_status_label(false, false, true), "error");
    assert_eq!(picker_status_label(false, false, false), "idle");
    assert_eq!(picker_status_label(true, false, true), "closed");
}
