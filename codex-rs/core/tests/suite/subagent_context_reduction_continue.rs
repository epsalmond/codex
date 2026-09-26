//! Integration coverage for continued child reduction failures and warning delivery.

use anyhow::Result;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_exec_command_call;
use core_test_support::responses::ev_exec_command_call_with_args;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;
use wiremock::MockServer;

const MULTI_AGENT_V2_NAMESPACE: &str = "collaboration";

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| body.to_string().contains(text))
}

fn has_function_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
        })
}

fn has_input_type(request: &wiremock::Request, item_type: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some(item_type))
        })
}

fn is_subagent_request(request: &wiremock::Request) -> bool {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|body| {
            body["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .and_then(|metadata| serde_json::from_str::<Value>(metadata).ok())
        })
        .is_some_and(|metadata| metadata["parent_turn_id"].is_string())
}

fn thread_id(request: &wiremock::Request) -> Option<String> {
    serde_json::from_slice::<Value>(&request.body)
        .ok()?
        .get("client_metadata")?
        .get("thread_id")?
        .as_str()
        .map(str::to_owned)
}

fn collaboration_call(call_id: &str, tool_name: &str, arguments: &Value) -> Value {
    ev_function_call_with_namespace(
        call_id,
        MULTI_AGENT_V2_NAMESPACE,
        tool_name,
        &arguments.to_string(),
    )
}

fn ev_compaction_output(summary: &str) -> Value {
    json!({
        "type": "response.output_item.done",
        "item": {
            "type": "compaction",
            "encrypted_content": summary
        }
    })
}

async fn mount_assistant_completion(
    server: &MockServer,
    request_matches: impl Fn(&wiremock::Request) -> bool + Send + Sync + 'static,
    response_id: &'static str,
    message_id: &'static str,
    message: &'static str,
) {
    mount_sse_once_match(
        server,
        request_matches,
        sse(vec![
            ev_response_created(response_id),
            ev_assistant_message(message_id, message),
            ev_completed(response_id),
        ]),
    )
    .await;
}

fn configure_multi_agent_v2(config: &mut codex_core::config::Config) {
    config
        .features
        .enable(Feature::Collab)
        .expect("test config should allow collaboration");
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow MultiAgentV2");
    config.tool_output_token_limit = Some(100_000);
}

#[cfg(windows)]
fn read_output_command(path: &str) -> String {
    format!("type {path}")
}

#[cfg(not(windows))]
fn read_output_command(path: &str) -> String {
    format!("cat {path}")
}

async fn wait_for_child_turn(test: &TestCodex, root_thread_id: ThreadId) -> Result<ThreadId> {
    let child_thread_id = timeout(Duration::from_secs(30), async {
        loop {
            let child_thread_id = test
                .thread_manager
                .list_thread_ids()
                .await
                .into_iter()
                .find(|thread_id| *thread_id != root_thread_id);
            if let Some(child_thread_id) = child_thread_id {
                break child_thread_id;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    timeout(Duration::from_secs(30), async {
        loop {
            let event = child_thread
                .next_event()
                .await
                .expect("child event stream should remain open");
            if matches!(event.msg, EventMsg::TurnComplete(_)) {
                break;
            }
        }
    })
    .await?;
    Ok(child_thread_id)
}

async fn response_requests_for_thread(
    server: &MockServer,
    expected_thread_id: ThreadId,
) -> Vec<wiremock::Request> {
    let expected_thread_id = expected_thread_id.to_string();
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| {
            request.method == "POST"
                && request.url.path().ends_with("/responses")
                && thread_id(request).as_deref() == Some(expected_thread_id.as_str())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_failure_warns_idle_parent_once_and_suppresses_recompact() -> Result<()> {
    const ROOT_PROMPT: &str = "spawn a child that continues after an insufficient compact";
    const CHILD_TASK: &str = "run two steps before finishing";
    const FOLLOWUP_PROMPT: &str = "check the child reduction warning";
    const WARNING: &str = "Subagent context reduction did not reach its threshold";
    const THRESHOLD_TOKENS: i64 = 80_000;

    let server = start_mock_server().await;
    let spawn_arguments = json!({
        "message": CHILD_TASK,
        "task_name": "continue_child",
        "fork_turns": "none",
        "context_policy": {
            "enabled": true,
            "threshold_tokens": THRESHOLD_TOKENS,
            "check_after_tools": true,
            "shake": "off",
            "on_failure": "continue"
        }
    });
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_PROMPT),
        sse(vec![
            ev_response_created("continue-root-spawn-response"),
            collaboration_call("continue-root-spawn", "spawn_agent", &spawn_arguments),
            ev_completed("continue-root-spawn-response"),
        ]),
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "continue-root-spawn"),
        "continue-root-after-spawn-response",
        "continue-root-after-spawn-message",
        "child started",
    )
    .await;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, CHILD_TASK)
                && !has_input_type(request, "compaction_trigger")
                && !has_input_type(request, "compaction")
                && !has_function_call_output(request, "continue-first-tool")
                && !has_function_call_output(request, "continue-second-tool")
        },
        sse(vec![
            ev_response_created("continue-child-first-sample"),
            ev_exec_command_call("continue-first-tool", "echo first step"),
            ev_completed_with_tokens("continue-child-first-complete", 100_000),
        ]),
    )
    .await;

    let compact_summary = "CONTINUE_MODE_COMPACTION_SUMMARY\n"
        .to_string()
        .repeat(30_000);
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request) && has_input_type(request, "compaction_trigger")
        },
        sse(vec![
            ev_response_created("continue-child-compact-response"),
            ev_compaction_output(&compact_summary),
            ev_completed("continue-child-compact-response"),
        ]),
    )
    .await;

    let second_output = "SECOND_TOOL_OUTPUT_MARKER\n".to_string().repeat(400);
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;
    std::fs::write(
        test.workspace_path("continue-second-tool-output.txt"),
        second_output,
    )?;
    let second_tool_call = ev_exec_command_call_with_args(
        "continue-second-tool",
        &json!({
            "cmd": read_output_command("continue-second-tool-output.txt"),
            "max_output_tokens": 100_000
        }),
    );
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && has_input_type(request, "compaction")
                && !has_input_type(request, "compaction_trigger")
                && !has_function_call_output(request, "continue-second-tool")
        },
        sse(vec![
            ev_response_created("continue-child-after-compact-response"),
            second_tool_call,
            ev_completed_with_tokens("continue-child-after-compact-response", 186_000),
        ]),
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && has_function_call_output(request, "continue-second-tool")
                && !has_input_type(request, "compaction_trigger")
        },
        "continue-child-final-response",
        "continue-child-final-message",
        "finished both steps after compact",
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FOLLOWUP_PROMPT),
        sse(vec![
            ev_response_created("continue-parent-followup-list-response"),
            collaboration_call("continue-root-list-agents", "list_agents", &json!({})),
            ev_completed("continue-parent-followup-list-response"),
        ]),
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| {
            has_function_call_output(request, "continue-root-list-agents")
        },
        "continue-parent-followup-response",
        "continue-parent-followup-message",
        "observed the child warning",
    )
    .await;

    let root_thread_id = test.session_configured.thread_id;
    test.submit_turn_with_permission_profile(ROOT_PROMPT, PermissionProfile::workspace_write())
        .await?;
    let child_thread_id = wait_for_child_turn(&test, root_thread_id).await?;

    let child_requests = response_requests_for_thread(&server, child_thread_id).await;
    let compact_requests = child_requests
        .iter()
        .filter(|request| has_input_type(request, "compaction_trigger"))
        .collect::<Vec<_>>();
    assert_eq!(compact_requests.len(), 1, "the child should compact once");
    assert!(has_input_type(compact_requests[0], "compaction_trigger"));
    let after_compact_request_count = child_requests
        .iter()
        .filter(|request| {
            has_input_type(request, "compaction")
                && !has_input_type(request, "compaction_trigger")
                && !has_function_call_output(request, "continue-second-tool")
        })
        .count();
    assert_eq!(
        after_compact_request_count, 1,
        "the child should continue after compaction"
    );
    let child_final_request_count = child_requests
        .iter()
        .filter(|request| {
            has_function_call_output(request, "continue-second-tool")
                && !has_input_type(request, "compaction_trigger")
        })
        .count();
    assert_eq!(
        child_final_request_count, 1,
        "the child should finish after the suppressed boundary"
    );

    let parent_requests_before_followup =
        response_requests_for_thread(&server, root_thread_id).await;
    assert_eq!(
        parent_requests_before_followup.len(),
        2,
        "the queue-only warning must not wake the parent before an explicit follow-up"
    );
    assert!(
        !parent_requests_before_followup
            .iter()
            .any(|request| body_contains(request, FOLLOWUP_PROMPT))
    );

    test.submit_turn(FOLLOWUP_PROMPT).await?;
    let parent_requests_after_followup =
        response_requests_for_thread(&server, root_thread_id).await;
    assert_eq!(parent_requests_after_followup.len(), 4);
    let followup_requests = parent_requests_after_followup
        .iter()
        .filter(|request| body_contains(request, FOLLOWUP_PROMPT))
        .collect::<Vec<_>>();
    assert_eq!(followup_requests.len(), 2);
    let warning_count = followup_requests
        .iter()
        .map(|request| {
            serde_json::from_slice::<Value>(&request.body)
                .expect("follow-up request should be valid JSON")
                .to_string()
                .matches(WARNING)
                .count()
        })
        .sum::<usize>();
    assert_eq!(
        warning_count, 1,
        "the parent should receive one warning across its explicit follow-up requests"
    );
    assert!(
        followup_requests.iter().any(|request| {
            serde_json::from_slice::<Value>(&request.body)
                .expect("follow-up request should be valid JSON")
                .to_string()
                .contains("continue_child")
        }),
        "the warning should identify the child task"
    );
    assert_eq!(parent_requests_after_followup.len(), 4);

    assert_eq!(
        child_requests
            .iter()
            .map(|request| {
                serde_json::from_slice::<Value>(&request.body)
                    .ok()
                    .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
                    .map_or(0, |items| {
                        items
                            .iter()
                            .filter(|item| {
                                item.get("type").and_then(Value::as_str)
                                    == Some("compaction_trigger")
                            })
                            .count()
                    })
            })
            .sum::<usize>(),
        1,
        "the second child boundary should stay suppressed below the retry-growth limit"
    );

    Ok(())
}
