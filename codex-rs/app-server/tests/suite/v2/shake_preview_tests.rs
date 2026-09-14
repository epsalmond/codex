use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ThreadInjectItemsParams;
use codex_app_server_protocol::ThreadInjectItemsResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::ThreadSettingsUpdateResponse;
use codex_app_server_protocol::ThreadSettingsUpdatedNotification;
use codex_app_server_protocol::ThreadShakePreviewParams;
use codex_app_server_protocol::ThreadShakePreviewResponse;
use codex_app_server_protocol::ThreadShakeStartParams;
use codex_app_server_protocol::ThreadShakeStartResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::WarningNotification;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

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
    assert!(artifact_dir.is_dir());
    assert!(std::fs::read_dir(&artifact_dir)?.next().is_none());
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
    assert!(std::fs::read_dir(&artifact_dir)?.next().is_none());
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

#[tokio::test]
async fn smart_compact_preview_start_uses_matching_fingerprint_and_luna() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let luna_request = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message(
                "luna-summary",
                "### Goal\n- EXPLICIT_LUNA_HANDOFF\n\n### Decisions\n- Keep the handoff\n\n### Open threads\n- None\n\n### Files\n- (none stated)\n\n### Commands\n- (none stated)\n\n### User context needed\n- Keep the latest user request intact.",
            ),
            responses::ev_completed_with_tokens("luna-response", /*total_tokens*/ 250),
        ]),
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model_provider("openai-custom")
        .with_provider_name("OpenAI")
        .with_model("gpt-6-astra")
        .write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-6-astra".to_string()),
            ..Default::default()
        })
        .await?
        .thread;
    let _: ThreadInjectItemsResponse = app
        .request(|request_id| ClientRequest::ThreadInjectItems {
            request_id,
            params: ThreadInjectItemsParams {
                thread_id: thread.id.clone(),
                items: vec![
                    json!({"type":"function_call", "call_id":"call", "name":"exec_command", "arguments":"{}"}),
                    json!({"type":"function_call_output", "call_id":"call", "output":"original output ".repeat(/*n*/ 1_000)}),
                    json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"recent context ".repeat(/*n*/ 2_000)}]}),
                ],
            },
        })
        .await?;

    let params = ThreadShakePreviewParams {
        thread_id: thread.id.clone(),
        mode: "smartCompact".to_string(),
    };
    let preview: ThreadShakePreviewResponse = app
        .request(|request_id| ClientRequest::ThreadShakePreview {
            request_id,
            params: params.clone(),
        })
        .await?;
    assert!(preview.preview.tokens_before > preview.preview.tokens_after);

    let _: ThreadSettingsUpdateResponse = app
        .request(|request_id| ClientRequest::ThreadSettingsUpdate {
            request_id,
            params: ThreadSettingsUpdateParams {
                thread_id: thread.id.clone(),
                model: Some("gpt-5.4".to_string()),
                ..Default::default()
            },
        })
        .await?;
    let _: ThreadSettingsUpdatedNotification =
        app.read_notification("thread/settings/updated").await?;
    let _: ThreadShakeStartResponse = app
        .request(|request_id| ClientRequest::ThreadShakeStart {
            request_id,
            params: ThreadShakeStartParams {
                thread_id: thread.id.clone(),
                mode: "smartCompact".to_string(),
                expected_fingerprint: Some(preview.preview.fingerprint),
            },
        })
        .await?;
    let stale_warning: WarningNotification = app.read_notification("warning").await?;
    assert!(
        stale_warning
            .message
            .contains("model settings changed since the preview")
    );

    let _: ThreadSettingsUpdateResponse = app
        .request(|request_id| ClientRequest::ThreadSettingsUpdate {
            request_id,
            params: ThreadSettingsUpdateParams {
                thread_id: thread.id.clone(),
                model: Some("gpt-6-astra".to_string()),
                ..Default::default()
            },
        })
        .await?;
    let _: ThreadSettingsUpdatedNotification =
        app.read_notification("thread/settings/updated").await?;
    let fresh_preview: ThreadShakePreviewResponse = app
        .request(|request_id| ClientRequest::ThreadShakePreview {
            request_id,
            params: params.clone(),
        })
        .await?;

    let _: ThreadShakeStartResponse = app
        .request(|request_id| ClientRequest::ThreadShakeStart {
            request_id,
            params: ThreadShakeStartParams {
                thread_id: thread.id.clone(),
                mode: "smartCompact".to_string(),
                expected_fingerprint: Some(fresh_preview.preview.fingerprint),
            },
        })
        .await?;
    let item_started: ItemStartedNotification = app.read_notification("item/started").await?;
    assert!(matches!(
        &item_started.item,
        ThreadItem::ContextCompaction { .. }
    ));
    let warning: WarningNotification = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 60),
        app.read_notification("warning"),
    )
    .await??;
    let item_completed: ItemCompletedNotification = app.read_notification("item/completed").await?;
    assert_eq!(item_completed.item, item_started.item);
    assert!(warning.message.contains("Luna handoff saved at"));

    let request = luna_request.single_request();
    assert_eq!(request.body_json()["model"], "gpt-5.6-luna");
    assert!(request.body_contains_text("original output"));
    Ok(())
}

#[tokio::test]
async fn smart_compact_interrupt_during_luna_request_completes_promptly() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let summary = responses::sse(vec![responses::ev_response_created("luna-stream")]);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(summary)
                .set_delay(std::time::Duration::from_secs(/*secs*/ 5)),
        )
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model_provider("openai-custom")
        .with_provider_name("OpenAI")
        .with_model("gpt-6-astra")
        .with_provider_config("supports_websockets = false")
        .write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-6-astra".to_string()),
            ..Default::default()
        })
        .await?
        .thread;
    let _: ThreadInjectItemsResponse = app
        .request(|request_id| ClientRequest::ThreadInjectItems {
            request_id,
            params: ThreadInjectItemsParams {
                thread_id: thread.id.clone(),
                items: vec![
                    json!({"type":"function_call", "call_id":"call", "name":"exec_command", "arguments":"{}"}),
                    json!({"type":"function_call_output", "call_id":"call", "output":"original output ".repeat(/*n*/ 1_000)}),
                    json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"recent context ".repeat(/*n*/ 2_000)}]}),
                ],
            },
        })
        .await?;
    let preview: ThreadShakePreviewResponse = app
        .request(|request_id| ClientRequest::ThreadShakePreview {
            request_id,
            params: ThreadShakePreviewParams {
                thread_id: thread.id.clone(),
                mode: "smartCompact".to_string(),
            },
        })
        .await?;
    let _: ThreadShakeStartResponse = app
        .request(|request_id| ClientRequest::ThreadShakeStart {
            request_id,
            params: ThreadShakeStartParams {
                thread_id: thread.id.clone(),
                mode: "smartCompact".to_string(),
                expected_fingerprint: Some(preview.preview.fingerprint),
            },
        })
        .await?;
    let started: TurnStartedNotification = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 60),
        app.read_notification("turn/started"),
    )
    .await??;
    // The delayed response keeps the cancellable Luna request active while
    // the app-server processes the interrupt RPC.
    tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 100)).await;
    let _: TurnInterruptResponse = app
        .request(|request_id| ClientRequest::TurnInterrupt {
            request_id,
            params: TurnInterruptParams {
                thread_id: thread.id.clone(),
                turn_id: started.turn.id.clone(),
            },
        })
        .await?;
    let completed: TurnCompletedNotification = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 60),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Interrupted);
    Ok(())
}
