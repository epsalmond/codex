use super::*;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::unbounded_channel;

fn picker() -> (
    ListSelectionView,
    tokio::sync::mpsc::UnboundedReceiver<crate::app_event::AppEvent>,
) {
    let (tx, rx) = unbounded_channel();
    let params = SelectionViewParams {
        is_searchable: true,
        search_activation: SelectionSearchActivation::Slash,
        items: ["Main", "worker"]
            .into_iter()
            .map(|name| SelectionItem {
                selection_key: Some(name.into()),
                name: name.into(),
                search_value: Some(name.into()),
                dismiss_on_select: true,
                actions: vec![Box::new(move |tx| {
                    tx.send(crate::app_event::AppEvent::UpdateModel(name.into()))
                })],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    (
        ListSelectionView::new(
            params,
            AppEventSender::new(tx),
            crate::keymap::RuntimeKeymap::defaults().list,
        ),
        rx,
    )
}

#[test]
fn slash_search_esc_restore_shortcuts_and_refresh_preserves_mode() {
    let (mut view, mut rx) = picker();
    assert!(!view.handle_paste("worker".into()));
    assert!(
        view.build_rows()[0].name_prefix_spans[0]
            .content
            .contains("1.")
    );
    view.handle_key_event(KeyCode::Char('/').into());
    view.handle_key_event(KeyCode::Char('1').into());
    assert_eq!(view.search_query, "1");
    assert!(rx.try_recv().is_err());
    view.handle_key_event(KeyCode::Backspace.into());
    assert!(view.handle_paste("worker".into()));
    assert_eq!(view.filtered_indices, vec![1]);
    let (replacement, _) = picker();
    view.replace_items_preserving_state(replacement.items);
    assert_eq!(
        (
            view.search_active,
            view.search_query.as_str(),
            view.filtered_indices.as_slice()
        ),
        (true, "worker", &[1][..])
    );
    view.handle_key_event(KeyCode::Esc.into());
    assert_eq!(
        (
            view.search_active,
            view.search_query.as_str(),
            view.completion
        ),
        (false, "", None)
    );
    assert_eq!(view.selected_actual_idx(), Some(/*value*/ 1));
    view.handle_key_event(KeyCode::Char('1').into());
    assert!(
        matches!(rx.try_recv(), Ok(crate::app_event::AppEvent::UpdateModel(name)) if name == "Main")
    );
}

#[test]
fn empty_search_esc_returns_to_navigation_and_ctrl_c_dismisses() {
    let (mut view, _) = picker();
    view.handle_key_event(KeyCode::Char('/').into());
    view.handle_key_event(KeyCode::Esc.into());
    assert_eq!((view.search_active, view.completion), (false, None));
    view.handle_key_event(KeyCode::Esc.into());
    assert_eq!(view.completion, Some(ViewCompletion::Cancelled));
    let (mut view, _) = picker();
    view.handle_key_event(KeyCode::Char('/').into());
    view.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(view.completion, Some(ViewCompletion::Cancelled));
}
