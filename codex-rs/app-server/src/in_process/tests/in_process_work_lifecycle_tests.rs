use super::*;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadWorkShutdownIfQuiescentParams;
use codex_app_server_protocol::ThreadWorkSubscribeOutcome;
use codex_app_server_protocol::ThreadWorkSubscribeParams;
use codex_app_server_protocol::ThreadWorkSubscribeResponse;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

/// `features.multi_agent_v2.agent_polling = "enabled"` turns wake mode off, which leaves an Exec
/// root unobserved.
#[tokio::test]
async fn opted_out_exec_root_reports_lifecycle_unavailable() {
    let client = start_test_client_with_experimental_api(SessionSource::Exec).await;
    let start_response = client
        .request(ClientRequest::ThreadStart {
            request_id: RequestId::Integer(10),
            params: ThreadStartParams {
                ephemeral: Some(true),
                config: Some(HashMap::from([(
                    "features.multi_agent_v2.agent_polling".to_string(),
                    serde_json::json!("enabled"),
                )])),
                ..ThreadStartParams::default()
            },
        })
        .await
        .expect("thread/start transport should work")
        .expect("thread/start should succeed");
    let start_response: ThreadStartResponse =
        serde_json::from_value(start_response).expect("thread/start response should parse");
    let thread_id = start_response.thread.id;

    let subscribe_response = client
        .request(ClientRequest::ThreadWorkSubscribe {
            request_id: RequestId::Integer(11),
            params: ThreadWorkSubscribeParams {
                thread_id: thread_id.clone(),
            },
        })
        .await
        .expect("work-state subscription transport should work")
        .expect("work-state subscription should report its capability");
    let subscribe_response: ThreadWorkSubscribeResponse =
        serde_json::from_value(subscribe_response).expect("subscribe response should parse");
    assert_eq!(
        subscribe_response.outcome,
        ThreadWorkSubscribeOutcome::Unavailable
    );
    assert_eq!(subscribe_response.snapshot, None);

    let shutdown_error = client
        .request(ClientRequest::ThreadWorkShutdownIfQuiescent {
            request_id: RequestId::Integer(12),
            params: ThreadWorkShutdownIfQuiescentParams {
                thread_id: thread_id.clone(),
                revision: "untrusted-revision".to_string(),
            },
        })
        .await
        .expect("guarded-shutdown transport should work")
        .expect_err("an incomplete backend must not close the root execution scope");
    assert!(shutdown_error.message.contains("does not own"));

    client
        .shutdown()
        .await
        .expect("in-process runtime should shut down cleanly");
}

#[tokio::test]
async fn unavailable_backend_does_not_publish_work_to_cli_connection() {
    let client = start_test_client_with_experimental_api(SessionSource::Cli).await;
    let start_response = client
        .request(ClientRequest::ThreadStart {
            request_id: RequestId::Integer(20),
            params: ThreadStartParams {
                ephemeral: Some(true),
                ..ThreadStartParams::default()
            },
        })
        .await
        .expect("thread/start transport should work")
        .expect("thread/start should succeed");
    let start_response: ThreadStartResponse =
        serde_json::from_value(start_response).expect("thread/start response should parse");

    let response = client
        .request(ClientRequest::ThreadWorkSubscribe {
            request_id: RequestId::Integer(21),
            params: ThreadWorkSubscribeParams {
                thread_id: start_response.thread.id,
            },
        })
        .await
        .expect("subscription transport should work")
        .expect("subscription should report its capability");
    let response: ThreadWorkSubscribeResponse =
        serde_json::from_value(response).expect("subscribe response should parse");
    assert_eq!(response.outcome, ThreadWorkSubscribeOutcome::Unavailable);
    assert_eq!(response.snapshot, None);

    client
        .shutdown()
        .await
        .expect("in-process runtime should shut down cleanly");
}

/// An Exec root is observable by default; `opted_out_exec_root_reports_lifecycle_unavailable`
/// covers the opt-out.
#[tokio::test]
async fn exec_root_subscribes_to_work_state_by_default() {
    let client = start_test_client_with_experimental_api(SessionSource::Exec).await;
    let start_response = client
        .request(ClientRequest::ThreadStart {
            request_id: RequestId::Integer(30),
            params: ThreadStartParams {
                ephemeral: Some(true),
                ..ThreadStartParams::default()
            },
        })
        .await
        .expect("thread/start transport should work")
        .expect("thread/start should succeed");
    let start_response: ThreadStartResponse =
        serde_json::from_value(start_response).expect("thread/start response should parse");

    let response = client
        .request(ClientRequest::ThreadWorkSubscribe {
            request_id: RequestId::Integer(31),
            params: ThreadWorkSubscribeParams {
                thread_id: start_response.thread.id,
            },
        })
        .await
        .expect("subscription transport should work")
        .expect("subscription should report its capability");
    let response: ThreadWorkSubscribeResponse =
        serde_json::from_value(response).expect("subscribe response should parse");
    assert_eq!(response.outcome, ThreadWorkSubscribeOutcome::Subscribed);
    let snapshot = response
        .snapshot
        .expect("a subscribed root reports its work snapshot");
    assert!(snapshot.quiescent, "{snapshot:?}");
    assert!(!snapshot.closed, "{snapshot:?}");

    client
        .shutdown()
        .await
        .expect("in-process runtime should shut down cleanly");
}
