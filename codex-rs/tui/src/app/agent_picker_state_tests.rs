use super::*;
use pretty_assertions::assert_eq;

#[test]
fn stale_backfills_cannot_replace_live_data_or_a_new_root_request() {
    let mut state = AgentPickerState::default();
    let root = ThreadId::new();
    let other_root = ThreadId::new();
    let child = ThreadId::new();
    let generation = state.ensure_preview_generation(root);
    let revision = state.begin_preview_backfill(child).unwrap();
    state.set_response_preview(child, Some("live answer".to_string()));
    state.finish_preview_backfill(
        root,
        generation,
        child,
        revision,
        Ok(Some("older answer".to_string())),
    );
    assert_eq!(
        state.details(&child).unwrap().response_preview.as_deref(),
        Some("live answer")
    );

    state.clear_details(child);
    let old_generation = state.ensure_preview_generation(root);
    let old_revision = state.begin_preview_backfill(child).unwrap();
    let new_generation = state.ensure_preview_generation(other_root);
    let new_revision = state.begin_preview_backfill(child).unwrap();
    state.finish_preview_backfill(
        root,
        old_generation,
        child,
        old_revision,
        Ok(Some("from old root".to_string())),
    );
    assert_eq!(state.begin_preview_backfill(child), None);
    state.finish_preview_backfill(
        other_root,
        new_generation,
        child,
        new_revision,
        Ok(Some("current answer".to_string())),
    );
    assert_eq!(
        state.details(&child).unwrap().response_preview.as_deref(),
        Some("current answer")
    );
}

#[test]
fn removing_and_readding_a_thread_invalidates_old_preview_and_status_replies() {
    let mut state = AgentPickerState::default();
    let root = ThreadId::new();
    let child = ThreadId::new();
    state.start_thread(child);
    let generation = state.ensure_preview_generation(root);
    let old_revision = state.begin_preview_backfill(child).unwrap();
    let old_status_snapshot = state.status_revision_snapshot();

    state.clear_thread(child);
    state.start_thread(child);
    let new_revision = state.begin_preview_backfill(child).unwrap();
    assert!(!state.status_revision_matches(child, &old_status_snapshot));

    state.finish_preview_backfill(
        root,
        generation,
        child,
        old_revision,
        Ok(Some("stale answer".to_string())),
    );
    assert_eq!(state.begin_preview_backfill(child), None);
    state.finish_preview_backfill(
        root,
        generation,
        child,
        new_revision,
        Ok(Some("current answer".to_string())),
    );
    assert_eq!(
        state.details(&child).unwrap().response_preview.as_deref(),
        Some("current answer")
    );
}

#[test]
fn live_status_revisions_reject_older_discovery_state() {
    let mut state = AgentPickerState::default();
    let child = ThreadId::new();
    let snapshot = state.status_revision_snapshot();

    state.bump_status_revision(child);
    assert!(!state.status_revision_matches(child, &snapshot));

    let after_live_error = state.status_revision_snapshot();
    state.set_error(child, /*is_error*/ true);
    assert!(!state.status_revision_matches(child, &after_live_error));

    let after_error = state.status_revision_snapshot();
    state.set_error_without_revision(child, /*is_error*/ false);
    assert!(state.status_revision_matches(child, &after_error));
    assert!(!state.details(&child).unwrap().is_error);
}

#[test]
fn empty_and_failed_preview_attempts_retry_only_on_a_new_trigger() {
    let mut state = AgentPickerState::default();
    let root = ThreadId::new();
    let child = ThreadId::new();
    let generation = state.ensure_preview_generation(root);
    let revision = state.begin_preview_backfill(child).unwrap();
    state.finish_preview_backfill(root, generation, child, revision, Ok(None));
    assert_eq!(state.begin_preview_backfill(child), None);

    assert!(state.note_completed_preview_turn(child, "turn-1", /*preview*/ None));
    let revision = state.begin_preview_backfill(child).unwrap();
    state.finish_preview_backfill(
        root,
        generation,
        child,
        revision,
        Err("temporary failure".to_string()),
    );
    assert_eq!(state.begin_preview_backfill(child), None);

    state.start_preview_generation(root);
    assert!(state.begin_preview_backfill(child).is_some());
}

#[test]
fn completion_without_new_response_preserves_an_eligible_preview() {
    let mut state = AgentPickerState::default();
    let child = ThreadId::new();
    state.set_response_preview(child, Some("earlier answer".to_string()));

    assert!(!state.note_completed_preview_turn(child, "turn-2", /*preview*/ None));
    assert_eq!(
        state.details(&child).unwrap().response_preview.as_deref(),
        Some("earlier answer")
    );
}

#[test]
fn revert_invalidates_pending_preview_and_clears_cached_context() {
    let mut state = AgentPickerState::default();
    let root = ThreadId::new();
    let child = ThreadId::new();
    let generation = state.ensure_preview_generation(root);
    let revision = state.begin_preview_backfill(child).unwrap();
    state.set_context_usage(
        child,
        Some(AgentPickerContextUsage {
            last_tokens: 42,
            model_context_window: Some(/*value*/ 100),
        }),
    );

    state.clear_details(child);
    state.finish_preview_backfill(
        root,
        generation,
        child,
        revision,
        Ok(Some("stale answer".to_string())),
    );

    let details = state.details(&child).unwrap();
    assert_eq!(details.response_preview, None);
    assert_eq!(details.context_usage, None);
}

#[test]
fn revert_does_not_restore_a_duplicate_pre_revert_completion() {
    let mut state = AgentPickerState::default();
    let child = ThreadId::new();
    assert!(!state.note_completed_preview_turn(child, "turn-1", Some("old answer".to_string()),));

    state.clear_details(child);

    assert!(!state.note_completed_preview_turn(child, "turn-1", Some("old answer".to_string()),));
    assert_eq!(state.details(&child).unwrap().response_preview, None);
}
