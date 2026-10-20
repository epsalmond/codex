use super::*;
use crate::app::agent_picker::AGENT_PICKER_VIEW_ID;
use crate::app::process_tree_view::process_tree_view_params;
use crate::app::process_tree_view::recent_output_lines;
use crate::app_event::ProcessTreeEntry;
use crate::chatwidget::tests::helpers::render_bottom_popup;
use pretty_assertions::assert_eq;

const NOW: i64 = 1_800_000_000;

fn entries() -> Vec<ProcessTreeEntry> {
    vec![
        ProcessTreeEntry {
            thread_id: ThreadId::from_u128(/*value*/ 1),
            owner: "Main [default]".to_string(),
            process_id: "1001".to_string(),
            command: "cargo build -p codex-tui".to_string(),
            started_at: Some(NOW - 125),
            wakes_owner: true,
            recent_output: vec!["   Compiling codex-tui v0.0.0".to_string()],
        },
        ProcessTreeEntry {
            thread_id: ThreadId::from_u128(/*value*/ 2),
            owner: "Planner [worker]".to_string(),
            process_id: "2001".to_string(),
            command: "npm run dev".to_string(),
            started_at: Some(NOW - 3_725),
            wakes_owner: false,
            recent_output: recent_output_lines(
                "> dev\n\nready on http://localhost:3000\n\u{1b}[32mcompiled\u{1b}[0m in 120ms\n",
            ),
        },
        ProcessTreeEntry {
            thread_id: ThreadId::from_u128(/*value*/ 3),
            owner: "/agents/review/tests".to_string(),
            process_id: "3001".to_string(),
            command: "wait-for-build -- just test".to_string(),
            started_at: None,
            wakes_owner: true,
            recent_output: Vec::new(),
        },
    ]
}

#[tokio::test]
async fn process_tree_rows_show_owner_wake_elapsed_command_and_output() {
    let mut app = make_test_app().await;
    app.chat_widget
        .show_selection_view(process_tree_view_params(&entries(), NOW));
    insta::assert_snapshot!(
        "process_tree_normal_width",
        render_bottom_popup(&app.chat_widget, /*width*/ 100)
    );
    app.chat_widget
        .handle_key_event(KeyEvent::from(KeyCode::Down));
    insta::assert_snapshot!(
        "process_tree_second_row_narrow",
        render_bottom_popup(&app.chat_widget, /*width*/ 56)
    );
}

#[tokio::test]
async fn process_tree_enter_shows_output_and_x_stops_selected_terminal() {
    let entries = entries();
    for (key, expect_stop) in [
        (KeyEvent::from(KeyCode::Enter), false),
        (KeyEvent::from(KeyCode::Char('x')), true),
    ] {
        let (mut app, mut rx, _) = make_test_app_with_channels().await;
        app.chat_widget
            .show_selection_view(process_tree_view_params(&entries, NOW));
        app.chat_widget
            .handle_key_event(KeyEvent::from(KeyCode::Down));
        app.chat_widget.handle_key_event(key);
        let event = rx.try_recv().expect("row action should send an event");
        let entry = match event {
            AppEvent::StopBackgroundTerminal(entry) if expect_stop => entry,
            AppEvent::ShowBackgroundTerminalOutput(entry) if !expect_stop => entry,
            other => panic!("unexpected event: {other:?}"),
        };
        assert_eq!(*entry, entries[1]);
    }
}

#[test]
fn recent_output_keeps_newest_nonempty_lines() {
    let output = (0..20)
        .map(|line| format!("line {line}  \n\n"))
        .collect::<String>();
    let lines = recent_output_lines(&output);
    assert_eq!(lines.len(), 12);
    assert_eq!(lines.first().map(String::as_str), Some("line 8"));
    assert_eq!(lines.last().map(String::as_str), Some("line 19"));
}

#[tokio::test]
async fn late_process_tree_listing_does_not_cover_other_views_or_hide_failures() {
    let mut app = make_test_app().await;
    let root = ThreadId::from_u128(/*value*/ 1);
    app.primary_thread_id = Some(root);

    // Every listing failed: report it instead of claiming nothing is running.
    app.show_process_tree(root, Vec::new(), vec!["Main [default]".to_string()]);
    assert!(app.chat_widget.active_view_id().is_none());

    // The user opened another view while the listing was in flight.
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(/*selected*/ None));
    app.show_process_tree(root, entries(), Vec::new());
    assert_eq!(app.chat_widget.active_view_id(), Some(AGENT_PICKER_VIEW_ID));
}
