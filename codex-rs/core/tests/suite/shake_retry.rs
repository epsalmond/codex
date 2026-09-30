use anyhow::Context;
use anyhow::Result;
use codex_history::RolloutItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ShakeMode;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// Regular `.log` files directly under `dir`, or an empty vector if it does not exist.
fn artifact_log_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .collect()
}

fn latest_shake_watermark(rollout_path: &Path) -> Result<usize> {
    let contents = fs::read_to_string(rollout_path)?;
    for line in contents.lines().rev() {
        if let RolloutItem::Compacted(compacted) = serde_json::from_str(line)?
            && let Some(state) = compacted.shake_history_state
        {
            return usize::try_from(state.watermark).map_err(Into::into);
        }
    }
    anyhow::bail!("rollout should contain a persisted shake watermark")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shake_save_failure_preserves_the_unprocessed_suffix_for_retry() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = Box::pin(core_test_support::responses::start_mock_server()).await;
    let mut builder = test_codex().with_model("gpt-5.2");
    let fixture = Box::pin(builder.build_with_auto_env(&server)).await?;
    let tail = "recent protected context ".repeat(/*n*/ 5_000);
    let history: Vec<ResponseItem> = vec![
        json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "preserve both outputs until saved"}]
        }),
        json!({
            "type": "function_call",
            "id": "call-one",
            "call_id": "shake-failure-one",
            "name": "exec_command",
            "arguments": "{}"
        }),
        json!({
            "type": "function_call_output",
            "call_id": "shake-failure-one",
            "output": "SHAKE_FAILURE_FIRST_MARKER\n".repeat(/*n*/ 2_000)
        }),
        json!({
            "type": "function_call",
            "id": "call-two",
            "call_id": "shake-failure-two",
            "name": "exec_command",
            "arguments": "{}"
        }),
        json!({
            "type": "function_call_output",
            "call_id": "shake-failure-two",
            "output": "SHAKE_FAILURE_SECOND_MARKER\n".repeat(/*n*/ 2_000)
        }),
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": tail}]
        }),
    ]
    .into_iter()
    .map(serde_json::from_value)
    .collect::<Result<_, _>>()?;
    Box::pin(fixture.codex.inject_response_items(history)).await?;

    let artifact_dir = fixture
        .codex_home_path()
        .join("artifacts")
        .join(fixture.session_configured.thread_id.to_string());
    let preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    assert_eq!(preview.tool_outputs, 2);

    // Making the per-thread artifact directory a regular file forces every save
    // to fail without changing either candidate or the later suffix.
    fs::create_dir_all(artifact_dir.parent().context("artifact root")?)?;
    fs::write(&artifact_dir, "block artifact directory")?;
    Box::pin(fixture.codex.submit(Op::Shake {
        mode: ShakeMode::Elide,
        expected_fingerprint: Some(preview.fingerprint.clone()),
    }))
    .await?;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;
    let after_failure = fixture
        .codex
        .load_history(/*include_archived*/ false)
        .await?;
    let after_failure = serde_json::to_string(&after_failure.items)?;
    assert!(after_failure.contains("SHAKE_FAILURE_FIRST_MARKER"));
    assert!(after_failure.contains("SHAKE_FAILURE_SECOND_MARKER"));
    assert!(after_failure.contains("recent protected context"));
    assert!(!artifact_dir.is_dir());
    let failure_watermark = latest_shake_watermark(
        &fixture
            .codex
            .rollout_path()
            .context("rollout path after failed save")?,
    )?;
    let before_retry_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("shake-failure-r1"),
            ev_assistant_message("shake-failure-m1", "still present"),
            ev_completed("shake-failure-r1"),
        ]),
    )
    .await;
    Box::pin(fixture.submit_text_turn("inspect before retry")).await?;
    let before_retry_input = before_retry_response.single_request().input();

    let retry_preview = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    assert_eq!(retry_preview.tool_outputs, 2);

    fs::remove_file(&artifact_dir)?;
    Box::pin(fixture.codex.submit(Op::Shake {
        mode: ShakeMode::Elide,
        expected_fingerprint: Some(retry_preview.fingerprint),
    }))
    .await?;
    Box::pin(wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::Warning(warning) if warning.message.starts_with("⛭ shake:"))
    }))
    .await;

    let after_retry = fixture.codex.preview_shake(ShakeMode::Elide).await?;
    assert_eq!((after_retry.tool_outputs, after_retry.text_blocks), (0, 0));
    assert_eq!(artifact_log_files(&artifact_dir).len(), 2);
    let after_retry_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("shake-failure-r2"),
            ev_assistant_message("shake-failure-m2", "retried"),
            ev_completed("shake-failure-r2"),
        ]),
    )
    .await;
    Box::pin(fixture.submit_text_turn("inspect after retry")).await?;
    let after_retry_input = after_retry_response.single_request().input();
    assert!(before_retry_input.len() >= failure_watermark);
    assert!(after_retry_input.len() >= failure_watermark);
    assert_eq!(
        &before_retry_input[..failure_watermark],
        &after_retry_input[..failure_watermark],
        "an artifact retry preserves the wire prefix sealed before the failed envelope"
    );
    Ok(())
}
