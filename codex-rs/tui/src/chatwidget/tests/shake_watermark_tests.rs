use super::*;
use codex_app_server_protocol::ThreadContextTokenBasis;
use codex_app_server_protocol::ThreadContextUsage;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::ThreadTokenUsageUpdatedNotification;
use codex_app_server_protocol::TokenUsageBreakdown;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn status_watermark_follows_captured_events_and_clears_with_context() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let tokens = TokenUsageBreakdown {
        total_tokens: 42,
        input_tokens: 42,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
    };
    let mut rows = Vec::new();
    for watermark in [Some(/*value*/ 42), Some(/*value*/ 0), None] {
        chat.handle_server_notification(
            ServerNotification::ThreadTokenUsageUpdated(ThreadTokenUsageUpdatedNotification {
                thread_id: ThreadId::new().to_string(),
                turn_id: "turn".to_string(),
                token_usage: ThreadTokenUsage {
                    total: tokens.clone(),
                    last: tokens.clone(),
                    model_context_window: Some(/*value*/ 272_000),
                },
                context_usage: Some(ThreadContextUsage {
                    active_tokens: 42,
                    basis: ThreadContextTokenBasis::Estimate,
                    last_reduction: None,
                    selected_model: None,
                    child_policy_enabled: None,
                    child_active_cap_tokens: None,
                    model_window_tokens: Some(/*value*/ 272_000),
                    observed_at: None,
                    provider_usage_at: None,
                    shake_watermark: watermark,
                }),
            }),
            /*replay_kind*/ None,
        );
        assert!(op_rx.try_recv().is_err());
        chat.add_status_output(
            /*refreshing_rate_limits*/ false, /*request_id*/ None,
        );
        let rendered = drain_insert_history(&mut rx)
            .into_iter()
            .flatten()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        let context = rendered
            .iter()
            .position(|line| line.contains("Context window:"))
            .expect("context row");
        assert!(rendered[context + 1].contains("Shake watermark:"));
        rows.push(rendered[context].trim().to_string());
        rows.push(rendered[context + 1].trim().to_string());
    }
    insta::assert_snapshot!("status_shake_watermark", rows.join("\n"));
    chat.clear_token_usage();
    assert_eq!(chat.context_snapshot, None);
}
