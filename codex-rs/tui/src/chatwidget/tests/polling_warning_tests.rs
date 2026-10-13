use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn polling_guard_warning_is_visible_in_warnings() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.warning_display_state.startup_complete = true;
    chat.handle_server_notification(
        ServerNotification::Warning(WarningNotification {
            thread_id: None,
            message: "Repeated status checks returned no new information. Requesting a pause; use a completion notification or one bounded wait helper before continuing.".to_string(),
        }),
        /*replay_kind*/ None,
    );
    let cells: Vec<Arc<dyn HistoryCell>> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(Arc::from(cell)),
            _ => None,
        })
        .collect();
    assert_eq!(cells.len(), 1);
    chat.open_warnings(&cells);
    insta::assert_snapshot!(
        "polling_guard_warning",
        super::helpers::render_bottom_popup(&chat, /*width*/ 80)
    );
}
