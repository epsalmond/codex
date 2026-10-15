//! Browser and desktop handoff output and clear-screen history reset behavior.

use super::*;
use crate::app::tests::make_test_app_with_channels;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn open_url_in_browser_success_shows_full_url_in_history() {
    let (mut app, mut rx, _op_rx) = make_test_app_with_channels().await;
    let url = "https://example.com/review/123?tab=files&line=42#discussion";
    let mut opened_url = None;

    app.open_url_in_browser_with(url.to_string(), |url| {
        opened_url = Some(url.to_string());
        Ok(())
    });

    assert_eq!(opened_url.as_deref(), Some(url));
    let cell = match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => cell,
        other => panic!("expected InsertHistoryCell event, got {other:?}"),
    };
    assert_eq!(
        render_cell(cell.as_ref()),
        format!("• Browser launch requested for {url}."),
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn open_url_in_browser_failure_shows_only_error_with_full_url() {
    let (mut app, mut rx, _op_rx) = make_test_app_with_channels().await;
    let url = "https://example.com/review/123?tab=files&line=42#discussion";
    let mut opened_url = None;

    app.open_url_in_browser_with(url.to_string(), |url| {
        opened_url = Some(url.to_string());
        Err(std::io::Error::other("launch failed"))
    });

    assert_eq!(opened_url.as_deref(), Some(url));
    let cell = match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => cell,
        other => panic!("expected InsertHistoryCell event, got {other:?}"),
    };
    assert_eq!(
        render_cell(cell.as_ref()),
        format!("■ Failed to open browser for {url}: launch failed"),
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn desktop_thread_opened_history_snapshot() {
    let cell = history_cell::new_info_event(
        DESKTOP_THREAD_OPENED_MESSAGE.to_string(),
        /*hint*/ None,
    );

    insta::assert_snapshot!("desktop_thread_opened_history", render_cell(&cell));
}

#[test]
fn desktop_thread_open_error_history_snapshot() {
    let cell = history_cell::new_error_event(desktop_thread_open_error_message("launch failed"));

    insta::assert_snapshot!("desktop_thread_open_error_history", render_cell(&cell));
}

fn render_cell(cell: &(impl HistoryCell + ?Sized)) -> String {
    let lines = cell.display_lines(/*width*/ 80);
    lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn owned_clear_resets_navigation_and_retains_one_fresh_header() -> Result<()> {
    let (mut app, _rx, _op_rx) = make_test_app_with_channels().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;
    app.insert_history_cell(
        &mut tui,
        Box::new(PlainHistoryCell::new(vec!["old".into()])),
    );
    app.transcript_view
        .jump_to_entry(&app.transcript_cells, /*index*/ 0);
    app.reset_transcript_state_after_clear();
    app.queue_clear_ui_header(&mut tui);
    app.queue_clear_ui_header(&mut tui);

    assert_eq!(app.transcript_cells.len(), 1);
    assert_eq!(
        app.transcript_cells[0].display_lines(/*width*/ 80),
        app.clear_ui_header_lines(/*width*/ 80),
    );
    app.chat_widget.set_raw_output_mode(/*enabled*/ true);
    assert_eq!(
        app.transcript_cells[0].raw_lines(),
        app.clear_ui_header_lines(/*width*/ 80),
    );
    assert!(app.transcript_view.is_following());
    assert!(tui.pending_history_lines_for_test().is_empty());
    tui.set_owned_screen(/*owned*/ false)?;
    Ok(())
}
