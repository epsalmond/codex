use super::*;
use crate::bottom_pane::StatusLineItem;
use crate::chatwidget::tests::helpers::render_bottom_popup;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::SubAgentActivityKind;
use pretty_assertions::assert_eq;

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

fn rendered_count(app: &App) -> String {
    render_bottom_popup(&app.chat_widget, /*width*/ 80)
        .lines()
        .find(|line| line.contains("working"))
        .expect("active subagent status line")
        .split(" · ")
        .next()
        .expect("count before contextual footer label")
        .trim()
        .to_string()
}

#[tokio::test]
async fn status_line_subagents_updates_during_parent_streaming() -> Result<()> {
    let mut app = make_test_app().await;
    let root = ThreadId::new();
    let child = ThreadId::new();
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.chat_widget.setup_status_line(
        vec![StatusLineItem::ActiveSubagents],
        /*use_theme_colors*/ false,
    );
    app.handle_thread_event_now(ThreadBufferedEvent::Notification(Box::new(
        turn_started_notification(root, "parent-turn"),
    )));
    app.handle_thread_event_now(ThreadBufferedEvent::Notification(Box::new(
        agent_message_delta_notification(root, "parent-turn", "answer", "Parent is streaming."),
    )));
    assert!(app.chat_widget.has_active_agent_stream());

    app.handle_thread_event_now(ThreadBufferedEvent::Notification(Box::new(
        activity_notification(root, child, "/root/worker", SubAgentActivityKind::Started),
    )));
    let started = rendered_count(&app);
    assert_eq!(started, "1 agent working");
    assert!(app.chat_widget.has_active_agent_stream());

    app.handle_thread_event_now(ThreadBufferedEvent::Notification(Box::new(
        activity_notification(root, child, "/root/worker", SubAgentActivityKind::Completed),
    )));
    let completed = rendered_count(&app);
    assert_eq!(completed, "0 agents working");
    assert!(app.chat_widget.has_active_agent_stream());

    insta::assert_snapshot!(format!("{started}\n{completed}"), @"
    1 agent working
    0 agents working
    ");
    Ok(())
}

#[tokio::test]
async fn status_line_subagents_follows_restart_failure_and_close() -> Result<()> {
    let mut app = make_test_app().await;
    let root = ThreadId::new();
    let child = ThreadId::new();
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.chat_widget.setup_status_line(
        vec![StatusLineItem::ActiveSubagents],
        /*use_theme_colors*/ false,
    );
    app.handle_thread_event_now(ThreadBufferedEvent::Notification(Box::new(
        activity_notification(root, child, "/root/worker", SubAgentActivityKind::Started),
    )));
    app.enqueue_thread_notification(child, turn_started_notification(child, "turn-1"))
        .await?;
    app.enqueue_thread_notification(
        child,
        turn_completed_notification(child, "turn-1", TurnStatus::Completed),
    )
    .await?;
    assert_eq!(rendered_count(&app), "0 agents working");

    // The same Interacted item accompanies trigger-turn followups and messages to idle agents.
    app.handle_thread_event_now(ThreadBufferedEvent::Notification(Box::new(
        activity_notification(
            root,
            child,
            "/root/worker",
            SubAgentActivityKind::Interacted,
        ),
    )));
    assert_eq!(rendered_count(&app), "0 agents working");
    app.enqueue_thread_notification(child, turn_started_notification(child, "turn-2"))
        .await?;
    assert_eq!(rendered_count(&app), "1 agent working");
    app.handle_thread_event_now(ThreadBufferedEvent::Notification(Box::new(
        activity_notification(
            root,
            child,
            "/root/worker",
            SubAgentActivityKind::Interacted,
        ),
    )));
    assert_eq!(rendered_count(&app), "1 agent working");

    // Failed child turns need not publish a Completed activity item to the parent.
    app.enqueue_thread_notification(
        child,
        turn_completed_notification(child, "turn-2", TurnStatus::Failed),
    )
    .await?;
    assert_eq!(rendered_count(&app), "0 agents working");
    app.enqueue_thread_notification(child, turn_started_notification(child, "turn-3"))
        .await?;
    assert_eq!(rendered_count(&app), "1 agent working");
    app.enqueue_thread_notification(child, thread_closed_notification(child))
        .await?;
    assert_eq!(rendered_count(&app), "0 agents working");
    Ok(())
}

#[tokio::test]
async fn status_line_subagents_scopes_count_to_displayed_descendants() -> Result<()> {
    let mut app = make_test_app().await;
    let root = ThreadId::new();
    let child = ThreadId::new();
    let grandchild = ThreadId::new();
    let sibling = ThreadId::new();
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.chat_widget.setup_status_line(
        vec![StatusLineItem::ActiveSubagents],
        /*use_theme_colors*/ false,
    );
    for (thread_id, path) in [
        (child, "/root/worker"),
        (grandchild, "/root/worker/nested"),
        (sibling, "/root/worker_other"),
    ] {
        app.handle_thread_event_now(ThreadBufferedEvent::Notification(Box::new(
            activity_notification(root, thread_id, path, SubAgentActivityKind::Started),
        )));
    }
    assert_eq!(rendered_count(&app), "3 agents working");
    app.active_thread_id = Some(child);
    app.sync_active_agent_label();
    assert_eq!(rendered_count(&app), "1 agent working");
    app.enqueue_thread_notification(
        grandchild,
        ServerNotification::ThreadStatusChanged(
            codex_app_server_protocol::ThreadStatusChangedNotification {
                thread_id: grandchild.to_string(),
                status: codex_app_server_protocol::ThreadStatus::SystemError,
            },
        ),
    )
    .await?;
    assert_eq!(rendered_count(&app), "0 agents working");

    app.agent_navigation.clear();
    app.primary_thread_id = Some(ThreadId::new());
    app.active_thread_id = app.primary_thread_id;
    app.sync_active_agent_label();
    assert_eq!(rendered_count(&app), "0 agents working");
    Ok(())
}
