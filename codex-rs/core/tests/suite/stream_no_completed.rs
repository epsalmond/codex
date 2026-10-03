//! Verifies that the agent retries when the SSE stream terminates before
//! delivering a `response.completed` event.

use codex_core::TurnInputRequest;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::net::TcpListener;
use wiremock::MockServer;

fn sse_incomplete() -> String {
    responses::sse(vec![serde_json::json!({
        "type": "response.output_item.done",
    })])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_on_early_close() {
    skip_if_no_network!();

    let incomplete_sse = sse_incomplete();
    let completed_sse = responses::sse_completed("resp_ok");

    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: incomplete_sse,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: completed_sse,
        }],
    ])
    .await;

    // Configure retry behavior explicitly to avoid mutating process-wide
    // environment variables.

    let model_provider = ModelProviderInfo {
        name: "openai".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        model_catalog_url: None,
        // Environment variable that should exist in the test environment.
        // ModelClient will return an error if the environment variable for the
        // provider is not set.
        env_key: Some("PATH".into()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        gateway_oauth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        // exercise retry path: first attempt yields incomplete stream, so allow 1 retry
        request_max_retries: Some(0),
        stream_max_retries: Some(1),
        stream_idle_timeout_ms: Some(2000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
        include_internal_metadata: false,
    };

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
        })
        .build_with_streaming_server(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    // Wait until TurnComplete (should succeed after retry).
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "expected retry after incomplete SSE stream"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_failure_pauses_retry_budget_until_provider_is_reachable() -> anyhow::Result<()>
{
    skip_if_no_network!(Ok(()));

    let bootstrap_server = responses::start_mock_server().await;
    let unavailable_listener = TcpListener::bind("127.0.0.1:0")?;
    let unavailable_address = unavailable_listener.local_addr()?;
    drop(unavailable_listener);

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider.base_url = Some(format!("http://{unavailable_address}/v1"));
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&bootstrap_server)
        .await?;

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "recover after the network returns".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let EventMsg::StreamError(connection_error) =
        wait_for_event(&codex, |event| matches!(event, EventMsg::StreamError(_))).await
    else {
        unreachable!("predicate guarantees a stream error event");
    };
    assert_eq!(
        connection_error.message,
        "Reconnecting... waiting for network"
    );

    let recovered_server = MockServer::builder()
        .listener(TcpListener::bind(unavailable_address)?)
        .start()
        .await;
    let response_mock = responses::mount_sse_sequence(
        &recovered_server,
        vec![sse_incomplete(), responses::sse_completed("resp_recovered")],
    )
    .await;

    let EventMsg::StreamError(stream_error) =
        wait_for_event(&codex, |event| matches!(event, EventMsg::StreamError(_))).await
    else {
        unreachable!("predicate guarantees a stream error event");
    };
    assert_eq!(stream_error.message, "Reconnecting... 1/1");

    let EventMsg::TurnComplete(completed) =
        wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await
    else {
        unreachable!("predicate guarantees a turn complete event");
    };

    assert_eq!(completed.error, None);
    assert_eq!(response_mock.requests().len(), 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_rechecks_partial_output_before_retry_without_completed_usage() -> anyhow::Result<()>
{
    use codex_core::config::SubagentContextReductionConfig;
    use codex_protocol::context_usage::ContextReductionOutcome;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;

    skip_if_no_network!(Ok(()));
    let partial = "partial ".repeat(/*n*/ 1_000);
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("seed", "done"),
                responses::ev_completed_with_tokens("seed", /*total_tokens*/ 49_000),
            ]),
            // A real item is retained, but the stream closes without measuring it.
            responses::sse(vec![responses::ev_assistant_message("partial", &partial)]),
            responses::sse(vec![
                serde_json::json!({"type": "response.output_item.done", "item": {
                    "type": "compaction", "encrypted_content": "retained-retry-task-summary"
                }}),
                responses::ev_completed("compact"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("final", "done"),
                responses::ev_completed("final"),
            ]),
        ],
    )
    .await;
    let fixture = test_codex()
        .with_model("gpt-5.6-sol")
        .with_session_source(SessionSource::SubAgent(SubAgentSource::Other(
            "retry-budget".to_string(),
        )))
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
            config.model_provider.supports_websockets = false;
            config.subagent_context_reduction = SubagentContextReductionConfig {
                enabled: true,
                threshold_tokens: 50_000,
            };
        })
        .build_with_auto_env(&server)
        .await?;
    fixture
        .submit_text_turn("Keep the retry task constraint.")
        .await?;
    fixture
        .submit_text_turn("Validate the remaining work.")
        .await?;
    let requests = mock.requests();
    assert!(
        requests.len() >= 3,
        "seed, partial stream and rebuilt retry requests"
    );
    let before_partial = requests[1].body_json();
    let retry = requests[2].body_json();
    assert!(
        before_partial.to_string().len() + partial.len() < 50_000 * 4,
        "the complete local request estimate stays below the cap"
    );
    assert!(
        retry["input"].as_array().is_some_and(|items| items
            .iter()
            .any(|item| item["type"] == "compaction_trigger")),
        "provider usage plus retained partial output must reduce before retrying"
    );
    assert!(retry["input"].to_string().contains(&partial));
    assert_eq!(requests.len(), 4);
    assert!(
        requests[3].body_json()["input"]
            .to_string()
            .contains("retained-retry-task-summary")
    );
    let snapshot = fixture
        .codex
        .context_usage_snapshot()
        .await
        .expect("captured context");
    assert_eq!(
        snapshot
            .last_reduction
            .as_ref()
            .map(|record| record.outcome),
        Some(ContextReductionOutcome::Compacted)
    );
    fixture.codex.shutdown_and_wait().await?;
    Ok(())
}
