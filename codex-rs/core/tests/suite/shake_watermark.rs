use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_history::RolloutItem;
use codex_history::ShakeHistoryState;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ShakeMode;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

const SEALED_TEST_IMAGE: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

fn edit_sealed_checkpoint_image(rollout_path: &Path) -> Result<()> {
    let contents = fs::read_to_string(rollout_path)?;
    let mut edited = false;
    let mut lines = Vec::new();
    for line in contents.lines() {
        let mut item: Value = serde_json::from_str(line)?;
        if item["type"] == "compacted"
            && item["payload"]["shake_history_state"].is_object()
            && let Some(history) = item["payload"]["replacement_history"].as_array_mut()
        {
            'history: for envelope in history {
                let Some(content) = envelope["content"].as_array_mut() else {
                    continue;
                };
                for content_item in content {
                    if content_item["type"] != "input_image" {
                        continue;
                    }
                    if content_item.get("image_url").is_some() {
                        content_item["image_url"] =
                            json!("https://example.invalid/edited-after-seal.png");
                        edited = true;
                        break 'history;
                    }
                    if content_item.get("file_id").is_some() {
                        content_item["file_id"] = json!("edited-after-seal");
                        edited = true;
                        break 'history;
                    }
                }
            }
        }
        lines.push(serde_json::to_string(&item)?);
    }
    anyhow::ensure!(edited, "checkpoint should contain sealed image media");
    let mut rewritten = lines.join("\n");
    rewritten.push('\n');
    fs::write(rollout_path, rewritten)?;
    Ok(())
}

fn latest_shake_state(rollout_path: &Path) -> Result<ShakeHistoryState> {
    fs::read_to_string(rollout_path)?
        .lines()
        .filter_map(|line| serde_json::from_str::<RolloutItem>(line).ok())
        .filter_map(|item| match item {
            RolloutItem::Compacted(compacted) => compacted.shake_history_state,
            _ => None,
        })
        .next_back()
        .context("rollout should contain a persisted shake watermark")
}

fn rollout_path(codex: &CodexThread) -> Result<PathBuf> {
    codex.rollout_path().context("rollout path")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_preserves_a_verified_shake_seal_after_media_preparation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(start_mock_server()).await;
    let mut builder = test_codex().with_model("gpt-5.2");
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    let history = vec![
        serde_json::from_value(json!({
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "keep the sealed media boundary"},
                {"type": "input_image", "image_url": SEALED_TEST_IMAGE}
            ]
        }))?,
        serde_json::from_value(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "mutable tail ".repeat(6_000)}]
        }))?,
    ];
    Box::pin(fixture.codex.inject_response_items(history)).await?;

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
    let original_state = latest_shake_state(&rollout_path(&fixture.codex)?)?;
    assert!(original_state.watermark > 0);

    let resumed = Box::pin(builder.restart(&server, &fixture))
        .await
        .context("resume a checkpoint containing sealed remote media")?;
    let resumed_preview = resumed.codex.preview_shake(ShakeMode::Elide).await?;
    Box::pin(resumed.codex.submit(Op::Shake {
        mode: ShakeMode::Elide,
        expected_fingerprint: Some(resumed_preview.fingerprint),
    }))
    .await?;
    Box::pin(wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;
    let rebased_state = latest_shake_state(&rollout_path(&resumed.codex)?)?;
    assert_eq!(rebased_state, original_state);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_rejects_an_edited_sealed_media_checkpoint() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(start_mock_server()).await;
    let mut builder = test_codex().with_model("gpt-5.2");
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    let history = vec![
        serde_json::from_value(json!({
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "keep the sealed media boundary"},
                {"type": "input_image", "image_url": SEALED_TEST_IMAGE}
            ]
        }))?,
        serde_json::from_value(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "mutable tail ".repeat(6_000)}]
        }))?,
    ];
    Box::pin(fixture.codex.inject_response_items(history)).await?;

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

    let path = rollout_path(&fixture.codex)?;
    let sealed_state = latest_shake_state(&path)?;
    assert!(sealed_state.watermark > 0);
    fixture.codex.ensure_rollout_materialized().await;
    fixture.codex.flush_rollout().await?;
    fixture.codex.shutdown_and_wait().await?;
    edit_sealed_checkpoint_image(&path)?;
    anyhow::ensure!(
        fs::metadata(&path)?.len() > 0,
        "edited checkpoint must not be empty"
    );
    assert_eq!(latest_shake_state(&path)?, sealed_state);

    let resumed = Box::pin(builder.resume(&server, fixture.home.clone(), path)).await?;
    let preview_error = resumed
        .codex
        .preview_shake(ShakeMode::Elide)
        .await
        .expect_err("the edited checkpoint must not be accepted under its old digest");
    assert!(
        preview_error
            .to_string()
            .contains("Stored Shake history seal is invalid")
    );

    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}
