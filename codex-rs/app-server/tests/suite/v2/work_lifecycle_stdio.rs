//! An external stdio client of `codex app-server --session-source exec` owns its root's work
//! lifecycle exactly like in-process `codex exec`. The terminal-output ack it gates on is the
//! transport's own write completion, so the client never has to send anything back for
//! `turn/completed` to arrive.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadWorkSubscribeOutcome;
use codex_app_server_protocol::ThreadWorkSubscribeParams;
use codex_app_server_protocol::ThreadWorkSubscribeResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput as V2UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn stdio_exec_client_owns_lifecycle_without_sending_acks() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let _response = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "Done"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_args(&["--session-source", "exec"])
        .build()
        .await?;
    timeout(READ_TIMEOUT, app_server.initialize()).await??;

    let start_request = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let start_response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(start_request)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(start_response)?;

    let subscribe_request = app_server
        .send_request(
            "thread/subscribeWorkState",
            Some(serde_json::to_value(ThreadWorkSubscribeParams {
                thread_id: thread.id.clone(),
            })?),
        )
        .await?;
    let subscribe_response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(subscribe_request)),
    )
    .await??;
    let subscribe_response: ThreadWorkSubscribeResponse = to_response(subscribe_response)?;
    assert_eq!(
        subscribe_response.outcome,
        ThreadWorkSubscribeOutcome::Subscribed
    );

    app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![V2UserInput::Text {
                text: "Hello".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    Ok(())
}
