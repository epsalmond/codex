//! Core integration coverage for Astra's explicit summary-assisted Elide path.

use anyhow::Context;
use anyhow::Result;
use codex_config::config_toml::AutoShakeThresholdToml;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_history::RolloutItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::Duration;
use wiremock::MockServer;

const ASTRA: &str = "gpt-6-astra";
const LUNA: &str = "gpt-5.6-luna";
const SOURCE: &str = "SMART_COMPACT_SOURCE_MARKER";
const SECOND_SOURCE: &str = "SMART_COMPACT_SECOND_SOURCE_MARKER";
const SUMMARY: &str = "SMART_COMPACT_SUMMARY_MARKER";
const SECOND_SUMMARY: &str = "SMART_COMPACT_SECOND_SUMMARY_MARKER";
const FIRST: &str = "SMART_COMPACT_FIRST_PROMPT";
const LAST: &str = "SMART_COMPACT_LAST_PROMPT";
const RESUMED: &str = "SMART_COMPACT_RESUMED_PROMPT";

fn summary_text(marker: &str, decision: &str, artifact_ref: &str) -> String {
    let suffix = format!("{} SMART_COMPACT_UTF8_SUFFIX", "界é長".repeat(100));
    format!(
        "### Goal\n- {marker}\n\n### Decisions\n- {decision}\n\n### Open threads\n- {suffix}\n\n### Files\n- {artifact_ref}\n\n### Commands\n- (none stated)\n\n### User context needed\n- Keep the latest user request intact."
    )
}

fn completed(id: &str, input_tokens: i64) -> Value {
    json!({"type":"response.completed","response":{"id":id,"usage":{
        "input_tokens":input_tokens,"input_tokens_details":null,"output_tokens":10,
        "output_tokens_details":null,"total_tokens":input_tokens + 10
    }}})
}

fn history(call_id: &str, source: &str) -> Result<Vec<ResponseItem>> {
    Ok(vec![
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":FIRST}]}),
        json!({"type":"function_call","id":call_id,"call_id":format!("{call_id}-exec"),"name":"exec_command","arguments":"{}"}),
        json!({"type":"function_call_output","call_id":format!("{call_id}-exec"),"output":format!("{source}\n").repeat(/*n*/ 8_000)}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"tail context ".repeat(/*n*/ 6_000)}]}),
    ]
    .into_iter()
    .map(serde_json::from_value)
    .collect::<std::result::Result<_, _>>()?)
}

fn configure(config: &mut codex_core::config::Config) {
    config.model_context_window = Some(100_000);
    config.auto_shake.models.insert(
        ASTRA.to_string(),
        codex_core::config::AutoShakeModelConfig {
            threshold: Some(AutoShakeThresholdToml::Percent(40)),
            min_elidable_percent: Some(1),
        },
    );
}

fn configure_skip(config: &mut codex_core::config::Config, manual: bool) {
    configure(config);
    let (family, threshold) = if manual {
        (ASTRA, AutoShakeThresholdToml::Off)
    } else {
        (LUNA, AutoShakeThresholdToml::Percent(40))
    };
    config.auto_shake.models.insert(
        family.to_string(),
        codex_core::config::AutoShakeModelConfig {
            threshold: Some(threshold),
            min_elidable_percent: None,
        },
    );
}

async fn turn(codex: &codex_core::CodexThread, prompt: &str) -> Result<()> {
    let submission = codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let turn_id = match submission {
        TurnInputSubmission::Started { turn_id } | TurnInputSubmission::Steered { turn_id } => {
            turn_id
        }
        TurnInputSubmission::NotSubmitted { reason } => {
            anyhow::bail!("turn input was not submitted: {reason:?}")
        }
    };
    wait_for_event(
        codex,
        |event| matches!(event, EventMsg::TurnComplete(completed) if completed.turn_id == turn_id),
    )
    .await;
    Ok(())
}

async fn smart_compact(codex: &codex_core::CodexThread) -> Result<()> {
    codex
        .submit(Op::Shake {
            mode: ShakeMode::SmartCompact,
            expected_fingerprint: None,
        })
        .await?;
    wait_for_event(codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.contains("smart-compact:"))
    })
    .await;
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(())
}

fn text(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message { content, .. } = item else {
        return None;
    };
    content.iter().find_map(|item| match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => Some(text.as_str()),
        ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
    })
}

fn checkpoint(path: &Path) -> Result<Vec<ResponseItem>> {
    let mut found = None;
    for line in fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.is_empty())
    {
        let entry = codex_rollout::parse_rollout_line(line)?;
        if let RolloutItem::Compacted(compacted) = entry.item
            && let Some(history) = compacted.replacement_history
        {
            found = Some(history.into_iter().map(|item| item.item).collect());
        }
    }
    found.context("smart compact checkpoint")
}

fn assert_handoff(item: &ResponseItem) {
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = item
    else {
        panic!("handoff should be a message: {item:?}");
    };
    assert_eq!(role, "user");
    assert_eq!(content.len(), 1);
    let body = text(item)
        .and_then(|text| text.strip_prefix("<codex_smart_compact_handoff>"))
        .and_then(|body| body.strip_suffix("</codex_smart_compact_handoff>"))
        .map(str::trim)
        .expect("rendered handoff body");
    for heading in [
        "### Goal",
        "### Decisions",
        "### Open threads",
        "### Files",
        "### Commands",
        "### User context needed",
    ] {
        assert!(body.contains(heading));
    }
    assert!(body.contains(SUMMARY));
    let kinds = internal_chat_message_metadata_passthrough
        .as_ref()
        .and_then(|metadata| metadata.content_item_kinds.as_ref())
        .expect("handoff content item kind");
    assert_eq!(kinds.len(), 1);
    assert_eq!(kinds[0].0, "compaction.smart_handoff");
}

fn no_tools(body: &Value) {
    assert_eq!(body.get("tools"), None);
    for item in body["input"].as_array().expect("summary input") {
        if item["type"] == "additional_tools" {
            assert_eq!(item["tools"], json!([]));
        }
    }
}

fn artifact_path(handoff: &str) -> Result<&str> {
    handoff
        .split("Artifact path: ")
        .nth(1)
        .and_then(|path| path.strip_suffix("</codex_smart_compact_handoff>"))
        .map(str::trim)
        .context("artifact path in saved handoff")
}

async fn skip_fixture(server: &MockServer, model: &str, manual: bool) -> Result<TestCodex> {
    let mut builder = test_codex()
        .with_model(model)
        .with_config(move |config| configure_skip(config, manual));
    let fixture = Box::pin(builder.build_with_auto_env(server)).await?;
    fixture
        .codex
        .inject_response_items(history("smart-call", SOURCE)?)
        .await?;
    Ok(fixture)
}

#[derive(Clone, Copy, Debug)]
enum Fallback {
    Failed,
    Malformed,
    Empty,
    Oversized,
}

fn fallback_response(case: Fallback) -> String {
    match case {
        Fallback::Failed => sse(vec![
            ev_assistant_message("partial", "partial summary before failure"),
            json!({
                "type": "response.failed",
                "response": {
                    "id": "failed",
                    "error": {"code": "server_error", "message": "no Luna"}
                }
            }),
        ]),
        Fallback::Malformed => sse(vec![
            ev_assistant_message(
                "malformed",
                "assistant output without the required headings",
            ),
            completed("malformed", 250),
        ]),
        Fallback::Empty => sse(vec![ev_completed("empty")]),
        Fallback::Oversized => sse(vec![
            ev_assistant_message("oversized", &format!("unstructured {}", "x".repeat(2_500))),
            completed("oversized", 300),
        ]),
    }
}

async fn explicit_fixture(
    server: &MockServer,
    summary: String,
) -> Result<(TestCodexBuilder, TestCodex, ResponseMock)> {
    let requests = mount_sse_sequence(
        server,
        vec![
            sse(vec![
                ev_assistant_message("first", "first"),
                completed("r1", 70_000),
            ]),
            summary,
            sse(vec![
                ev_assistant_message("last", "last"),
                completed("r3", 10_000),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_model(ASTRA)
        .with_config(|config| configure_skip(config, /*manual*/ true));
    let fixture = Box::pin(builder.build_with_auto_env(server)).await?;
    fixture
        .codex
        .inject_response_items(history("smart-call", SOURCE)?)
        .await?;
    turn(&fixture.codex, FIRST).await?;
    smart_compact(&fixture.codex).await?;
    let usage_after_smart = fixture
        .codex
        .token_usage_info()
        .await
        .context("usage after explicit smart compact")?;
    let model_context_window = usage_after_smart
        .model_context_window
        .context("model context window after smart compact")?;
    assert!(usage_after_smart.last_token_usage.total_tokens < model_context_window);
    turn(&fixture.codex, LAST).await?;
    Ok((builder, fixture, requests))
}

async fn automatic_fixture(server: &MockServer) -> Result<ResponseMock> {
    let requests = mount_sse_sequence(
        server,
        vec![
            sse(vec![
                ev_assistant_message("first", "first"),
                completed("r1", 70_000),
            ]),
            sse(vec![
                ev_assistant_message("last", "last"),
                completed("r2", 10_000),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_model(ASTRA).with_config(configure);
    let fixture = Box::pin(builder.build_with_auto_env(server)).await?;
    fixture
        .codex
        .inject_response_items(history("automatic-call", SOURCE)?)
        .await?;
    turn(&fixture.codex, FIRST).await?;
    turn(&fixture.codex, LAST).await?;
    Ok(requests)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn astra_explicit_smart_compact_uses_luna_and_persists_bounded_handoff() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let (mut builder, fixture, requests) = explicit_fixture(
        &server,
        sse(vec![
            ev_assistant_message(
                "luna",
                &summary_text(
                    SUMMARY,
                    "Preserve the user's current task.",
                    "(none stated)",
                ),
            ),
            completed("r2", 250),
        ]),
    )
    .await?;

    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].body_json()["model"], ASTRA);
    assert_eq!(requests[1].body_json()["model"], LUNA);
    assert_eq!(requests[2].body_json()["model"], ASTRA);
    let luna_body = requests[1].body_json();
    no_tools(&luna_body);
    assert_eq!(luna_body["reasoning"]["effort"], "medium");
    assert!(luna_body.to_string().contains(SOURCE));
    assert!(requests[1].has_content_kinds(&["compaction.smart_source"]));
    assert!(
        luna_body
            .to_string()
            .contains("<codex_smart_compact_source>")
    );
    assert!(
        luna_body
            .to_string()
            .contains("portable handoff after an explicit mechanical context shake")
    );
    assert!(luna_body.to_string().len() < 32 * 1024);
    let usage = fixture
        .codex
        .token_usage_info()
        .await
        .context("usage record")?;
    assert_eq!(usage.total_token_usage.total_tokens, 80_280);

    let survivor = requests[2].body_json().to_string();
    assert!(!survivor.contains(SOURCE));
    assert!(survivor.contains("[shaken ~"));
    assert!(survivor.contains("<codex_smart_compact_handoff>"));
    assert_eq!(survivor.matches(SUMMARY).count(), 1);
    assert!(survivor.contains("truncated"));
    let user_texts = requests[2].message_input_texts("user");
    let saved_handoff = user_texts
        .iter()
        .find(|text| text.contains(SUMMARY))
        .context("saved handoff in survivor request")?;
    assert!(saved_handoff.len() <= 900);
    let saved_artifact = artifact_path(saved_handoff)?;
    assert!(Path::new(saved_artifact).is_file());
    assert!(
        user_texts.iter().position(|text| text.contains(SUMMARY))
            < user_texts.iter().position(|text| text == LAST)
    );

    let artifact_dir = fixture
        .codex_home_path()
        .join("artifacts")
        .join(fixture.session_configured.thread_id.to_string());
    let artifacts: Vec<_> = fs::read_dir(&artifact_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    assert!(!artifacts.is_empty());
    let contents = artifacts
        .iter()
        .map(|artifact| -> Result<_> { Ok(fs::read_to_string(artifact)?) })
        .collect::<Result<Vec<_>>>()?;
    assert!(contents.iter().all(|body| body.len() <= 512 * 1024));
    assert!(contents.iter().any(|body| body.contains(SOURCE)));
    assert!(contents.iter().any(|body| body.contains(SUMMARY)));
    assert!(
        contents
            .iter()
            .any(|body| body.contains("SMART_COMPACT_UTF8_SUFFIX"))
    );
    let source_artifact = artifacts
        .iter()
        .find(|artifact| fs::read_to_string(artifact).is_ok_and(|body| body.contains(SOURCE)))
        .context("source artifact")?;
    let source_path = source_artifact.canonicalize()?.display().to_string();
    assert!(survivor.contains(&source_path));

    let path = fixture.codex.rollout_path().context("rollout path")?;
    let saved = checkpoint(&path)?;
    let handoffs: Vec<_> = saved
        .iter()
        .filter(|item| text(item).is_some_and(|text| text.contains(SUMMARY)))
        .collect();
    assert_eq!(handoffs.len(), 1);
    assert_handoff(handoffs[0]);
    assert!(
        saved
            .iter()
            .all(|item| !text(item).is_some_and(|text| text.contains(SOURCE)))
    );
    builder = builder
        .with_model(ASTRA)
        .with_config(|config| configure_skip(config, /*manual*/ true));
    let resumed = Box::pin(builder.restart(&server, &fixture)).await?;
    let resumed_request = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_assistant_message("resume", "resumed"),
            ev_completed("r4"),
        ])],
    )
    .await;
    turn(&resumed.codex, RESUMED).await?;
    let resumed_body = resumed_request.single_request().body_json().to_string();
    assert!(resumed_body.contains(SUMMARY));
    assert!(resumed_body.contains(LAST));
    assert!(
        resumed_body.contains(RESUMED),
        "resumed user inputs: {:?}",
        resumed_request
            .single_request()
            .message_input_texts("user")
            .iter()
            .rev()
            .take(3)
            .map(|text| text.chars().take(120).collect::<String>())
            .collect::<Vec<_>>()
    );
    assert!(!resumed_body.contains(SOURCE));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_explicit_smart_compact_summarizes_the_previous_handoff_and_artifact() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let (mut builder, fixture, first_requests) = explicit_fixture(
        &server,
        sse(vec![
            ev_assistant_message(
                "luna-1",
                &summary_text(
                    SUMMARY,
                    "Preserve the user's current task.",
                    "(none stated)",
                ),
            ),
            completed("r2", 250),
        ]),
    )
    .await?;
    let first_handoff = first_requests
        .requests()
        .into_iter()
        .flat_map(|request| request.message_input_texts("user"))
        .find(|text| text.contains(SUMMARY))
        .context("first saved handoff")?;
    let first_artifact = artifact_path(&first_handoff)?;
    assert!(Path::new(first_artifact).is_file());
    let resumed_requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("astra-resume", "resume"),
                completed("r4", 70_000),
            ]),
            sse(vec![
                ev_assistant_message(
                    "luna-2",
                    &summary_text(
                        SECOND_SUMMARY,
                        "Preserve the first handoff.",
                        first_artifact,
                    ),
                ),
                completed("r5", 250),
            ]),
            sse(vec![
                ev_assistant_message("astra-final", "second survivor"),
                ev_completed("r6"),
            ]),
        ],
    )
    .await;
    builder = builder
        .with_model(ASTRA)
        .with_config(|config| configure_skip(config, /*manual*/ true));
    let resumed = Box::pin(builder.restart(&server, &fixture)).await?;
    turn(&resumed.codex, RESUMED).await?;
    resumed
        .codex
        .inject_response_items(history("smart-call-2", SECOND_SOURCE)?)
        .await?;
    smart_compact(&resumed.codex).await?;
    turn(&resumed.codex, "SMART_COMPACT_SECOND_PROMPT").await?;

    let requests = resumed_requests.requests();
    assert_eq!(
        requests.len(),
        3,
        "resumed request models: {:?}",
        requests
            .iter()
            .map(|request| request.body_json()["model"].clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(requests[1].body_json()["model"], LUNA);
    assert!(requests[1].body_json().to_string().contains(SUMMARY));
    assert!(requests[1].body_json().to_string().contains(first_artifact));
    let final_request = requests[2].clone();
    assert!(
        final_request
            .body_json()
            .to_string()
            .contains(SECOND_SUMMARY)
    );
    assert!(final_request.body_json().to_string().contains(RESUMED));
    let handoff = final_request
        .message_input_texts("user")
        .into_iter()
        .find(|text| text.contains(SECOND_SUMMARY))
        .context("second saved handoff")?;
    assert!(fs::read_to_string(artifact_path(&handoff)?)?.contains(first_artifact));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_luna_handoff_keeps_mechanical_checkpoint() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
    let first_response = StreamingSseChunk {
        gate: None,
        body: sse(vec![
            json!({"type":"response.created","response":{"id":"r1"}}),
            ev_assistant_message("first", "first"),
            completed("r1", 70_000),
        ]),
    };
    let summary_response = vec![
        StreamingSseChunk {
            gate: None,
            body: sse(vec![
                json!({"type":"response.created","response":{"id":"summary"}}),
                ev_assistant_message(
                    "summary",
                    &summary_text(
                        SUMMARY,
                        "Preserve the user's current task.",
                        "(none stated)",
                    ),
                ),
            ]),
        },
        StreamingSseChunk {
            gate: Some(release_rx),
            body: sse(vec![ev_completed("summary")]),
        },
    ];
    let (streaming_server, _completions) =
        start_streaming_sse_server(vec![vec![first_response], summary_response]).await;
    let mut builder = test_codex()
        .with_model(ASTRA)
        .with_config(|config| configure_skip(config, /*manual*/ true));
    let fixture = builder
        .build_with_streaming_server(&streaming_server)
        .await?;
    fixture
        .codex
        .inject_response_items(history("smart-call", SOURCE)?)
        .await?;
    turn(&fixture.codex, FIRST).await?;
    fixture
        .codex
        .submit(Op::Shake {
            mode: ShakeMode::SmartCompact,
            expected_fingerprint: None,
        })
        .await?;
    streaming_server.wait_for_request_count(2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    fixture.codex.submit(Op::Interrupt).await?;
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    let path = fixture.codex.rollout_path().context("rollout path")?;
    let saved = checkpoint(&path)?;
    let saved_json = serde_json::to_string(&saved)?;
    assert!(!saved_json.contains(SUMMARY));
    assert!(!saved_json.contains(SOURCE));
    assert!(saved_json.contains("[shaken ~"));
    let artifact_dir = fixture
        .codex_home_path()
        .join("artifacts")
        .join(fixture.session_configured.thread_id.to_string());
    let artifacts = fs::read_dir(artifact_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    assert!(!artifacts.iter().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .contains("smart-compact-handoff")
    }));
    streaming_server.shutdown().await;
    Ok(())
}

#[test_case::test_case(Fallback::Failed; "request failure")]
#[test_case::test_case(Fallback::Malformed; "malformed output")]
#[test_case::test_case(Fallback::Empty; "empty output")]
#[test_case::test_case(Fallback::Oversized; "oversized output")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_failure_falls_back_to_mechanical_elide(case: Fallback) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let (_builder, fixture, requests) = explicit_fixture(&server, fallback_response(case)).await?;
    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].body_json()["model"], LUNA);
    let survivor = requests[2].body_json().to_string();
    assert!(!survivor.contains(SOURCE));
    assert!(survivor.contains("[shaken ~"));
    assert!(!survivor.contains(SUMMARY));
    let usage = fixture
        .codex
        .token_usage_info()
        .await
        .context("usage record")?;
    let expected_tokens = match case {
        Fallback::Failed | Fallback::Empty => 80_020,
        Fallback::Malformed => 80_280,
        Fallback::Oversized => 80_330,
    };
    assert_eq!(usage.total_token_usage.total_tokens, expected_tokens);
    Ok(())
}

#[test_case::test_case(LUNA; "luna automatic")]
#[test_case::test_case(ASTRA; "astra manual")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn luna_and_manual_elide_skip_summary_model(model: &str) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let manual = model == ASTRA;
    if manual {
        let server = Box::pin(core_test_support::responses::start_mock_server()).await;
        let fixture = skip_fixture(&server, model, /*manual*/ true).await?;
        fixture
            .codex
            .submit(Op::Shake {
                mode: ShakeMode::Elide,
                expected_fingerprint: None,
            })
            .await?;
        wait_for_event(&fixture.codex, |event| {
            matches!(event, EventMsg::Warning(warning) if warning.message.contains("shake:"))
        })
        .await;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );
        return Ok(());
    }
    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("luna-1", "first"),
                completed("r1", 70_000),
            ]),
            sse(vec![
                ev_assistant_message("luna-2", "last"),
                ev_completed("r2"),
            ]),
        ],
    )
    .await;
    let fixture = skip_fixture(&server, model, /*manual*/ false).await?;
    turn(&fixture.codex, FIRST).await?;
    turn(&fixture.codex, LAST).await?;
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.body_json()["model"] == model)
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.body_json().to_string().contains(SUMMARY))
    );
    assert!(!requests[1].body_json().to_string().contains(SOURCE));
    assert!(requests[1].body_json().to_string().contains("[shaken ~"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn astra_automatic_elide_never_requests_luna() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let requests = automatic_fixture(&server).await?.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        request.body_json()["model"] == ASTRA
            && !request.body_json().to_string().contains("smart_compact")
            && !request
                .body_json()
                .to_string()
                .contains("SMART_COMPACT_SUMMARY_MARKER")
    }));
    assert!(requests[1].body_json().to_string().contains("[shaken ~"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn luna_usage_exhausts_budget_before_survivor_sampling() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("first", "first"),
                completed("r1", 70_000),
            ]),
            sse(vec![
                ev_assistant_message(
                    "luna",
                    &summary_text(SUMMARY, "Keep working", "(none stated)"),
                ),
                completed("r2", 250),
            ]),
        ],
    )
    .await;
    let fixture = Box::pin(
        test_codex()
            .with_model(ASTRA)
            .with_config(|config| {
                configure_skip(config, /*manual*/ true);
                config.rollout_budget = Some(codex_core::config::RolloutBudgetConfig {
                    limit_tokens: 70_100,
                    reminder_at_remaining_tokens: vec![],
                    sampling_token_weight: 1.0,
                    prefill_token_weight: 1.0,
                });
            })
            .build_with_auto_env(&server),
    )
    .await?;
    fixture
        .codex
        .inject_response_items(history("budget-call", SOURCE)?)
        .await?;
    turn(&fixture.codex, FIRST).await?;
    fixture
        .codex
        .submit(Op::Shake {
            mode: ShakeMode::SmartCompact,
            expected_fingerprint: None,
        })
        .await?;
    wait_for_event(&fixture.codex, |event| matches!(event, EventMsg::Error(error)
        if error.codex_error_info == Some(codex_protocol::protocol::CodexErrorInfo::SessionBudgetExceeded)
    )).await;
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
    })
    .await;
    let usage_after_smart = fixture
        .codex
        .token_usage_info()
        .await
        .context("usage after budget-stopped smart compact")?;
    let model_context_window = usage_after_smart
        .model_context_window
        .context("model context window after budget stop")?;
    assert!(usage_after_smart.last_token_usage.total_tokens < model_context_window);
    assert_eq!(requests.requests().len(), 2);
    let usage = fixture
        .codex
        .token_usage_info()
        .await
        .context("usage record")?;
    assert_eq!(usage.total_token_usage.total_tokens, 70_270);
    let path = fixture.codex.rollout_path().context("rollout path")?;
    let saved = serde_json::to_string(&checkpoint(&path)?)?;
    assert!(saved.contains("[shaken ~"));
    assert!(!saved.contains(SOURCE));
    assert!(!saved.contains(SUMMARY));
    Ok(())
}
