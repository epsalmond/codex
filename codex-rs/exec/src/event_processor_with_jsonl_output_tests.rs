use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

#[test]
fn failed_turn_does_not_overwrite_output_last_message_file() {
    let tempdir = tempdir().expect("create tempdir");
    let output_path = tempdir.path().join("last-message.txt");
    std::fs::write(&output_path, "keep existing contents").expect("seed output file");

    let mut processor = EventProcessorWithJsonOutput::new(Some(output_path.clone()));

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: "msg-1".to_string(),
                text: "partial answer".to_string(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        },
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(processor.final_message(), Some("partial answer"));

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            context_usage: None,
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                root_turn_id: None,
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Failed,
                error: Some(codex_app_server_protocol::TurnError {
                    misalignment: None,
                    message: "turn failed".to_string(),
                    additional_details: None,
                    codex_error_info: None,
                }),
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(status, CodexStatus::InitiateShutdown);
    assert_eq!(processor.final_message(), None);

    EventProcessor::print_final_output(&mut processor).expect("final output should succeed");

    assert_eq!(
        std::fs::read_to_string(&output_path).expect("read output file"),
        "keep existing contents"
    );
}

#[tokio::test]
async fn failed_final_output_does_not_skip_cleanup() {
    let tempdir = tempdir().expect("create tempdir");
    let output_path = tempdir.path().join("missing").join("last-message.txt");
    let mut processor = EventProcessorWithJsonOutput::new(Some(output_path));
    processor.final_message = Some("final answer".to_string());
    processor.emit_final_message_on_shutdown = true;
    let cleanup_ran = AtomicBool::new(false);

    let (output_result, ()) =
        crate::event_processor::print_final_output_before_cleanup(&mut processor, async {
            cleanup_ran.store(true, Ordering::Release);
        })
        .await;

    assert!(output_result.is_err());
    assert!(cleanup_ran.load(Ordering::Acquire));
}

#[test]
fn final_output_reports_last_message_write_failure() {
    let tempdir = tempdir().expect("create tempdir");
    let output_path = tempdir.path().join("missing").join("last-message.txt");
    let mut processor = EventProcessorWithJsonOutput::new(Some(output_path));
    processor.final_message = Some("final answer".to_string());
    processor.emit_final_message_on_shutdown = true;

    let error = EventProcessor::print_final_output(&mut processor)
        .expect_err("missing parent directory should fail the write");

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn runtime_warning_emits_a_non_fatal_error_item() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::Warning(
        codex_app_server_protocol::WarningNotification {
            thread_id: Some("thread-1".to_string()),
            message: "invalid global instructions".to_string(),
        },
    ));

    assert_eq!(
        collected,
        CollectedThreadEvents {
            events: vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
                item: ExecThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::Error(ErrorItem {
                        message: "invalid global instructions".to_string(),
                    }),
                },
            })],
            status: CodexStatus::Running,
        }
    );
}

#[test]
fn mcp_tool_call_result_preserves_meta_in_jsonl_event() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::McpToolCall {
                id: "mcp-1".to_string(),
                server: "search service".to_string(),
                tool: "web_run".to_string(),
                status: McpToolCallStatus::Completed,
                arguments: json!({"search_query": [{"q": "OpenAI Codex CLI documentation"}]}),
                app_context: None,
                mcp_app_resource_uri: None,
                mcp_app_ui: None,
                plugin_id: None,
                read_only_hint: None,
                result: Some(Box::new(codex_app_server_protocol::McpToolCallResult {
                    content: vec![json!({"type": "text", "text": "search result"})],
                    structured_content: None,
                    meta: Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]})),
                })),
                error: None,
                duration_ms: Some(42),
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        },
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(collected.events.len(), 1);

    let ThreadEvent::ItemCompleted(ItemCompletedEvent { item }) = &collected.events[0] else {
        panic!("expected item.completed event");
    };
    let ThreadItemDetails::McpToolCall(item) = &item.details else {
        panic!("expected MCP tool call item");
    };
    let result = item.result.as_ref().expect("expected MCP tool result");
    assert_eq!(
        result.meta,
        Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]}))
    );

    let serialized = serde_json::to_value(&collected.events[0]).expect("serialize event");
    assert_eq!(
        serialized["item"]["result"]["_meta"],
        json!({"raw_messages": [{"ref_id": "turn0search0"}]})
    );
    assert!(serialized["item"]["result"].get("meta").is_none());
}

fn root_turn_completed(
    turn_id: &str,
    status: TurnStatus,
    message: Option<&str>,
) -> ServerNotification {
    ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
        context_usage: None,
        thread_id: "thread-1".to_string(),
        turn: codex_app_server_protocol::Turn {
            id: turn_id.to_string(),
            items_view: codex_app_server_protocol::TurnItemsView::Full,
            items: message
                .map(|text| ThreadItem::AgentMessage {
                    id: format!("{turn_id}-msg"),
                    text: text.to_string(),
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                })
                .into_iter()
                .collect(),
            status,
            error: None,
            started_at: None,
            completed_at: Some(0),
            duration_ms: None,
        },
    })
}

/// A drained exec run emits one `turn.completed` per completed root turn, and a later failed or
/// interrupted wake turn leaves the last completed answer in `--output-last-message`.
#[test]
fn failed_or_interrupted_wake_turn_keeps_the_last_completed_answer() {
    for status in [TurnStatus::Failed, TurnStatus::Interrupted] {
        let tempdir = tempdir().expect("create tempdir");
        let output_path = tempdir.path().join("last-message.txt");
        let mut processor = EventProcessorWithJsonOutput::new(Some(output_path.clone()));

        let first = processor.collect_thread_events(root_turn_completed(
            "turn-1",
            TurnStatus::Completed,
            Some("answer A"),
        ));
        assert!(
            first
                .events
                .iter()
                .any(|event| matches!(event, ThreadEvent::TurnCompleted(_)))
        );
        processor.final_message = Some("partial B".to_string());
        processor.process_server_notification(root_turn_completed(
            "turn-2",
            status.clone(),
            /*message*/ None,
        ));

        assert_eq!(processor.final_message(), Some("answer A"), "{status:?}");
        EventProcessor::print_final_output(&mut processor).expect("final output should succeed");
        assert_eq!(
            std::fs::read_to_string(&output_path).expect("read output file"),
            "answer A",
            "{status:?}"
        );
    }
}

#[test]
fn completed_wake_turn_replaces_the_earlier_answer() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);
    let mut turn_completed_events = 0;
    for (turn_id, message) in [("turn-1", "answer A"), ("turn-2", "answer B")] {
        let collected = processor.collect_thread_events(root_turn_completed(
            turn_id,
            TurnStatus::Completed,
            Some(message),
        ));
        turn_completed_events += collected
            .events
            .iter()
            .filter(|event| matches!(event, ThreadEvent::TurnCompleted(_)))
            .count();
    }

    assert_eq!(turn_completed_events, 2);
    assert_eq!(processor.final_message(), Some("answer B"));
}
