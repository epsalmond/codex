use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadInjectItemsParams;
use codex_app_server_protocol::ThreadInjectItemsResponse;
use codex_app_server_protocol::ThreadShakePreviewParams;
use codex_app_server_protocol::ThreadShakePreviewResponse;
use codex_app_server_protocol::ThreadShakeStartParams;
use codex_app_server_protocol::ThreadShakeStartResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::WarningNotification;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn shake_preview_is_read_only_and_rejects_stale_confirmation() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let _: ThreadInjectItemsResponse = app.request(|request_id| ClientRequest::ThreadInjectItems {
        request_id,
        params: ThreadInjectItemsParams {
            thread_id: thread.id.clone(),
            items: vec![
                json!({"type":"function_call", "call_id":"call", "name":"exec_command", "arguments":"{}"}),
                json!({"type":"function_call_output", "call_id":"call", "output":"original output ".repeat(/*n*/ 1_000)}),
                json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"recent context ".repeat(/*n*/ 2_000)}]}),
            ],
        },
    }).await?;
    let params = ThreadShakePreviewParams {
        thread_id: thread.id.clone(),
        mode: "elide".to_string(),
    };
    let before: ThreadShakePreviewResponse = app
        .request(|request_id| ClientRequest::ThreadShakePreview {
            request_id,
            params: params.clone(),
        })
        .await?;
    assert_eq!(before.preview.tool_outputs, 1);
    assert!(before.preview.tokens_before > before.preview.tokens_after);
    let artifact_dir = codex_home.path().join("artifacts").join(&thread.id);
    assert!(!artifact_dir.exists());
    let repeated: ThreadShakePreviewResponse = app
        .request(|request_id| ClientRequest::ThreadShakePreview {
            request_id,
            params: params.clone(),
        })
        .await?;
    assert_eq!(repeated, before);

    let _: ThreadInjectItemsResponse = app.request(|request_id| ClientRequest::ThreadInjectItems {
        request_id,
        params: ThreadInjectItemsParams {
            thread_id: thread.id.clone(),
            items: vec![json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"new context after preview"}]})],
        },
    }).await?;
    let _: ThreadShakeStartResponse = app
        .request(|request_id| ClientRequest::ThreadShakeStart {
            request_id,
            params: ThreadShakeStartParams {
                thread_id: thread.id.clone(),
                mode: "elide".to_string(),
                expected_fingerprint: Some(before.preview.fingerprint),
            },
        })
        .await?;
    let warning: WarningNotification = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 60),
        app.read_notification("warning"),
    )
    .await??;
    assert!(warning.message.contains("History changed"));
    assert!(!artifact_dir.exists());
    let current: ThreadShakePreviewResponse = app
        .request(|request_id| ClientRequest::ThreadShakePreview {
            request_id,
            params: params.clone(),
        })
        .await?;
    assert_eq!(current.preview.tool_outputs, 1);
    let _: ThreadShakeStartResponse = app
        .request(|request_id| ClientRequest::ThreadShakeStart {
            request_id,
            params: ThreadShakeStartParams {
                thread_id: thread.id.clone(),
                mode: "elide".to_string(),
                expected_fingerprint: Some(current.preview.fingerprint),
            },
        })
        .await?;
    let warning: WarningNotification = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 60),
        app.read_notification("warning"),
    )
    .await??;
    assert!(warning.message.contains("Shook"));
    let after: ThreadShakePreviewResponse = app
        .request(|request_id| ClientRequest::ThreadShakePreview { request_id, params })
        .await?;
    assert_eq!(after.preview.tokens_before, current.preview.tokens_after);
    assert_eq!(after.preview.tool_outputs, 0);
    assert!(artifact_dir.exists());
    assert!(
        server
            .received_requests()
            .await
            .context("failed to read mock server requests")?
            .is_empty()
    );
    Ok(())
}
