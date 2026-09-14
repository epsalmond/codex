use super::SourceAccumulator;
use super::build_summary_input;
use super::prior_artifact_provenance;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::path::PathBuf;

fn handoff(body: &str, path: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("<codex_smart_compact_handoff>{body}</codex_smart_compact_handoff>"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![ContentItemKind(
                        "compaction.smart_handoff".to_string(),
                    )]),
                    ..Default::default()
                },
            ),
        },
        metadata: Some(CodexHarnessMetadata {
            smart_compact_artifact_path: Some(PathBuf::from(path)),
            ..Default::default()
        }),
    }
}

fn text(item: &ResponseItem) -> &str {
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected contextual message")
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("expected text content")
    };
    text
}

#[test]
fn newest_typed_prior_handoff_and_fresh_source_fit_bounded_input() {
    let mut history = (0..24)
        .map(|index| {
            handoff(
                &format!("old handoff {index} {}", "x".repeat(/*n*/ 700)),
                &format!("/tmp/old-{index}"),
            )
        })
        .collect::<Vec<_>>();
    history.push(handoff("LATEST_PRIOR_HANDOFF", "/tmp/latest-artifact"));
    let mut source = SourceAccumulator::default();
    source.record(
        "FRESH_ELIDED_SOURCE",
        "tool output",
        Path::new("/tmp/fresh-source"),
    );

    let input = build_summary_input(&history, &source);
    let rendered = input.iter().map(text).collect::<Vec<_>>();
    assert!(rendered.iter().all(|item| item.len() <= 900));
    assert!(rendered.iter().map(|item| item.len()).sum::<usize>() <= 12_000);
    assert!(
        rendered
            .iter()
            .any(|item| item.contains("LATEST_PRIOR_HANDOFF"))
    );
    assert!(
        rendered
            .iter()
            .any(|item| item.contains("/tmp/latest-artifact"))
    );
    assert!(
        rendered
            .iter()
            .any(|item| item.contains("FRESH_ELIDED_SOURCE"))
    );
}

#[test]
fn prior_artifact_provenance_keeps_newest_and_marks_omission() {
    let history = (0..32)
        .map(|index| {
            handoff(
                &format!("handoff {index}"),
                &format!("/tmp/artifact-{index}-{index}-{index}"),
            )
        })
        .collect::<Vec<_>>();
    let provenance = prior_artifact_provenance(&history);
    assert!(provenance.len() <= 420);
    assert!(provenance.contains("/tmp/artifact-31-31-31"));
    assert!(provenance.contains("omitted"));
    assert_eq!(
        provenance.lines().next(),
        Some("Prior durable handoff artifacts (host provenance):")
    );
}
