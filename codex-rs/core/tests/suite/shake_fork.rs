use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::AgentPolling;
use codex_features::Feature;
use codex_history::CompactedItem;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use core_test_support::ThreadIdle;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use test_case::test_case;

fn decoded_body(request: &wiremock::Request) -> Vec<u8> {
    if request
        .headers
        .get("content-encoding")
        .is_some_and(|encoding| encoding == "zstd")
    {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body))
            .expect("decode compressed request")
    } else {
        request.body.clone()
    }
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    String::from_utf8(decoded_body(request))
        .expect("request body is UTF-8")
        .contains(text)
}

fn latest_checkpoint(path: &Path) -> Result<CompactedItem> {
    fs::read_to_string(path)?
        .lines()
        .map(serde_json::from_str::<RolloutItem>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::Compacted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .next_back()
        .context("a persisted Shake checkpoint")
}

#[derive(Clone, Copy)]
enum ParentSeal {
    Valid,
    Corrupt,
    FilteredCorruption,
}

#[test_case("all", ParentSeal::Valid; "full fork dispatches")]
#[test_case("2", ParentSeal::Valid; "bounded fork dispatches")]
#[test_case("all", ParentSeal::Corrupt; "full fork rejects corrupt parent")]
#[test_case("2", ParentSeal::Corrupt; "bounded fork rejects corrupt parent")]
#[test_case("all", ParentSeal::FilteredCorruption; "filtering cannot repair corrupt seal")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shake_checkpoint_survives_subagent_fork(
    fork_turns: &str,
    parent_seal: ParentSeal,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(start_mock_server()).await;
    let seed = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "seed the Shake fork"),
        sse(vec![
            ev_response_created("seed-response"),
            ev_assistant_message("seed-message", &"mutable tail ".repeat(/*n*/ 6_000)),
            ev_completed("seed-response"),
        ]),
    )
    .await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let fixture = Box::pin(
        test_codex()
            .with_extensions(Arc::new(extensions.build()))
            .with_model("gpt-5.2")
            .with_session_source(SessionSource::Cli)
            .with_history_mode(ThreadHistoryMode::Legacy)
            .with_config(move |config| {
                config
                    .features
                    .enable(Feature::Collab)
                    .expect("enable collab");
                if matches!(parent_seal, ParentSeal::FilteredCorruption) {
                    config
                        .features
                        .disable(Feature::MultiAgentV2)
                        .expect("disable v2");
                } else {
                    config
                        .features
                        .enable(Feature::MultiAgentV2)
                        .expect("enable v2");
                }
                config.multi_agent_v2.agent_polling = AgentPolling::Enabled;
                config.model_provider.request_max_retries = Some(0);
                config.model_provider.stream_max_retries = Some(0);
                config.model_provider.supports_websockets = false;
            })
            .build_with_auto_env(&server),
    )
    .await?;
    Box::pin(fixture.submit_turn("seed the Shake fork")).await?;
    ThreadIdle::wait(&fixture.codex).await;
    seed.single_request();
    let preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    Box::pin(fixture.codex.submit(Op::Shake {
        mode: ShakeMode::Elide,
        expected_fingerprint: Some(preview.fingerprint),
    }))
    .await?;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;
    fixture.codex.flush_rollout().await?;
    let parent_path = fixture.codex.rollout_path().context("parent rollout")?;
    let original_checkpoint = latest_checkpoint(&parent_path)?;
    let original_state = original_checkpoint
        .shake_history_state
        .as_ref()
        .context("parent seal")?;
    assert!(original_state.watermark > 0);
    let history = original_checkpoint
        .replacement_history
        .as_ref()
        .context("checkpoint history")?;
    assert!(
        history
            .iter()
            .take(original_state.watermark as usize)
            .any(|item| item
                .metadata
                .as_ref()
                .is_some_and(|metadata| { metadata.user_input_order.is_some() }))
    );

    if !matches!(parent_seal, ParentSeal::Valid) {
        // Damage only the stored source. The live parent's verified history can still dispatch
        // the spawn call, while its child's snapshot must reject the original invalid seal.
        let contents = fs::read_to_string(&parent_path)?;
        let lines = contents
            .lines()
            .map(|line| {
                let mut item: Value = serde_json::from_str(line)?;
                if item["type"] == "compacted" {
                    let mut checkpoint: CompactedItem =
                        serde_json::from_value(item["payload"].clone())?;
                    match parent_seal {
                        ParentSeal::Valid => {}
                        ParentSeal::Corrupt => {
                            if let Some(state) = &mut checkpoint.shake_history_state {
                                state.sealed_prefix_digest = "0".repeat(/*n*/ 40);
                            }
                        }
                        ParentSeal::FilteredCorruption => {
                            // V1 leaves the original prefix unchanged after dropping this item.
                            // Sanitizing first would accidentally accept the invalid source seal.
                            if let Some(history) = &mut checkpoint.replacement_history {
                                history.insert(
                                    /*index*/ 0,
                                    ResponseItemEnvelope::new(serde_json::from_value(
                                        json!({"type": "agent_message", "author": "/root",
                                        "recipient": "/root/worker", "content": []}),
                                    )?),
                                );
                            }
                        }
                    }
                    item["payload"] = serde_json::to_value(checkpoint)?;
                }
                serde_json::to_string(&item)
            })
            .collect::<Result<Vec<_>, _>>()?;
        fs::write(&parent_path, format!("{}\n", lines.join("\n")))?;
    }
    let source_checkpoint = latest_checkpoint(&parent_path)?;
    if matches!(parent_seal, ParentSeal::FilteredCorruption) {
        let mut sanitized = source_checkpoint.clone();
        sanitized
            .replacement_history
            .as_mut()
            .expect("checkpoint replacement history")
            .remove(/*index*/ 0);
        assert_eq!(sanitized, original_checkpoint);
    }
    let (namespace, spawn_args) = if matches!(parent_seal, ParentSeal::FilteredCorruption) {
        (
            "multi_agent_v1",
            json!({
                "message": "dispatch the Shake fork child", "fork_context": true,
            }),
        )
    } else {
        (
            "collaboration",
            json!({
                "message": "dispatch the Shake fork child", "task_name": "shake_child",
                "fork_turns": fork_turns,
            }),
        )
    };
    let spawn_args = serde_json::to_string(&spawn_args)?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "spawn from the sealed checkpoint")
                && !body_contains(request, "shake-spawn-call")
        },
        sse(vec![
            ev_response_created("spawn-response"),
            ev_function_call_with_namespace(
                "shake-spawn-call",
                namespace,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("spawn-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "shake-spawn-call")
                || body_contains(request, "dispatch the Shake fork child")
        },
        sse(vec![
            ev_response_created("parent-response"),
            ev_assistant_message("parent-message", "parent done"),
            ev_completed("parent-response"),
        ]),
    )
    .await;
    let parent_thread_id = fixture.session_configured.thread_id.to_string();
    let child_response = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, "dispatch the Shake fork child")
                && serde_json::from_slice::<Value>(&decoded_body(request)).expect("request JSON")
                    ["client_metadata"]["thread_id"]
                    .as_str()
                    .is_some_and(|thread_id| thread_id != parent_thread_id)
        },
        sse(vec![
            ev_response_created("child-response"),
            ev_assistant_message("child-message", "child dispatched"),
            ev_completed("child-response"),
        ]),
    )
    .await;
    fixture
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "spawn from the sealed checkpoint".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let child_id = Box::pin(wait_for_event_match(&fixture.codex, |event| match event {
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::SubAgentActivity(activity)
                if activity.kind == SubAgentActivityKind::Started =>
            {
                Some(activity.agent_thread_id)
            }
            TurnItem::CollabAgentToolCall(call) => call.receiver_thread_ids.first().copied(),
            _ => None,
        },
        _ => None,
    }))
    .await;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    }))
    .await;
    let child = fixture.thread_manager.get_thread(child_id).await?;
    let child_event = Box::pin(wait_for_event(&child, |event| {
        matches!(event, EventMsg::Error(_) | EventMsg::TurnComplete(_))
    }))
    .await;
    match parent_seal {
        ParentSeal::Valid => {
            assert!(
                matches!(child_event, EventMsg::TurnComplete(_)),
                "valid inherited checkpoint must permit dispatch: {child_event:?}"
            );
            let request = child_response.single_request();
            assert_eq!(
                request.body_json()["client_metadata"]["thread_id"],
                child_id.to_string()
            );
            assert!(request.body_contains_text("seed the Shake fork"));
            child.flush_rollout().await?;
            let checkpoint = latest_checkpoint(&child.rollout_path().context("child rollout")?)?;
            let state = checkpoint.shake_history_state.context("child seal")?;
            assert_ne!(state.epoch_id, original_state.epoch_id);
            assert_eq!(state.watermark, 0);
        }
        ParentSeal::Corrupt | ParentSeal::FilteredCorruption => {
            let EventMsg::Error(error) = child_event else {
                anyhow::bail!("corrupt inherited checkpoint dispatched: {child_event:?}");
            };
            assert!(
                error
                    .message
                    .contains("Shake sealed history state is invalid")
            );
            assert!(
                child_response.requests().iter().all(|request| {
                    request.body_json()["client_metadata"]["thread_id"] != child_id.to_string()
                }),
                "the child with an invalid seal must not dispatch a Responses request"
            );
            child.flush_rollout().await?;
            let checkpoint = latest_checkpoint(&child.rollout_path().context("child rollout")?)?;
            assert_eq!(
                (
                    checkpoint.replacement_history,
                    checkpoint.shake_history_state
                ),
                (
                    source_checkpoint.replacement_history.clone(),
                    source_checkpoint.shake_history_state.clone(),
                )
            );
        }
    }
    fixture.codex.flush_rollout().await?;
    assert_eq!(latest_checkpoint(&parent_path)?, source_checkpoint);
    child.shutdown_and_wait().await?;
    fixture.codex.shutdown_and_wait().await?;
    Ok(())
}
