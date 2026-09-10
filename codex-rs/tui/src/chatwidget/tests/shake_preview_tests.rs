use super::*;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::shake::ShakePreview;

fn preview() -> ShakePreview {
    ShakePreview {
        fingerprint: "measured-history".to_string(),
        tokens_before: 118_000,
        tokens_after: 71_000,
        tool_outputs: 24,
        text_blocks: 8,
        images: 0,
        thinking_blocks: 0,
        unavailable_reason: None,
    }
}

#[tokio::test]
async fn shake_preview_confirms_only_after_selection() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.input_queue.user_turn_pending_start = true;
    chat.show_shake_preview(thread_id, ShakeMode::Elide, preview());
    assert!(!chat.input_queue.user_turn_pending_start);
    assert!(rx.try_recv().is_err());
    assert_chatwidget_snapshot!("shake_preview", render_bottom_popup(&chat, /*width*/ 90));
    assert_chatwidget_snapshot!(
        "shake_preview_narrow",
        render_bottom_popup(&chat, /*width*/ 45)
    );

    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    let AppEvent::SubmitThreadOp {
        thread_id: target,
        op,
    } = rx.try_recv().unwrap()
    else {
        panic!("expected confirmed shake");
    };
    assert_eq!(
        (target, op),
        (
            thread_id,
            Op::Shake {
                mode: ShakeMode::Elide,
                expected_fingerprint: "measured-history".to_string(),
            }
        )
    );
}

#[tokio::test]
async fn shake_preview_cancel_and_escape_leave_input_available() {
    for cancel_key in [KeyCode::Esc, KeyCode::Enter] {
        let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        chat.show_shake_preview(thread_id, ShakeMode::Elide, preview());
        if cancel_key == KeyCode::Enter {
            chat.handle_key_event(KeyEvent::from(KeyCode::Down));
        }
        chat.handle_key_event(KeyEvent::from(cancel_key));
        assert!(rx.try_recv().is_err());
        assert!(!chat.input_queue.user_turn_pending_start);
        assert!(!chat.bottom_pane.has_active_view());
    }
}

#[tokio::test]
async fn shake_preview_noop_and_unavailable_do_not_offer_confirmation() {
    for reason in [
        None,
        Some("Elide requires a persistent thread.".to_string()),
    ] {
        let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        let mut preview = preview();
        preview.tool_outputs = 0;
        preview.text_blocks = 0;
        preview.tokens_after = preview.tokens_before;
        preview.unavailable_reason = reason;
        chat.show_shake_preview(thread_id, ShakeMode::Elide, preview);
        assert!(!chat.bottom_pane.has_active_view());
        assert!(!chat.input_queue.user_turn_pending_start);
        assert!(!drain_insert_history(&mut rx).is_empty());
    }
}

#[tokio::test]
async fn shake_preview_images_and_thinking_describe_recovery_limits() {
    for mode in [ShakeMode::Images, ShakeMode::Thinking] {
        let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        let mut preview = preview();
        preview.tool_outputs = 0;
        preview.text_blocks = 0;
        preview.images = u32::from(mode == ShakeMode::Images);
        preview.thinking_blocks = u32::from(mode == ShakeMode::Thinking);
        chat.show_shake_preview(thread_id, mode, preview);
        assert_chatwidget_snapshot!(
            format!("shake_preview_{}", mode.as_str()),
            render_bottom_popup(&chat, /*width*/ 90)
        );
    }
}
