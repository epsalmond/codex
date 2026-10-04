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
    assert_eq!(details.context_snapshot, None);
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
        .handle_key_event(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
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
    usage.context_usage = Some(codex_app_server_protocol::ThreadContextUsage {
        active_tokens: 12,
        basis: codex_app_server_protocol::ThreadContextTokenBasis::Estimate,
        last_reduction: None,
        selected_model: None,
        child_policy_enabled: Some(/*value*/ true),
        child_active_cap_tokens: Some(/*value*/ 50),
        model_window_tokens: Some(/*value*/ 100),
        observed_at: None,
        provider_usage_at: None,
        shake_watermark: None,
    });
    app.enqueue_thread_notification(changed_id, compacted_usage)
        .await?;
    let after_compaction = render_bottom_popup(&app.chat_widget, /*width*/ 96);
    assert!(after_compaction.contains("context 12 · cap 50 · window 100"));
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
    app.upsert_agent_picker_thread(
        ThreadId::from_u128(/*value*/ 3),
        /*agent_nickname*/ None,
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    app.upsert_agent_picker_thread(
        ThreadId::from_u128(/*value*/ 6),
        /*agent_nickname*/ None,
        /*agent_role*/ None,
        /*is_closed*/ false,
    );

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
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Char('/')));
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Char('p')));
    insta::assert_snapshot!(
        "agent_picker_search_normal_width",
        render_bottom_popup(&app.chat_widget, /*width*/ 96)
    );
    insta::assert_snapshot!(
        "agent_picker_search_narrow_width",
        render_bottom_popup(&app.chat_widget, /*width*/ 48)
    );
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    insta::assert_snapshot!(
        "agent_picker_search_cleared",
        render_bottom_popup(&app.chat_widget, /*width*/ 96)
    );
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Char('t')));
    assert!(!render_bottom_popup(&app.chat_widget, /*width*/ 96).contains("Planner"));
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Esc));
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

#[tokio::test]
async fn picker_main_shortcut_and_close_keys_respect_search_mode() -> Result<()> {
    for close_key in [
        KeyEvent::from(KeyCode::Char('x')),
        KeyEvent::from(KeyCode::Char('X')),
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::SHIFT),
    ] {
        let (mut app, mut rx, _) = make_test_app_with_channels().await;
        let root = ThreadId::from_u128(/*value*/ 3);
        let child = ThreadId::from_u128(/*value*/ 1);
        app.primary_thread_id = Some(root);
        app.active_thread_id = Some(child);
        // Main must be row one even when first discovered after a child.
        for id in [child, root] {
            app.upsert_agent_picker_thread(
                id, /*agent_nickname*/ None, /*agent_role*/ None,
                /*is_closed*/ false,
            );
        }
        app.chat_widget
            .show_selection_view(app.agent_picker_selection_view_params(/*selected*/ None));
        app.chat_widget
            .handle_key_event(KeyEvent::from(KeyCode::Char('/')));
        app.chat_widget.handle_key_event(close_key);
        assert!(rx.try_recv().is_err());
        app.chat_widget
            .handle_key_event(KeyEvent::from(KeyCode::Esc));
        app.chat_widget.handle_key_event(close_key);
        assert!(matches!(rx.try_recv(), Ok(AppEvent::CloseAgentThread(id)) if id == child));
        app.chat_widget
            .show_selection_view(app.agent_picker_selection_view_params(Some(/*value*/ 0)));
        app.chat_widget.handle_key_event(close_key);
        assert!(rx.try_recv().is_err(), "Main must never be closable");
        app.chat_widget
            .handle_key_event(KeyEvent::from(KeyCode::Char('1')));
        assert!(matches!(rx.try_recv(), Ok(AppEvent::SelectAgentThread(id)) if id == root));
        app.chat_widget
            .show_selection_view(app.agent_picker_selection_view_params(Some(/*value*/ 0)));
        app.chat_widget
            .handle_key_event(KeyEvent::from(KeyCode::Char('2')));
        assert!(matches!(rx.try_recv(), Ok(AppEvent::SelectAgentThread(id)) if id == child));
    }
    Ok(())
}

#[tokio::test]
async fn agent_word_motion_shortcuts_switch_empty_drafts_with_any_keyboard_protocol() -> Result<()>
{
    for enhanced_keys_supported in [false, true] {
        let mut app = make_test_app().await;
        app.enhanced_keys_supported = enhanced_keys_supported;
        let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        app.start_fresh_session(
            &mut tui,
            &mut server,
            /*session_start_source*/ None,
            /*initial_user_message*/ None,
            /*new_thread_name*/ None,
        )
        .await;
        let root = app.chat_widget.thread_id().unwrap();
        let child_session = server.start_thread(&app.config).await?.session;
        let child = child_session.thread_id;
        app.thread_event_channels.insert(
            child,
            ThreadEventChannel::new_with_session(/*capacity*/ 4, child_session, Vec::new()),
        );
        app.upsert_agent_picker_thread(
            child, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
        for (key, expected) in [('f', child), ('b', root)] {
            app.handle_tui_event(
                &mut tui,
                &mut server,
                TuiEvent::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::ALT)),
            )
            .await?;
            assert_eq!(app.active_thread_id, Some(expected));
        }
        app.chat_widget.insert_str("one two");
        app.handle_tui_event(
            &mut tui,
            &mut server,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
        )
        .await?;
        app.chat_widget.insert_str("!");
        assert_eq!(app.active_thread_id, Some(root));
        assert_eq!(app.chat_widget.composer_text_with_pending(), "one !two");
        app.handle_tui_event(
            &mut tui,
            &mut server,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT)),
        )
        .await?;
        app.chat_widget.insert_str("?");
        assert_eq!(app.active_thread_id, Some(root));
        assert_eq!(app.chat_widget.composer_text_with_pending(), "one !two?");
        server.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn hidden_terminal_snapshot_without_counters_updates_picker_without_requests() -> Result<()> {
    use codex_app_server_protocol::ThreadContextReduction;
    use codex_app_server_protocol::ThreadContextReductionOutcome;
    use codex_app_server_protocol::ThreadContextTokenBasis;
    use codex_app_server_protocol::ThreadContextUsage;
    let (mut app, mut app_event_rx, mut op_rx) = make_test_app_with_channels().await;
    let child_id = ThreadId::from_u128(/*value*/ 17);
    app.upsert_agent_picker_thread(
        child_id,
        Some("Policy".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    let captured_at = chrono::Utc::now().timestamp();
    let snapshot = ThreadContextUsage {
        active_tokens: 120_000,
        basis: ThreadContextTokenBasis::Usage,
        last_reduction: Some(ThreadContextReduction {
            completed_at: captured_at - 3600,
            before_tokens: 120_000,
            after_tokens: None,
            outcome: ThreadContextReductionOutcome::Failed,
        }),
        selected_model: Some("gpt-6-luna".to_string()),
        child_policy_enabled: Some(/*value*/ true),
        child_active_cap_tokens: Some(/*value*/ 100_000),
        model_window_tokens: Some(/*value*/ 272_000),
        observed_at: Some(captured_at - 7200),
        provider_usage_at: Some(captured_at - 86400),
        shake_watermark: Some(/*value*/ 42),
    };
    let mut notification = turn_completed_notification(child_id, "first", TurnStatus::Failed);
    let ServerNotification::TurnCompleted(completed) = &mut notification else {
        unreachable!()
    };
    completed.context_usage = Some(snapshot.clone());
    app.enqueue_thread_notification(child_id, notification)
        .await?;
    let details = app.agent_navigation.picker_details(&child_id).unwrap();
    assert_eq!(details.context_snapshot.as_ref(), Some(&snapshot));
    assert_eq!(details.context_usage, None);
    assert!(app_event_rx.try_recv().is_err());
    assert!(op_rx.try_recv().is_err());
    app.upsert_agent_picker_thread(
        ThreadId::from_u128(/*value*/ 18),
        Some("Other".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(/*selected*/ None));
    insta::assert_snapshot!(
        "agent_picker_context_policy",
        render_bottom_popup(&app.chat_widget, /*width*/ 160)
    );
    insta::assert_snapshot!(
        "agent_picker_context_policy_narrow",
        render_bottom_popup(&app.chat_widget, /*width*/ 64)
    );
    let full = render_bottom_popup(&app.chat_widget, /*width*/ 160);
    assert!(full.contains("snapshot 2h ago"));
    assert!(full.contains("provider 1d ago"));
    assert!(full.contains("failed 120000→? (1h ago)"));
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Down));
    insta::assert_snapshot!(
        "agent_picker_context_other_selected",
        render_bottom_popup(&app.chat_widget, /*width*/ 160)
    );
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Char('/')));
    for key in ['p', 'o', 'l'] {
        app.chat_widget
            .handle_key_event(KeyEvent::from(KeyCode::Char(key)));
    }
    let filtered = render_bottom_popup(&app.chat_widget, /*width*/ 160);
    assert!(filtered.contains("failed 120000→? (1h ago)"));
    assert!(app_event_rx.try_recv().is_err());
    assert!(op_rx.try_recv().is_err());
    Ok(())
}
