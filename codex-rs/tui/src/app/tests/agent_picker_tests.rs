use super::*;
use crate::chatwidget::tests::helpers::render_bottom_popup;
use crate::multi_agents::AgentPickerContextUsage;
use crate::multi_agents::AgentPickerThreadEntry;
use codex_protocol::models::MessagePhase;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn child_notifications_cache_last_context_and_final_response_while_hidden() -> Result<()> {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    app.upsert_agent_picker_thread(
        thread_id,
        Some("Review".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );

    let mut usage_notification = token_usage_notification(thread_id, "turn-1", Some(/*value*/ 100));
    let ServerNotification::ThreadTokenUsageUpdated(usage) = &mut usage_notification else {
        unreachable!();
    };
    usage.token_usage.total.total_tokens = 900;
    usage.token_usage.last.total_tokens = 0;
    app.enqueue_thread_notification(thread_id, usage_notification)
        .await?;
    assert_eq!(
        app.agent_navigation
            .picker_details(&thread_id)
            .and_then(|details| details.context_usage.as_ref()),
        Some(&AgentPickerContextUsage {
            last_tokens: 0,
            model_context_window: Some(/*value*/ 100),
        })
    );

    let mut completed_notification =
        turn_completed_notification(thread_id, "turn-1", TurnStatus::Completed);
    let ServerNotification::TurnCompleted(completed) = &mut completed_notification else {
        unreachable!();
    };
    completed.turn.items = vec![
        ThreadItem::AgentMessage {
            id: "answer".to_string(),
            text: "  review\n complete  ".to_string(),
            phase: Some(MessagePhase::FinalAnswer),
            memory_citation: None,
            delivery: None,
            questions: None,
        },
        ThreadItem::AgentMessage {
            id: "commentary".to_string(),
            text: "still working".to_string(),
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            delivery: None,
            questions: None,
        },
    ];
    app.enqueue_thread_notification(thread_id, completed_notification)
        .await?;

    let details = app.agent_navigation.picker_details(&thread_id).unwrap();
    assert_eq!(details.response_preview.as_deref(), Some("review complete"));
    assert_eq!(details.context_usage.as_ref().unwrap().last_tokens, 0);
    assert_eq!(
        app.agent_navigation.get(&thread_id),
        Some(&AgentPickerThreadEntry {
            agent_nickname: Some("Review".to_string()),
            agent_role: Some("worker".to_string()),
            agent_path: None,
            is_running: false,
            is_closed: false,
        })
    );

    app.enqueue_thread_notification(
        thread_id,
        ServerNotification::ThreadReverted(codex_app_server_protocol::ThreadRevertedNotification {
            thread_id: thread_id.to_string(),
        }),
    )
    .await?;
    let details = app.agent_navigation.picker_details(&thread_id).unwrap();
    assert_eq!(details.response_preview, None);
    assert_eq!(details.context_usage, None);
    Ok(())
}

#[tokio::test]
async fn hidden_completion_and_token_updates_do_not_schedule_preview_reads() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let root_id = ThreadId::new();
    let child_id = ThreadId::new();
    app.primary_thread_id = Some(root_id);
    app.upsert_agent_picker_thread(
        child_id,
        /*agent_nickname*/ None,
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(/*selected*/ None));
    app.chat_widget.show_selection_view(SelectionViewParams {
        view_id: Some("covering-picker"),
        ..Default::default()
    });
    assert!(
        app.chat_widget
            .selected_index_for_present_view("agent-picker")
            .is_some()
    );
    assert_eq!(app.chat_widget.active_view_id(), Some("covering-picker"));

    app.enqueue_thread_notification(
        child_id,
        token_usage_notification(child_id, "turn-1", Some(/*value*/ 100)),
    )
    .await?;
    app.enqueue_thread_notification(
        child_id,
        turn_completed_notification(child_id, "turn-1", TurnStatus::Completed),
    )
    .await?;

    assert!(matches!(
        app_event_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(
        app.agent_navigation
            .picker_details(&child_id)
            .is_some_and(|details| details.response_preview.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn visible_picker_refreshes_a_nonselected_child_row_in_place() -> Result<()> {
    let mut app = make_test_app().await;
    let selected_id = ThreadId::from_u128(/*value*/ 10);
    let changed_id = ThreadId::from_u128(/*value*/ 11);
    app.active_thread_id = Some(selected_id);
    for (thread_id, name) in [(selected_id, "Selected"), (changed_id, "Other")] {
        app.upsert_agent_picker_thread(
            thread_id,
            Some(name.to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
        );
    }
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(/*selected*/ None));
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
    let before = render_bottom_popup(&app.chat_widget, /*width*/ 96);
    assert!(before.contains("Selected") && before.contains("Other"));
    assert_eq!(
        app.chat_widget
            .selected_index_for_present_view("agent-picker"),
        Some(/*value*/ 1)
    );

    let mut usage_notification =
        token_usage_notification(changed_id, "turn-2", Some(/*value*/ 100));
    let ServerNotification::ThreadTokenUsageUpdated(usage) = &mut usage_notification else {
        unreachable!();
    };
    usage.token_usage.total.total_tokens = 900;
    usage.token_usage.last.total_tokens = 42;
    app.enqueue_thread_notification(changed_id, usage_notification)
        .await?;

    let after = render_bottom_popup(&app.chat_widget, /*width*/ 96);
    assert!(after.contains("context 42 / 100 (42%)"));
    assert_eq!(
        app.chat_widget
            .selected_index_for_present_view("agent-picker"),
        Some(/*value*/ 1)
    );
    assert!(after.contains("Selected") && after.contains("Other"));
    assert!(after.contains("context 42 / 100 (42%)"));

    let mut compacted_usage = token_usage_notification(changed_id, "turn-2", Some(/*value*/ 100));
    let ServerNotification::ThreadTokenUsageUpdated(usage) = &mut compacted_usage else {
        unreachable!();
    };
    usage.token_usage.total.total_tokens = 1_500;
    usage.token_usage.last.total_tokens = 12;
    app.enqueue_thread_notification(changed_id, compacted_usage)
        .await?;
    let after_compaction = render_bottom_popup(&app.chat_widget, /*width*/ 96);
    assert!(after_compaction.contains("context 12 / 100 (12%)"));
    assert_eq!(
        app.chat_widget
            .selected_index_for_present_view("agent-picker"),
        Some(/*value*/ 1)
    );
    Ok(())
}

#[tokio::test]
async fn child_failure_and_close_update_picker_status() -> Result<()> {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    app.upsert_agent_picker_thread(
        thread_id,
        /*agent_nickname*/ None,
        Some("worker".to_string()),
        /*is_closed*/ false,
    );

    app.enqueue_thread_notification(thread_id, turn_started_notification(thread_id, "turn-1"))
        .await?;
    assert!(app.agent_navigation.get(&thread_id).unwrap().is_running);

    app.enqueue_thread_notification(
        thread_id,
        turn_completed_notification(thread_id, "turn-1", TurnStatus::Failed),
    )
    .await?;
    assert!(!app.agent_navigation.get(&thread_id).unwrap().is_running);
    assert!(
        app.agent_navigation
            .picker_details(&thread_id)
            .unwrap()
            .is_error
    );

    app.enqueue_thread_notification(thread_id, thread_closed_notification(thread_id))
        .await?;
    assert!(app.agent_navigation.get(&thread_id).unwrap().is_closed);
    assert!(
        !app.agent_navigation
            .picker_details(&thread_id)
            .unwrap()
            .is_error
    );
    Ok(())
}

#[tokio::test]
async fn not_loaded_status_does_not_close_a_live_picker_thread() -> Result<()> {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    app.upsert_agent_picker_thread(
        thread_id,
        /*agent_nickname*/ None,
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.ensure_thread_channel(thread_id);

    app.enqueue_thread_notification(
        thread_id,
        ServerNotification::ThreadStatusChanged(
            codex_app_server_protocol::ThreadStatusChangedNotification {
                thread_id: thread_id.to_string(),
                status: codex_app_server_protocol::ThreadStatus::NotLoaded,
            },
        ),
    )
    .await?;

    assert!(!app.agent_navigation.get(&thread_id).unwrap().is_closed);
    Ok(())
}

#[tokio::test]
async fn picker_rows_show_status_context_and_preview_at_normal_and_narrow_widths() -> Result<()> {
    let mut app = make_test_app().await;
    let idle_id = ThreadId::from_u128(/*value*/ 1);
    let running_id = ThreadId::from_u128(/*value*/ 2);
    let closed_id = ThreadId::from_u128(/*value*/ 4);
    let error_id = ThreadId::from_u128(/*value*/ 5);
    app.primary_thread_id = Some(ThreadId::from_u128(/*value*/ 3));
    app.active_thread_id = Some(idle_id);
    app.upsert_agent_picker_thread(
        idle_id,
        Some("Planner".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.agent_navigation.set_context_usage(
        idle_id,
        Some(AgentPickerContextUsage {
            last_tokens: 4_800,
            model_context_window: Some(/*value*/ 32_000),
        }),
    );
    app.agent_navigation.set_response_preview(
        idle_id,
        Some("Finished the dependency review and recorded the recommendation.".to_string()),
    );
    app.upsert_agent_picker_thread(
        running_id,
        Some("Tests".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.agent_navigation.mark_running(running_id);
    app.agent_navigation.set_agent_path(
        running_id,
        Some("/agents/review/long-running-test-agent".to_string()),
    );
    app.upsert_agent_picker_thread(
        closed_id,
        Some("Closed".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ true,
    );
    app.upsert_agent_picker_thread(
        error_id,
        Some("Error".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.agent_navigation.set_error(error_id, /*is_error*/ true);

    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(/*selected*/ None));

    insta::assert_snapshot!(
        "agent_picker_normal_width",
        render_bottom_popup(&app.chat_widget, /*width*/ 96)
    );
    insta::assert_snapshot!(
        "agent_picker_narrow_width",
        render_bottom_popup(&app.chat_widget, /*width*/ 48)
    );
    let narrow = render_bottom_popup(&app.chat_widget, /*width*/ 48);
    for status in ["idle", "mid-turn", "closed", "error"] {
        assert!(narrow.contains(status), "narrow picker omitted {status}");
    }
    Ok(())
}

#[tokio::test]
async fn archives_reconcile_visibility_without_forgetting_closed_history() -> Result<()> {
    use super::super::agent_navigation::AgentPickerThreadVisibility;
    use crate::app_event::AgentPickerThreadRefresh;
    let mut app = make_test_app().await;
    let app_server =
        crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let root = ThreadId::new();
    let child = ThreadId::new();
    app.primary_thread_id = Some(root);
    for id in [root, child] {
        app.upsert_agent_picker_thread(
            id,
            Some("worker".into()),
            /*agent_role*/ None,
            /*is_closed*/ false,
        );
    }
    app.ensure_thread_channel(child);
    app.enqueue_thread_notification(child, thread_closed_notification(child))
        .await?;
    assert_eq!(app.agent_navigation.visible_threads().len(), 2);
    let request_id = app.agent_navigation.begin_picker_refresh(root).unwrap();
    app.apply_agent_picker_thread_refresh(
        &app_server,
        root,
        request_id,
        app.agent_navigation.status_revision_snapshot(),
        Ok(AgentPickerThreadRefresh {
            threads: vec![],
            archived_thread_ids: HashSet::from([child]),
        }),
    );
    assert_eq!(app.agent_navigation.visible_threads().len(), 1);
    assert_eq!(
        app.thread_event_channels[&child].attachment(),
        ThreadEventAttachment::ReplayOnly
    );
    app.agent_navigation
        .record_sub_agent_activity(SubAgentActivityDisplay {
            thread_id: child,
            agent_path: "/root/late".into(),
            is_running_hint: true,
        });
    assert_eq!(app.agent_navigation.visible_threads().len(), 1);
    app.handle_agent_picker_visibility_notification(
        &app_server,
        &ServerNotification::ThreadUnarchived(
            codex_app_server_protocol::ThreadUnarchivedNotification {
                thread_id: child.to_string(),
            },
        ),
    );
    assert_eq!(app.agent_navigation.visible_threads().len(), 2);
    let request_id = app.agent_navigation.begin_picker_refresh(root).unwrap();
    app.handle_agent_picker_visibility_notification(
        &app_server,
        &ServerNotification::ThreadArchived(
            codex_app_server_protocol::ThreadArchivedNotification {
                thread_id: child.to_string(),
            },
        ),
    );
    // An older archive listing must not hide an intervening unarchive notification.
    app.set_agent_picker_thread_visibility(child, AgentPickerThreadVisibility::Visible);
    app.apply_agent_picker_thread_refresh(
        &app_server,
        root,
        request_id,
        app.agent_navigation.status_revision_snapshot(),
        Ok(AgentPickerThreadRefresh {
            threads: vec![],
            archived_thread_ids: HashSet::from([child]),
        }),
    );
    assert_eq!(app.agent_navigation.visible_threads().len(), 2);
    assert_eq!(
        app.agent_navigation
            .adjacent_thread_id(Some(root), AgentNavigationDirection::Next),
        Some(child)
    );
    app_server.shutdown().await?;
    Ok(())
}
