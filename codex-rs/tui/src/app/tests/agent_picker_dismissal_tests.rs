use super::*;
use crate::bottom_pane::StatusLineItem;
use crate::chatwidget::tests::helpers::render_bottom_popup;
use crate::multi_agents::SubagentCounts;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::SubAgentActivityKind;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadStatusChangedNotification;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn picker_dismissal_preserves_running_grandchildren_and_restores_restarted_rows() -> Result<()>
{
    let (mut app, mut rx, mut op_rx) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(/*value*/ 100);
    let idle = ThreadId::from_u128(/*value*/ 101);
    let running = ThreadId::from_u128(/*value*/ 102);
    let closed = ThreadId::from_u128(/*value*/ 103);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    for (id, path) in [
        (root, "/root"),
        (idle, "/root/idle"),
        (running, "/root/idle/running"),
        (closed, "/root/closed"),
    ] {
        app.upsert_agent_picker_thread(
            id,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
            id == closed,
        );
        app.agent_navigation
            .set_agent_path(id, Some(path.to_string()));
    }
    app.ensure_thread_channel(idle);
    app.agent_navigation.mark_running(running);
    app.chat_widget.setup_status_line(
        vec![StatusLineItem::ActiveSubagents],
        /*use_theme_colors*/ false,
    );
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(Some(/*value*/ 2)));
    // q during a search is ordinary query text and sends no cleanup event.
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Char('/')));
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Char('q')));
    assert!(rx.try_recv().is_err());
    assert!(render_bottom_popup(&app.chat_widget, /*width*/ 120).contains("q"));
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Esc));
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Char('q')));
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::DismissInactiveSubagents)
    ));
    let attachment = app.thread_event_channels[&idle].attachment();
    app.dismiss_inactive_subagents();
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, running]
    );
    assert_eq!(app.thread_event_channels[&idle].attachment(), attachment);
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 1,
            total: 2
        }
    );
    assert_eq!(
        app.chat_widget
            .selected_index_for_present_view("agent-picker"),
        Some(/*value*/ 1)
    );
    insta::assert_snapshot!(
        "agent_picker_inactive_dismissed",
        render_bottom_popup(&app.chat_widget, /*width*/ 120)
    );
    assert!(rx.try_recv().is_err());
    assert!(op_rx.try_recv().is_err());
    // Rediscovery and unarchive visibility updates cannot undo local dismissal.
    app.upsert_agent_picker_thread(
        idle, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    app.agent_navigation
        .set_picker_thread_visibility(idle, AgentPickerThreadVisibility::Visible);
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, running]
    );
    app.enqueue_thread_notification(idle, turn_started_notification(idle, "restart"))
        .await?;
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, idle, running]
    );
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 2,
            total: 2
        }
    );
    app.enqueue_thread_notification(
        idle,
        turn_completed_notification(idle, "restart", TurnStatus::Completed),
    )
    .await?;
    // Completion stays reviewable until the user dismisses inactive rows again.
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, idle, running]
    );
    app.dismiss_inactive_subagents();
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, running]
    );
    app.set_agent_picker_thread_visibility(running, AgentPickerThreadVisibility::Hidden);
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 0,
            total: 1
        }
    );
    app.agent_navigation.mark_running(running);
    // A backend archive remains hidden even if a delayed running event arrives.
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root]);
    app.agent_navigation.clear();
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts::default()
    );
    Ok(())
}

#[tokio::test]
async fn picker_dismissal_keeps_main_when_every_child_is_inactive() -> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(/*value*/ 200);
    let child = ThreadId::from_u128(/*value*/ 201);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(child);
    for id in [root, child] {
        app.upsert_agent_picker_thread(
            id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
    }
    app.agent_navigation.mark_subagent(child);
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(/*selected*/ None));
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Char('q')));
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::DismissInactiveSubagents)
    ));
    app.dismiss_inactive_subagents();
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root]);
    assert_eq!(app.active_thread_id, Some(child));
    insta::assert_snapshot!(
        "agent_picker_all_inactive_dismissed",
        render_bottom_popup(&app.chat_widget, /*width*/ 120)
    );
    app.agent_navigation
        .set_running_from_discovery(child, /*is_running*/ true);
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root, child]);
    Ok(())
}

#[tokio::test]
async fn picker_dismissal_and_counts_exclude_side_conversations_but_include_legacy_subagents()
-> Result<()> {
    let mut app = make_test_app().await;
    let root = ThreadId::new();
    let side = ThreadId::new();
    let legacy = ThreadId::new();
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    // /side seeds a pathless navigation entry before its thread/started notification.
    app.side_threads.insert(side, SideThreadState::new(root));
    for id in [root, side] {
        app.upsert_agent_picker_thread(
            id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
    }
    app.enqueue_thread_notification(
        root,
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: root.to_string(),
            turn_id: "spawn-turn".into(),
            started_at_ms: 0,
            item: ThreadItem::CollabAgentToolCall {
                id: "spawn".into(),
                tool: codex_app_server_protocol::CollabAgentTool::SpawnAgent,
                status: codex_app_server_protocol::CollabAgentToolCallStatus::InProgress,
                sender_thread_id: root.to_string(),
                receiver_thread_ids: vec![legacy.to_string()],
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: HashMap::new(),
            },
        }),
    )
    .await?;
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 0,
            total: 1
        }
    );
    app.agent_navigation.mark_running(side);
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 0,
            total: 1
        }
    );
    app.agent_navigation.mark_stopped(side);
    // The legacy child was just spawned and has not started its first turn.
    app.dismiss_inactive_subagents();
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, side, legacy]
    );
    app.agent_navigation.mark_running(legacy);
    app.agent_navigation.mark_stopped(legacy);
    app.dismiss_inactive_subagents();
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root, side]);
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 0,
            total: 1
        }
    );
    Ok(())
}

#[tokio::test]
async fn archived_subagents_leave_status_counts_durably() -> Result<()> {
    let mut app = make_test_app().await;
    let app_server =
        crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let root = ThreadId::from_u128(/*value*/ 300);
    let worker = ThreadId::from_u128(/*value*/ 301);
    let helper = ThreadId::from_u128(/*value*/ 302);
    let sibling = ThreadId::from_u128(/*value*/ 303);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    for (id, path) in [
        (root, "/root"),
        (worker, "/root/worker"),
        (helper, "/root/worker/helper"),
        (sibling, "/root/sibling"),
    ] {
        app.upsert_agent_picker_thread(
            id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
        app.agent_navigation
            .set_agent_path(id, Some(path.to_string()));
    }
    for id in [worker, helper, sibling] {
        app.agent_navigation.mark_running(id);
    }
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 3,
            total: 3
        }
    );

    // The backend archives the closed subtree and notifies each member.
    for id in [worker, helper] {
        app.handle_agent_picker_visibility_notification(
            &app_server,
            &ServerNotification::ThreadArchived(
                codex_app_server_protocol::ThreadArchivedNotification {
                    thread_id: id.to_string(),
                },
            ),
        );
    }
    let remaining = SubagentCounts {
        running: 1,
        total: 1,
    };
    assert_eq!(app.agent_navigation.subagent_counts(Some(root)), remaining);
    assert_eq!(
        app.agent_navigation
            .get(&worker)
            .map(|entry| entry.is_closed),
        Some(true)
    );

    // Late status events and an empty refresh cannot bring the closed agents back.
    app.agent_navigation.mark_running(worker);
    app.agent_navigation
        .record_sub_agent_activity(SubAgentActivityDisplay {
            thread_id: helper,
            agent_path: "/root/worker/helper".into(),
            is_running_hint: true,
        });
    app.set_agent_picker_thread_visibility(worker, AgentPickerThreadVisibility::Visible);
    let request_id = app.agent_navigation.begin_picker_refresh(root).unwrap();
    app.apply_agent_picker_thread_refresh(
        &app_server,
        root,
        request_id,
        app.agent_navigation.status_revision_snapshot(),
        Ok(crate::app_event::AgentPickerThreadRefresh {
            threads: vec![],
            archived_thread_ids: HashSet::new(),
        }),
    );
    assert_eq!(app.agent_navigation.subagent_counts(Some(root)), remaining);
    Ok(())
}

fn activity_notification(
    parent: ThreadId,
    child: ThreadId,
    path: &str,
    kind: SubAgentActivityKind,
) -> ServerNotification {
    ServerNotification::ItemCompleted(ItemCompletedNotification {
        thread_id: parent.to_string(),
        turn_id: "parent-turn".to_string(),
        completed_at_ms: 0,
        item: ThreadItem::SubAgentActivity {
            id: "activity".to_string(),
            kind,
            agent_thread_id: child.to_string(),
            agent_path: path.to_string(),
            model: None,
            reasoning_effort: None,
        },
    })
}

fn status_notification(thread_id: ThreadId, status: ThreadStatus) -> ServerNotification {
    ServerNotification::ThreadStatusChanged(ThreadStatusChangedNotification {
        thread_id: thread_id.to_string(),
        status,
    })
}

fn active_status() -> ThreadStatus {
    ThreadStatus::Active {
        active_flags: Vec::new(),
    }
}

async fn parent_event(app: &mut App, notification: ServerNotification) {
    let parent = match &notification {
        ServerNotification::ItemStarted(item) => &item.thread_id,
        ServerNotification::ItemCompleted(item) => &item.thread_id,
        _ => panic!("expected parent activity"),
    };
    app.enqueue_thread_notification(ThreadId::from_string(parent).unwrap(), notification)
        .await
        .unwrap();
}

fn press_q(app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    while rx.try_recv().is_ok() {}
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(/*selected*/ None));
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Char('q')));
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::DismissInactiveSubagents)
    ));
    app.dismiss_inactive_subagents();
}

#[tokio::test]
async fn picker_dismissal_keeps_spawned_child_until_its_first_turn_ends() -> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(/*value*/ 400);
    let child = ThreadId::from_u128(/*value*/ 401);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.upsert_agent_picker_thread(
        root, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    app.agent_navigation
        .set_agent_path(root, Some("/root".to_string()));
    parent_event(
        &mut app,
        activity_notification(root, child, "/root/worker", SubAgentActivityKind::Started),
    )
    .await;
    // The new child reports idle before its first turn starts.
    app.enqueue_thread_notification(child, status_notification(child, ThreadStatus::Idle))
        .await?;
    assert_eq!(
        app.agent_navigation
            .get(&child)
            .map(|entry| entry.is_running),
        Some(false)
    );

    press_q(&mut app, &mut rx);
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root, child]);

    app.enqueue_thread_notification(child, turn_started_notification(child, "first"))
        .await?;
    app.enqueue_thread_notification(
        child,
        turn_completed_notification(child, "first", TurnStatus::Completed),
    )
    .await?;
    press_q(&mut app, &mut rx);
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root]);
    Ok(())
}

#[tokio::test]
async fn dismissed_child_reappears_for_follow_up_turn_after_idle_eviction() -> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(/*value*/ 500);
    let child = ThreadId::from_u128(/*value*/ 501);
    let path = "/root/worker";
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.upsert_agent_picker_thread(
        root, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    app.agent_navigation
        .set_agent_path(root, Some("/root".to_string()));

    // Spawn, first turn, and completion as the parent and child streams report them.
    parent_event(
        &mut app,
        activity_notification(root, child, path, SubAgentActivityKind::Started),
    )
    .await;
    for notification in [
        status_notification(child, active_status()),
        turn_started_notification(child, "first"),
        turn_completed_notification(child, "first", TurnStatus::Completed),
        status_notification(child, ThreadStatus::Idle),
    ] {
        app.enqueue_thread_notification(child, notification).await?;
    }
    parent_event(
        &mut app,
        activity_notification(root, child, path, SubAgentActivityKind::Completed),
    )
    .await;
    press_q(&mut app, &mut rx);
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root]);

    // The idle child is evicted, then reloads under the same id for the parent's follow-up.
    app.enqueue_thread_notification(child, thread_closed_notification(child))
        .await?;
    parent_event(
        &mut app,
        activity_notification(root, child, path, SubAgentActivityKind::Interacted),
    )
    .await;
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root, child]);
    press_q(&mut app, &mut rx);
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root]);
    for notification in [
        status_notification(child, active_status()),
        turn_started_notification(child, "follow-up"),
    ] {
        app.enqueue_thread_notification(child, notification).await?;
    }
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root, child]);
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 1,
            total: 1
        }
    );

    // Completion keeps the row until the user dismisses inactive rows again.
    for notification in [
        turn_completed_notification(child, "follow-up", TurnStatus::Completed),
        status_notification(child, ThreadStatus::Idle),
    ] {
        app.enqueue_thread_notification(child, notification).await?;
    }
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root, child]);
    press_q(&mut app, &mut rx);
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root]);
    Ok(())
}

#[tokio::test]
async fn late_spawn_hints_do_not_pin_a_child_that_already_ran() -> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(/*value*/ 600);
    let child = ThreadId::from_u128(/*value*/ 601);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.upsert_agent_picker_thread(
        root, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    app.agent_navigation
        .set_agent_path(root, Some("/root".to_string()));

    // The child stream can arrive before either producer form reports the spawn.
    for notification in [
        turn_started_notification(child, "first"),
        turn_completed_notification(child, "first", TurnStatus::Failed),
        status_notification(child, ThreadStatus::Idle),
    ] {
        app.enqueue_thread_notification(child, notification).await?;
    }
    parent_event(
        &mut app,
        activity_notification(root, child, "/root/worker", SubAgentActivityKind::Started),
    )
    .await;
    parent_event(
        &mut app,
        legacy_spawn_notification(root, vec![child.to_string()]),
    )
    .await;
    assert!(!app.agent_navigation.get(&child).unwrap().is_running);
    assert_eq!(app.agent_navigation.idle_close_targets(root), vec![child]);

    press_q(&mut app, &mut rx);
    assert_eq!(app.agent_navigation.tracked_thread_ids(), vec![root]);
    Ok(())
}

fn seed_hidden_parent(app: &mut App, root: ThreadId, parent: ThreadId) {
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    for (id, path) in [(root, "/root"), (parent, "/root/parent")] {
        app.upsert_agent_picker_thread(
            id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
        app.agent_navigation.set_agent_path(id, Some(path.into()));
    }
    app.agent_navigation.mark_running(parent);
    app.ensure_thread_channel(parent);
}

async fn route_notification(
    app: &mut App,
    server: &AppServerSession,
    notification: ServerNotification,
) {
    app.handle_app_server_event(
        server,
        codex_app_server_client::AppServerEvent::ServerNotification(Box::new(notification)),
    )
    .await;
}

fn legacy_spawn_notification(parent: ThreadId, receivers: Vec<String>) -> ServerNotification {
    ServerNotification::ItemCompleted(ItemCompletedNotification {
        thread_id: parent.to_string(),
        turn_id: "parent-turn".into(),
        completed_at_ms: 0,
        item: ThreadItem::CollabAgentToolCall {
            id: "spawn".into(),
            tool: codex_app_server_protocol::CollabAgentTool::SpawnAgent,
            status: codex_app_server_protocol::CollabAgentToolCallStatus::Completed,
            sender_thread_id: parent.to_string(),
            receiver_thread_ids: receivers,
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        },
    })
}

#[tokio::test]
async fn hidden_parent_arrival_updates_live_picker_and_protects_pending_grandchild() -> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let [root, parent, child] = [700, 701, 702].map(ThreadId::from_u128);
    seed_hidden_parent(&mut app, root, parent);
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(None));
    assert!(!render_bottom_popup(&app.chat_widget, 120).contains("/root/parent/grandchild"));

    route_notification(
        &mut app,
        &server,
        activity_notification(
            parent,
            child,
            "/root/parent/grandchild",
            SubAgentActivityKind::Started,
        ),
    )
    .await;
    assert_eq!(app.current_displayed_thread_id(), Some(root));
    assert!(
        app.active_thread_rx.is_none(),
        "hidden parent was never displayed"
    );
    assert!(render_bottom_popup(&app.chat_widget, 120).contains("/root/parent/grandchild"));
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent, child]
    );
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 2,
            total: 2
        }
    );

    route_notification(
        &mut app,
        &server,
        status_notification(child, ThreadStatus::Idle),
    )
    .await;
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 1,
            total: 2
        }
    );
    assert!(app.agent_navigation.idle_close_targets(root).is_empty());
    press_q(&mut app, &mut rx);
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent, child]
    );
    for notification in [
        turn_started_notification(child, "first"),
        turn_completed_notification(child, "first", TurnStatus::Completed),
    ] {
        route_notification(&mut app, &server, notification).await;
    }
    assert_eq!(app.agent_navigation.idle_close_targets(root), vec![child]);
    press_q(&mut app, &mut rx);
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent]
    );
    Ok(())
}

#[tokio::test]
async fn hidden_parent_contact_restores_order_without_liveness_and_snapshot_cannot_restore_again()
-> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let [root, parent, child, sibling] = [710, 711, 712, 713].map(ThreadId::from_u128);
    seed_hidden_parent(&mut app, root, parent);
    for (id, path) in [
        (child, "/root/parent/grandchild"),
        (sibling, "/root/sibling"),
    ] {
        parent_event(
            &mut app,
            activity_notification(parent, id, path, SubAgentActivityKind::Started),
        )
        .await;
        app.enqueue_thread_notification(id, turn_started_notification(id, "first"))
            .await?;
        app.enqueue_thread_notification(
            id,
            turn_completed_notification(id, "first", TurnStatus::Completed),
        )
        .await?;
    }
    let counts = app.agent_navigation.subagent_counts(Some(root));
    let child_state = app.agent_navigation.get(&child).unwrap().clone();
    press_q(&mut app, &mut rx);
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent]
    );
    route_notification(
        &mut app,
        &server,
        activity_notification(
            parent,
            child,
            "/root/parent/grandchild",
            SubAgentActivityKind::Interacted,
        ),
    )
    .await;
    assert_eq!(app.current_displayed_thread_id(), Some(root));
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent, child]
    );
    assert_eq!(app.agent_navigation.get(&child), Some(&child_state));
    assert_eq!(app.agent_navigation.subagent_counts(Some(root)), counts);
    assert!(render_bottom_popup(&app.chat_widget, 120).contains("/root/parent/grandchild"));
    // Restoring the older sibling proves the first-seen order survives dismissal/contact.
    route_notification(
        &mut app,
        &server,
        activity_notification(
            parent,
            sibling,
            "/root/sibling",
            SubAgentActivityKind::Interacted,
        ),
    )
    .await;
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent, child, sibling]
    );
    press_q(&mut app, &mut rx);
    for _ in 0..2 {
        let snapshot = app.thread_event_channels[&parent]
            .store
            .lock()
            .await
            .snapshot();
        assert!(!snapshot.events.is_empty());
        app.replay_thread_snapshot(snapshot, /*resume_restored_queue*/ false);
        assert_eq!(
            app.agent_navigation.tracked_thread_ids(),
            vec![root, parent]
        );
        assert_eq!(app.agent_navigation.get(&child), Some(&child_state));
        assert_eq!(app.agent_navigation.subagent_counts(Some(root)), counts);
    }
    Ok(())
}

#[tokio::test]
async fn active_parent_stale_queue_drain_cannot_undo_later_dismissal() -> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let [root, parent, child] = [720, 721, 722].map(ThreadId::from_u128);
    seed_hidden_parent(&mut app, root, parent);
    parent_event(
        &mut app,
        activity_notification(
            parent,
            child,
            "/root/parent/grandchild",
            SubAgentActivityKind::Started,
        ),
    )
    .await;
    for notification in [
        turn_started_notification(child, "first"),
        turn_completed_notification(child, "first", TurnStatus::Completed),
    ] {
        app.enqueue_thread_notification(child, notification).await?;
    }
    press_q(&mut app, &mut rx);
    app.active_thread_id = None;
    app.activate_thread_channel(parent).await;
    parent_event(
        &mut app,
        activity_notification(
            parent,
            child,
            "/root/parent/grandchild",
            SubAgentActivityKind::Interacted,
        ),
    )
    .await;
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent, child]
    );
    assert!(!app.active_thread_rx.as_ref().unwrap().is_empty());
    press_q(&mut app, &mut rx);
    let counts = app.agent_navigation.subagent_counts(Some(root));
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.drain_active_thread_events_until(&mut tui, Instant::now() + Duration::from_secs(1))
        .await?;
    assert!(app.active_thread_rx.as_ref().unwrap().is_empty());
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent]
    );
    assert_eq!(app.agent_navigation.subagent_counts(Some(root)), counts);
    Ok(())
}

#[tokio::test]
async fn hidden_parent_legacy_spawn_is_pending_and_rejects_invalid_or_not_found_receivers()
-> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let [root, parent, child, missing] = [730, 731, 732, 733].map(ThreadId::from_u128);
    seed_hidden_parent(&mut app, root, parent);
    let mut notification = legacy_spawn_notification(
        parent,
        vec![child.to_string(), "invalid-id".into(), missing.to_string()],
    );
    if let ServerNotification::ItemCompleted(item) = &mut notification
        && let ThreadItem::CollabAgentToolCall { agents_states, .. } = &mut item.item
    {
        agents_states.insert(
            missing.to_string(),
            codex_app_server_protocol::CollabAgentState {
                status: codex_app_server_protocol::CollabAgentStatus::NotFound,
                message: None,
            },
        );
    }
    route_notification(&mut app, &server, notification).await;
    assert_eq!(app.current_displayed_thread_id(), Some(root));
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent, child]
    );
    assert_eq!(app.agent_navigation.get(&child).unwrap().agent_path, None);
    assert!(!app.agent_navigation.get(&child).unwrap().is_running);
    assert_eq!(app.agent_navigation.get(&missing), None);
    assert_eq!(
        app.agent_navigation.subagent_counts(Some(root)),
        SubagentCounts {
            running: 1,
            total: 2
        }
    );
    assert!(app.agent_navigation.idle_close_targets(root).is_empty());
    press_q(&mut app, &mut rx);
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent, child]
    );
    Ok(())
}

#[tokio::test]
async fn parent_contact_preserves_closed_review_rows_and_backend_archive_exclusion() -> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let [root, parent, closed, archived] = [740, 741, 742, 743].map(ThreadId::from_u128);
    seed_hidden_parent(&mut app, root, parent);
    for (id, path) in [(closed, "/root/closed"), (archived, "/root/archived")] {
        parent_event(
            &mut app,
            activity_notification(parent, id, path, SubAgentActivityKind::Started),
        )
        .await;
        app.mark_agent_picker_thread_closed(id);
    }
    app.set_agent_picker_thread_visibility(archived, AgentPickerThreadVisibility::Hidden);
    press_q(&mut app, &mut rx);
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent]
    );
    let counts = app.agent_navigation.subagent_counts(Some(root));
    for (id, path) in [(closed, "/root/closed"), (archived, "/root/archived")] {
        route_notification(
            &mut app,
            &server,
            activity_notification(parent, id, path, SubAgentActivityKind::Interacted),
        )
        .await;
        assert!(app.agent_navigation.get(&id).unwrap().is_closed);
        assert!(!app.agent_navigation.get(&id).unwrap().is_running);
    }
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent, closed]
    );
    for notification in [
        activity_notification(
            parent,
            archived,
            "/root/archived",
            SubAgentActivityKind::Started,
        ),
        legacy_spawn_notification(parent, vec![archived.to_string()]),
    ] {
        route_notification(&mut app, &server, notification).await;
        assert_eq!(
            app.agent_navigation.tracked_thread_ids(),
            vec![root, parent, closed]
        );
        assert_eq!(app.agent_navigation.subagent_counts(Some(root)), counts);
    }
    press_q(&mut app, &mut rx);
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent]
    );
    Ok(())
}

#[tokio::test]
async fn rejected_parent_router_arrivals_cannot_create_or_restore_receivers() -> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let [
        root,
        parent,
        dismissed,
        unrelated,
        structured,
        abandoned,
        new_child,
    ] = [750, 751, 752, 753, 754, 755, 756].map(ThreadId::from_u128);
    seed_hidden_parent(&mut app, root, parent);
    parent_event(
        &mut app,
        activity_notification(
            parent,
            dismissed,
            "/root/dismissed",
            SubAgentActivityKind::Completed,
        ),
    )
    .await;
    press_q(&mut app, &mut rx);
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    app.temporary_structured_requests.insert(structured, sender);
    for id in [structured, abandoned] {
        app.ensure_thread_channel(id);
    }
    app.abandoned_side_threads.insert(abandoned);
    let ids = app.agent_navigation.tracked_thread_ids();
    let state = app.agent_navigation.get(&dismissed).unwrap().clone();
    let counts = app.agent_navigation.subagent_counts(Some(root));
    for rejected in [unrelated, structured, abandoned] {
        for notification in [
            activity_notification(
                rejected,
                new_child,
                "/root/new",
                SubAgentActivityKind::Started,
            ),
            activity_notification(
                rejected,
                dismissed,
                "/root/dismissed",
                SubAgentActivityKind::Interacted,
            ),
            legacy_spawn_notification(rejected, vec![new_child.to_string(), dismissed.to_string()]),
        ] {
            route_notification(&mut app, &server, notification).await;
            assert_eq!(app.agent_navigation.tracked_thread_ids(), ids);
            assert_eq!(app.agent_navigation.get(&dismissed), Some(&state));
            assert_eq!(app.agent_navigation.get(&new_child), None);
            assert_eq!(app.agent_navigation.subagent_counts(Some(root)), counts);
        }
    }
    Ok(())
}

#[tokio::test]
async fn full_parent_channel_applies_receiver_visibility_before_display_capacity_returns()
-> Result<()> {
    let mut app = make_test_app().await;
    let [root, parent, child] = [760, 761, 762].map(ThreadId::from_u128);
    seed_hidden_parent(&mut app, root, parent);
    app.thread_event_channels
        .insert(parent, ThreadEventChannel::new(1));
    app.set_thread_active(parent, /*active*/ true).await;
    parent_event(
        &mut app,
        activity_notification(
            parent,
            child,
            "/root/parent/grandchild",
            SubAgentActivityKind::Completed,
        ),
    )
    .await;
    app.dismiss_inactive_subagents();
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent]
    );
    time::timeout(
        Duration::from_millis(50),
        parent_event(
            &mut app,
            activity_notification(
                parent,
                child,
                "/root/parent/grandchild",
                SubAgentActivityKind::Interacted,
            ),
        ),
    )
    .await
    .expect("receiver visibility must not wait for channel capacity");
    assert_eq!(
        app.agent_navigation.tracked_thread_ids(),
        vec![root, parent, child]
    );
    let mut receiver = app
        .thread_event_channels
        .get_mut(&parent)
        .unwrap()
        .receiver
        .take()
        .unwrap();
    // Free the slot and consume the deferred send so this test leaves no sender task waiting.
    for _ in 0..2 {
        time::timeout(Duration::from_secs(1), receiver.recv())
            .await?
            .expect("queued parent event");
    }
    Ok(())
}
