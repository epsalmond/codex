//! Integration coverage for subagent context-reduction policy and safe boundaries.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_exec_command_call;
use core_test_support::responses::ev_exec_command_call_with_args;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::ev_tool_search_call;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::namespace_child_tool;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::stdio_server_bin;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_with_timeout;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
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

fn has_tool_search_output(request: &wiremock::Request, call_id: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("tool_search_output")
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

fn response_request_summary(request: &wiremock::Request) -> String {
    let Some(body) = serde_json::from_slice::<Value>(&request.body).ok() else {
        return format!("{} <unparseable body>", request.url.path());
    };
    let items = body["input"].as_array().cloned().unwrap_or_default();
    let input = items
        .iter()
        .map(|item| {
            format!(
                "{}:{}/{}:{} success={:?}",
                item["type"].as_str().unwrap_or("?"),
                item["namespace"].as_str().unwrap_or(""),
                item["name"].as_str().unwrap_or(""),
                item["call_id"].as_str().unwrap_or(""),
                item["success"]
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let last_input_type = items
        .last()
        .and_then(|item| item["type"].as_str())
        .unwrap_or("?");
    let short_output = items.iter().rev().find_map(|item| {
        item.get("output").map(|output| {
            let text = output
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| output.to_string());
            text.chars().take(100).collect::<String>()
        })
    });
    let thread_id = body["client_metadata"]["thread_id"].as_str().unwrap_or("?");
    let parent_turn_id = body["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .and_then(|metadata| serde_json::from_str::<Value>(metadata).ok())
        .and_then(|metadata| metadata["parent_turn_id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "root".to_string());
    format!(
        "{} method={} thread={} parent_turn={} previous_response_id={:?} last_type={} input=[{}] output_excerpt={:?}",
        request.url.path(),
        request.method,
        thread_id,
        parent_turn_id,
        body["previous_response_id"].as_str(),
        last_input_type,
        input,
        short_output
    )
}

async fn response_request_diagnostics(server: &MockServer) -> String {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.method == "POST" && request.url.path().ends_with("/responses"))
        .map(response_request_summary)
        .collect::<Vec<_>>()
        .join("\n")
}

fn captured_response_request_summary(request: &ResponsesRequest) -> String {
    let body = request.body_json();
    let items = body["input"].as_array().cloned().unwrap_or_default();
    let item_types = items
        .iter()
        .map(|item| {
            format!(
                "{}:{}/{}:{}",
                item["type"].as_str().unwrap_or("?"),
                item["namespace"].as_str().unwrap_or(""),
                item["name"].as_str().unwrap_or(""),
                item["call_id"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let thread_id = body["client_metadata"]["thread_id"].as_str().unwrap_or("?");
    let last_type = items
        .last()
        .and_then(|item| item["type"].as_str())
        .unwrap_or("?");
    format!(
        "thread={thread_id} last_type={last_type} previous_response_id={:?} input=[{item_types}]",
        body["previous_response_id"].as_str()
    )
}

fn diagnostic_tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .flat_map(|tool| {
            let namespace = tool["name"].as_str().unwrap_or_default();
            if tool["type"].as_str() == Some("namespace") {
                tool["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(20)
                    .map(|child| format!("{namespace}/{}", child["name"].as_str().unwrap_or("?")))
                    .collect::<Vec<_>>()
            } else {
                vec![
                    tool["name"]
                        .as_str()
                        .or_else(|| tool["function"]["name"].as_str())
                        .unwrap_or("?")
                        .to_string(),
                ]
            }
        })
        .take(20)
        .collect()
}

fn child_response_request_summary(request: &wiremock::Request) -> Option<String> {
    let compressed = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|encoding| {
            encoding
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    let body = if compressed {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()?
    } else {
        request.body.clone()
    };
    let body = serde_json::from_slice::<Value>(&body).ok()?;
    let turn_metadata = body["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .and_then(|metadata| serde_json::from_str::<Value>(metadata).ok())?;
    if !turn_metadata["parent_turn_id"].is_string() {
        return None;
    }

    let inputs = body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .take(16)
        .map(|item| {
            let item_type = item["type"].as_str().unwrap_or("?");
            let mut text_parts = Vec::new();
            if let Some(text) = item["text"].as_str() {
                text_parts.push(text.to_string());
            }
            if let Some(content) = item["content"].as_array() {
                text_parts.extend(
                    content
                        .iter()
                        .filter_map(|span| span["text"].as_str().map(str::to_string)),
                );
            }
            if let Some(output) = item["output"].as_str() {
                text_parts.push(output.to_string());
            }
            let text = text_parts.join("");
            let excerpt = text.chars().take(120).collect::<String>();
            let searched_tools = item["tools"]
                .as_array()
                .map(|tools| diagnostic_tool_names(tools).join(", "))
                .filter(|tools| !tools.is_empty())
                .map(|tools| format!(" searched_tools=[{tools}]"))
                .unwrap_or_default();
            format!("{item_type}:{excerpt:?}{searched_tools}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let tools = body["tools"]
        .as_array()
        .map(|tools| diagnostic_tool_names(tools).join(", "))
        .unwrap_or_default();
    Some(format!("inputs=[{inputs}] tools=[{tools}]"))
}

async fn child_response_request_diagnostics(server: &MockServer) -> String {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.method == "POST" && request.url.path().ends_with("/responses"))
        .filter_map(child_response_request_summary)
        .collect::<Vec<_>>()
        .join("\n")
}

fn captured_is_subagent_request(request: &ResponsesRequest) -> bool {
    let body = request.body_json();
    body["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .and_then(|metadata| serde_json::from_str::<Value>(metadata).ok())
        .is_some_and(|metadata| metadata["parent_turn_id"].is_string())
}

fn captured_has_tool_search_output(request: &ResponsesRequest, call_id: &str) -> bool {
    request
        .inputs_of_type("tool_search_output")
        .iter()
        .any(|item| item.get("call_id").and_then(Value::as_str) == Some(call_id))
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

async fn wait_for_child_turns(
    test: &TestCodex,
    server: &MockServer,
    expected_children: usize,
) -> Result<()> {
    let thread_ids_result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let thread_ids = test.thread_manager.list_thread_ids().await;
            if thread_ids.len() >= expected_children.saturating_add(1) {
                return thread_ids;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let thread_ids = match thread_ids_result {
        Ok(thread_ids) => thread_ids,
        Err(_) => {
            let current_ids = test.thread_manager.list_thread_ids().await;
            let requests = response_request_diagnostics(server).await;
            anyhow::bail!(
                "timed out waiting for {expected_children} children; current threads={current_ids:?}; responses:\n{requests}"
            );
        }
    };
    for thread_id in thread_ids {
        if thread_id == test.session_configured.thread_id {
            continue;
        }
        let thread = test.thread_manager.get_thread(thread_id).await?;
        let child_completion = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = thread
                    .next_event()
                    .await
                    .expect("child event stream should remain open");
                if matches!(event.msg, EventMsg::TurnComplete(_)) {
                    break;
                }
            }
        })
        .await;
        if child_completion.is_err() {
            let requests = response_request_diagnostics(server).await;
            anyhow::bail!(
                "timed out waiting for child {thread_id} turn completion; responses:\n{requests}"
            );
        }
    }
    Ok(())
}

#[derive(Default)]
struct SpawnOverrides {
    context_policy: Option<Value>,
    inherit_to_children: Option<bool>,
}

async fn mount_spawn_response(
    server: &MockServer,
    request_matches: impl Fn(&wiremock::Request) -> bool + Send + Sync + 'static,
    call_id: &'static str,
    task_name: &'static str,
    task_message: &'static str,
    overrides: SpawnOverrides,
    response_id: &'static str,
) {
    let mut arguments = json!({
        "message": task_message,
        "task_name": task_name,
        "fork_turns": "none"
    });
    if let Some(context_policy) = overrides.context_policy {
        arguments["context_policy"] = context_policy;
    }
    if let Some(inherit_to_children) = overrides.inherit_to_children {
        arguments["inherit_to_children"] = json!(inherit_to_children);
    }
    mount_sse_once_match(
        server,
        request_matches,
        sse(vec![
            ev_response_created(response_id),
            collaboration_call(call_id, "spawn_agent", &arguments),
            ev_completed(response_id),
        ]),
    )
    .await;
}

async fn mount_assistant_completion(
    server: &MockServer,
    request_matches: impl Fn(&wiremock::Request) -> bool + Send + Sync + 'static,
    response_id: &'static str,
    message_id: &'static str,
    message: &'static str,
) -> ResponseMock {
    mount_sse_once_match(
        server,
        request_matches,
        sse(vec![
            ev_response_created(response_id),
            ev_assistant_message(message_id, message),
            ev_completed(response_id),
        ]),
    )
    .await
}

#[cfg(windows)]
fn read_output_command(path: &str) -> String {
    format!("type {path}")
}

#[cfg(not(windows))]
fn read_output_command(path: &str) -> String {
    format!("cat {path}")
}

#[cfg(windows)]
fn delayed_read_output_command(path: &str) -> String {
    format!("Start-Sleep -Milliseconds 1000; type {path}")
}

#[cfg(not(windows))]
fn delayed_read_output_command(path: &str) -> String {
    format!("sleep 1; cat {path}")
}

#[cfg(windows)]
fn wait_for_release_command(path: &str) -> String {
    format!(
        "while (-not (Test-Path '{path}')) {{ Start-Sleep -Milliseconds 50 }}; Write-Output released"
    )
}

#[cfg(not(windows))]
fn wait_for_release_command(path: &str) -> String {
    format!("while [ ! -f '{path}' ]; do sleep 0.05; done; printf 'released\\n'")
}

fn write_sample_plugin_mcp_fixture(home: &TempDir, server_bin: &str) -> Result<()> {
    let plugin_root = home.path().join("plugins/cache/test/sample/local");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample","description":"inspect sample data"}"#,
    )?;
    std::fs::write(
        home.path().join("config.toml"),
        "[features]\nplugins = true\n\n[plugins.\"sample@test\"]\nenabled = true\n",
    )?;
    std::fs::write(
        plugin_root.join(".mcp.json"),
        serde_json::to_vec(&json!({
            "mcpServers": {
                "sample": {
                    "command": server_bin,
                    "cwd": ".",
                    "startup_timeout_sec": 60.0
                }
            }
        }))?,
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_policy_shakes_old_tool_output_before_child_continuation() -> Result<()> {
    const ROOT_PROMPT: &str = "spawn the policy inheriting parent";
    const PARENT_TASK: &str = "spawn one reducer grandchild";
    const GRANDCHILD_TASK: &str =
        "MUST_RETAIN_ASSIGNMENT_CONSTRAINT: finish the assigned work after reading both files";
    const LIST_PROMPT: &str = "inspect subagent reduction telemetry";
    const ELIDABLE_MARKER: &str = "ELIDABLE_TOOL_OUTPUT_MARKER_71";
    const RECENT_MARKER: &str = "RECENT_TOOL_OUTPUT_MARKER_92";
    const THRESHOLD_TOKENS: i64 = 80_000;

    let server = start_mock_server().await;
    let policy = json!({
        "enabled": true,
        "threshold_tokens": THRESHOLD_TOKENS,
        "check_after_tools": true,
        "shake": "on",
        "on_failure": "stop"
    });
    mount_spawn_response(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_PROMPT),
        "root-spawn-parent",
        "policy_parent",
        PARENT_TASK,
        SpawnOverrides {
            context_policy: Some(policy),
            inherit_to_children: Some(true),
        },
        "root-spawn-parent-response",
    )
    .await;
    mount_spawn_response(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, PARENT_TASK)
                && !has_function_call_output(request, "parent-spawn-grandchild")
        },
        "parent-spawn-grandchild",
        "reducer_grandchild",
        GRANDCHILD_TASK,
        SpawnOverrides {
            inherit_to_children: Some(true),
            ..SpawnOverrides::default()
        },
        "parent-spawn-grandchild-response",
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "parent-spawn-grandchild"),
        "parent-completion",
        "parent-completion-message",
        "grandchild started",
    )
    .await;

    let elidable_contents = format!("{ELIDABLE_MARKER}\n").repeat(12_000);
    let recent_contents = format!("{RECENT_MARKER}\n").repeat(4_000);
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.truncation_policy = TruncationPolicyConfig::tokens(96_000);
        })
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;
    std::fs::write(
        test.workspace_path("reduction-older.txt"),
        elidable_contents,
    )?;
    std::fs::write(test.workspace_path("reduction-recent.txt"), recent_contents)?;

    let elidable_call = ev_exec_command_call_with_args(
        "elidable-output",
        &json!({
            "cmd": read_output_command("reduction-older.txt"),
            "max_output_tokens": 100_000
        }),
    );
    let recent_call = ev_exec_command_call_with_args(
        "recent-output",
        &json!({
            "cmd": delayed_read_output_command("reduction-recent.txt"),
            "max_output_tokens": 100_000
        }),
    );
    let _child_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, GRANDCHILD_TASK)
                && !has_function_call_output(request, "parent-spawn-grandchild")
                && !has_input_type(request, "compaction_trigger")
                && !has_function_call_output(request, "elidable-output")
                && !has_function_call_output(request, "recent-output")
        },
        sse(vec![
            ev_response_created("grandchild-tools-response"),
            elidable_call,
            recent_call,
            ev_completed("grandchild-tools-response"),
        ]),
    )
    .await;
    let continuation = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| {
            has_function_call_output(request, "elidable-output")
                && has_function_call_output(request, "recent-output")
                && !has_input_type(request, "compaction_trigger")
        },
        "grandchild-continuation-response",
        "grandchild-continuation-message",
        "completed the assigned work",
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "root-spawn-parent"),
        "root-spawn-parent-completion",
        "root-spawn-parent-completion-message",
        "parent finished",
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, LIST_PROMPT),
        sse(vec![
            ev_response_created("root-list-agents-response"),
            collaboration_call("root-list-agents", "list_agents", &json!({})),
            ev_completed("root-list-agents-response"),
        ]),
    )
    .await;
    let list_agents = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "root-list-agents"),
        "root-list-agents-completion",
        "root-list-agents-completion-message",
        "inspected agent policy",
    )
    .await;

    test.submit_turn_with_permission_profile(ROOT_PROMPT, PermissionProfile::workspace_write())
        .await?;
    wait_for_child_turns(&test, &server, 2).await?;
    test.submit_turn(LIST_PROMPT).await?;

    let list_output: Value = serde_json::from_str(
        &list_agents
            .function_call_output_text("root-list-agents")
            .expect("list_agents result should be recorded"),
    )?;
    let grandchild = list_output["agents"]
        .as_array()
        .expect("list_agents should return an agents array")
        .iter()
        .find(|agent| {
            agent["agent_name"]
                .as_str()
                .is_some_and(|name| name.ends_with("/reducer_grandchild"))
        })
        .expect("the reducer grandchild should be listed");
    let context_reduction = &grandchild["context_reduction"];
    let desired_policy = &context_reduction["desired_policy"];
    let attempt_summary = context_reduction.to_string();

    let all_captured_requests = continuation.requests();
    let child_requests = all_captured_requests
        .iter()
        .filter(|request| captured_is_subagent_request(request))
        .collect::<Vec<_>>();
    let continuation_requests = child_requests
        .iter()
        .filter(|request| {
            request
                .function_call_output_text("elidable-output")
                .is_some()
                && request.function_call_output_text("recent-output").is_some()
                && request.inputs_of_type("compaction_trigger").is_empty()
        })
        .collect::<Vec<_>>();
    let continuation_summaries = child_requests
        .iter()
        .copied()
        .map(captured_response_request_summary)
        .collect::<Vec<_>>();
    assert_eq!(
        continuation_requests.len(),
        1,
        "expected one post-shake sample without a compact request; child requests: {continuation_summaries:?}; reduction telemetry: {attempt_summary}; all requests:\n{}",
        response_request_diagnostics(&server).await
    );
    let child_request = &continuation_requests[0];
    assert!(
        child_request.has_function_call("elidable-output"),
        "older tool call missing; telemetry: {attempt_summary}"
    );
    assert!(
        child_request.has_function_call("recent-output"),
        "recent tool call missing; telemetry: {attempt_summary}"
    );
    let elided_output = child_request
        .function_call_output_text("elidable-output")
        .expect("the older tool output should keep its paired result item");
    assert!(
        elided_output.contains("[shaken ~"),
        "older output lacked a recovery placeholder; telemetry: {attempt_summary}; excerpt: {:?}",
        elided_output.chars().take(120).collect::<String>()
    );
    assert!(
        !elided_output.contains(ELIDABLE_MARKER),
        "older output marker survived; telemetry: {attempt_summary}"
    );
    assert!(
        child_request
            .function_call_output_text("recent-output")
            .is_some_and(|output| output.contains(RECENT_MARKER)),
        "the newest tool result should remain inside the protected recent tail; telemetry: {attempt_summary}"
    );
    assert!(
        child_request
            .body_json()
            .to_string()
            .contains("MUST_RETAIN_ASSIGNMENT_CONSTRAINT"),
        "the child continuation must retain its assignment constraint; telemetry: {attempt_summary}"
    );
    assert!(
        child_request
            .body_json()
            .get("previous_response_id")
            .and_then(Value::as_str)
            .is_none(),
        "a shake rewrite must continue with a full-history request; telemetry: {attempt_summary}"
    );

    assert_eq!(desired_policy["enabled"], true);
    assert_eq!(desired_policy["threshold_tokens"], THRESHOLD_TOKENS);
    assert_eq!(desired_policy["check_after_tools"], true);
    assert_eq!(desired_policy["shake"], "on");
    assert_eq!(desired_policy["on_failure"], "stop");
    assert_eq!(
        desired_policy["provenance"]["threshold_tokens"],
        "inherited"
    );
    assert!(
        context_reduction["active_context_token_basis"]
            .as_str()
            .is_some()
    );
    assert!(
        desired_policy["effective_threshold_tokens"]
            .as_i64()
            .is_some_and(|tokens| tokens <= THRESHOLD_TOKENS)
    );
    assert_eq!(
        context_reduction["last_reduction"]["outcome"], "shaken",
        "{attempt_summary}"
    );
    assert!(
        context_reduction["last_reduction"]["before_tokens"]
            .as_i64()
            .is_some_and(|tokens| tokens > THRESHOLD_TOKENS),
        "the reduction should estimate usage when the mock completion has no token usage; telemetry: {attempt_summary}"
    );
    assert!(
        context_reduction["last_reduction"]["after_tokens"]
            .as_i64()
            .is_some_and(|tokens| tokens < THRESHOLD_TOKENS),
        "the successful shake should put surviving history below the configured threshold; telemetry: {attempt_summary}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ancestor_policy_update_persists_and_sibling_target_is_rejected() -> Result<()> {
    const ROOT_PROMPT: &str = "spawn two policy target workers";
    const BETA_TASK: &str = "complete the beta worker";
    const ALPHA_TASK: &str = "try to update the beta sibling policy";

    let server = start_mock_server().await;
    mount_spawn_response(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_PROMPT),
        "root-spawn-beta",
        "beta",
        BETA_TASK,
        SpawnOverrides::default(),
        "root-spawn-beta-response",
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request) && body_contains(request, BETA_TASK)
        },
        "beta-completion",
        "beta-completion-message",
        "beta complete",
    )
    .await;
    mount_spawn_response(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "root-spawn-beta"),
        "root-spawn-alpha",
        "alpha",
        ALPHA_TASK,
        SpawnOverrides::default(),
        "root-spawn-alpha-response",
    )
    .await;

    let sibling_update = json!({
        "target": "/root/beta",
        "context_policy": { "enabled": true }
    });
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, ALPHA_TASK)
                && !has_function_call_output(request, "alpha-sibling-update")
        },
        sse(vec![
            ev_response_created("alpha-sibling-update-response"),
            collaboration_call(
                "alpha-sibling-update",
                "set_agent_context_policy",
                &sibling_update,
            ),
            ev_completed("alpha-sibling-update-response"),
        ]),
    )
    .await;
    let sibling_rejection = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "alpha-sibling-update"),
        "alpha-sibling-rejection-completion",
        "alpha-sibling-rejection-message",
        "could not update a sibling",
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "root-spawn-alpha"),
        sse(vec![
            ev_response_created("root-update-beta-response"),
            collaboration_call(
                "root-update-beta",
                "set_agent_context_policy",
                &json!({
                    "target": "/root/beta",
                    "context_policy": {
                        "enabled": false,
                        "threshold_tokens": 512,
                        "check_after_tools": false,
                        "shake": "off",
                        "on_failure": "continue"
                    },
                    "inherit_to_children": true
                }),
            ),
            ev_completed("root-update-beta-response"),
        ]),
    )
    .await;
    let update_result = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "root-update-beta"),
        sse(vec![
            ev_response_created("root-list-agents-response"),
            collaboration_call("root-list-agents", "list_agents", &json!({})),
            ev_completed("root-list-agents-response"),
        ]),
    )
    .await;
    let list_agents = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "root-list-agents"),
        "root-list-agents-completion",
        "root-list-agents-completion-message",
        "inspected the persisted policy",
    )
    .await;

    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn(ROOT_PROMPT).await?;
    wait_for_child_turns(&test, &server, 2).await?;

    let sibling_error = sibling_rejection
        .function_call_output_text("alpha-sibling-update")
        .expect("the sibling setter result should be recorded as a tool error");
    assert!(
        sibling_error.contains("not a descendant of the caller"),
        "unexpected sibling setter result: {sibling_error}"
    );

    let update: Value = serde_json::from_str(
        &update_result
            .function_call_output_text("root-update-beta")
            .expect("the ancestor setter result should be recorded"),
    )?;
    assert!(
        update["desired_revision"]
            .as_u64()
            .is_some_and(|revision| revision >= 2)
    );
    assert!(update.get("applied_revision").is_some());
    assert_eq!(update["persisted"], true);

    let agents: Value = serde_json::from_str(
        &list_agents
            .function_call_output_text("root-list-agents")
            .expect("list_agents should report the updated child"),
    )?;
    let beta = agents["agents"]
        .as_array()
        .expect("list_agents should return an agents array")
        .iter()
        .find(|agent| {
            agent["agent_name"]
                .as_str()
                .is_some_and(|name| name.ends_with("/beta"))
        })
        .expect("the beta child should remain listed after completion");
    let policy = &beta["context_reduction"]["desired_policy"];
    assert_eq!(policy["enabled"], false);
    assert_eq!(policy["threshold_tokens"], 512);
    assert_eq!(policy["check_after_tools"], false);
    assert_eq!(policy["shake"], "off");
    assert_eq!(policy["on_failure"], "continue");
    assert_eq!(policy["provenance"]["threshold_tokens"], "local");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_after_tools_false_keeps_the_legacy_child_context() -> Result<()> {
    const ROOT_PROMPT: &str = "spawn a child with post-tool checks disabled";
    const CHILD_TASK: &str = "read both large files and finish the task";
    const ELIDABLE_MARKER: &str = "DISABLED_CHECK_OLDER_OUTPUT_19";
    const RECENT_MARKER: &str = "DISABLED_CHECK_RECENT_OUTPUT_28";

    let server = start_mock_server().await;
    let disabled_policy = json!({
        "enabled": true,
        "threshold_tokens": 60_000,
        "check_after_tools": false,
        "shake": "on",
        "on_failure": "stop"
    });
    mount_spawn_response(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_PROMPT),
        "root-spawn-child",
        "unchecked_child",
        CHILD_TASK,
        SpawnOverrides {
            context_policy: Some(disabled_policy),
            inherit_to_children: Some(true),
        },
        "root-spawn-child-response",
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "root-spawn-child"),
        "root-completion",
        "root-completion-message",
        "spawned child",
    )
    .await;

    let elidable_contents = format!("{ELIDABLE_MARKER}\n").repeat(4_500);
    let recent_contents = format!("{RECENT_MARKER}\n").repeat(4_500);
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.truncation_policy = TruncationPolicyConfig::tokens(96_000);
        })
        .with_config(configure_multi_agent_v2)
        .build_with_auto_env(&server)
        .await?;
    std::fs::write(test.workspace_path("disabled-older.txt"), elidable_contents)?;
    std::fs::write(test.workspace_path("disabled-recent.txt"), recent_contents)?;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, CHILD_TASK)
                && !has_function_call_output(request, "unchecked-elidable-output")
                && !has_function_call_output(request, "unchecked-recent-output")
        },
        sse(vec![
            ev_response_created("unchecked-child-tools-response"),
            ev_exec_command_call_with_args(
                "unchecked-elidable-output",
                &json!({
                    "cmd": read_output_command("disabled-older.txt"),
                    "max_output_tokens": 100_000
                }),
            ),
            ev_exec_command_call_with_args(
                "unchecked-recent-output",
                &json!({
                    "cmd": read_output_command("disabled-recent.txt"),
                    "max_output_tokens": 100_000
                }),
            ),
            ev_completed("unchecked-child-tools-response"),
        ]),
    )
    .await;
    let continuation = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| {
            has_function_call_output(request, "unchecked-elidable-output")
                && has_function_call_output(request, "unchecked-recent-output")
        },
        "unchecked-child-continuation-response",
        "unchecked-child-continuation-message",
        "finished without after-tool context reduction",
    )
    .await;

    test.submit_turn_with_permission_profile(ROOT_PROMPT, PermissionProfile::workspace_write())
        .await?;
    wait_for_child_turns(&test, &server, 1).await?;

    let captured_requests = continuation.requests();
    let child_requests = captured_requests
        .iter()
        .filter(|request| captured_is_subagent_request(request))
        .filter(|request| {
            request
                .function_call_output_text("unchecked-elidable-output")
                .is_some()
                && request
                    .function_call_output_text("unchecked-recent-output")
                    .is_some()
                && request.inputs_of_type("compaction_trigger").is_empty()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        child_requests.len(),
        1,
        "expected one child continuation; requests: {:?}",
        captured_requests
            .iter()
            .map(captured_response_request_summary)
            .collect::<Vec<_>>()
    );
    let child_request = child_requests[0];
    assert!(child_request.has_function_call("unchecked-elidable-output"));
    assert!(child_request.has_function_call("unchecked-recent-output"));
    assert!(
        child_request
            .function_call_output_text("unchecked-elidable-output")
            .is_some_and(|output| output.contains(ELIDABLE_MARKER))
    );
    assert!(
        child_request
            .function_call_output_text("unchecked-recent-output")
            .is_some_and(|output| output.contains(RECENT_MARKER))
    );
    assert!(!child_request.body_json().to_string().contains("[shaken ~"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_stop_after_insufficient_compaction_notifies_parent_without_resampling()
-> Result<()> {
    const ROOT_PROMPT: &str = "spawn a child that exceeds its reduction threshold";
    const CHILD_TASK: &str = "run the operation before returning";
    const FOLLOWUP_PROMPT: &str = "check the stopped child result";
    const STOP_MESSAGE: &str =
        "context reduction completed but the child remains above its 60000 token threshold";

    let server = start_mock_server().await;
    let compact_summary = "STRICT_STOP_COMPACT_SUMMARY\n".to_string().repeat(20_000);
    mount_spawn_response(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_PROMPT),
        "root-spawn-stop-child",
        "stop_child",
        CHILD_TASK,
        SpawnOverrides {
            context_policy: Some(json!({
                "enabled": true,
                "threshold_tokens": 60_000,
                "check_after_tools": true,
                "shake": "off"
            })),
            inherit_to_children: Some(true),
        },
        "root-spawn-stop-child-response",
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "root-spawn-stop-child"),
        "root-stop-child-completion",
        "root-stop-child-completion-message",
        "spawned stop child",
    )
    .await;

    let child_initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, CHILD_TASK)
                && !has_input_type(request, "compaction_trigger")
                && !has_function_call_output(request, "stop-child-tool")
        },
        sse(vec![
            ev_response_created("stop-child-initial-response"),
            ev_exec_command_call("stop-child-tool", "echo completed-before-compaction"),
            ev_completed_with_tokens("stop-child-initial-complete", 80_000),
        ]),
    )
    .await;
    let compact_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_input_type(request, "compaction_trigger"),
        sse(vec![
            ev_response_created("stop-child-compact-response"),
            ev_compaction_output(&compact_summary),
            ev_completed_with_tokens("stop-child-compact-complete", 80_000),
        ]),
    )
    .await;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FOLLOWUP_PROMPT),
        sse(vec![
            ev_response_created("root-wait-stop-child-response"),
            collaboration_call("root-wait-stop-child", "wait_agent", &json!({})),
            ev_completed("root-wait-stop-child-response"),
        ]),
    )
    .await;
    let parent_notification = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| {
            has_function_call_output(request, "root-wait-stop-child")
                && body_contains(request, "Message Type: FINAL_ANSWER")
                && body_contains(request, STOP_MESSAGE)
        },
        "root-observed-stop-notification",
        "root-observed-stop-notification-message",
        "observed strict stop",
    )
    .await;

    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(|config| {
            configure_multi_agent_v2(config);
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn_with_permission_profile(ROOT_PROMPT, PermissionProfile::workspace_write())
        .await?;
    test.submit_turn(FOLLOWUP_PROMPT).await?;

    let child_initial_requests = child_initial
        .requests()
        .into_iter()
        .filter(|request| {
            captured_is_subagent_request(request)
                && request.body_json().to_string().contains(CHILD_TASK)
                && request.inputs_of_type("compaction_trigger").is_empty()
                && request
                    .function_call_output_text("stop-child-tool")
                    .is_none()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        child_initial_requests.len(),
        1,
        "unexpected requests matched the strict-stop initial-sample route:\n{}",
        response_request_diagnostics(&server).await
    );
    assert!(
        child_initial_requests[0]
            .body_json()
            .to_string()
            .contains(CHILD_TASK),
        "the initial child request should contain its assignment"
    );
    let compact_requests = compact_request
        .requests()
        .into_iter()
        .filter(|request| {
            captured_is_subagent_request(request)
                && !request.inputs_of_type("compaction_trigger").is_empty()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        compact_requests.len(),
        1,
        "unexpected requests matched the strict-stop compact route:\n{}",
        response_request_diagnostics(&server).await
    );
    assert_eq!(
        compact_requests[0]
            .inputs_of_type("compaction_trigger")
            .len(),
        1,
        "the child should try compaction after shake is disabled"
    );
    let parent_requests = parent_notification
        .requests()
        .into_iter()
        .filter(|request| {
            request
                .function_call_output_text("root-wait-stop-child")
                .is_some()
                && request.body_json().to_string().contains(STOP_MESSAGE)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        parent_requests.len(),
        1,
        "expected one parent request with the strict-stop notification; requests: {:?}",
        parent_notification
            .requests()
            .iter()
            .map(captured_response_request_summary)
            .collect::<Vec<_>>()
    );
    let parent_request = &parent_requests[0];
    let parent_body = parent_request.body_json().to_string();
    assert!(parent_body.contains("Message Type: FINAL_ANSWER"));
    assert!(parent_body.contains("Agent errored:"));
    assert!(parent_body.contains(STOP_MESSAGE));

    let child_thread_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|thread_id| *thread_id != test.session_configured.thread_id)
        .expect("the spawned child thread should remain available")
        .to_string();
    let requests = server.received_requests().await.unwrap_or_default();
    let child_response_requests = requests
        .iter()
        .filter(|request| {
            request.method == "POST"
                && request.url.path().ends_with("/responses")
                && serde_json::from_slice::<Value>(&request.body)
                    .ok()
                    .is_some_and(|body| {
                        body["client_metadata"]["thread_id"].as_str()
                            == Some(child_thread_id.as_str())
                    })
        })
        .count();
    assert_eq!(
        child_response_requests, 2,
        "the child should issue its initial sample and one compact request, with no continuation sample"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_policy_survives_compaction_and_resume() -> Result<()> {
    const ROOT_PROMPT: &str = "spawn a worker with a persistent context policy";
    const CHILD_TASK: &str = "COMPAC_RESUME_CHILD_ASSIGNED_TASK complete the requested operation";
    const COMPACTED_SUMMARY: &str = "COMPACT_RESUME_POLICY_SUMMARY preserve the child policy";
    const LIST_PROMPT: &str = "inspect the resumed child policy";
    const THRESHOLD_TOKENS: i64 = 60_000;

    let server = start_mock_server().await;
    mount_spawn_response(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_PROMPT),
        "root-spawn-resume-child",
        "resume_child",
        CHILD_TASK,
        SpawnOverrides {
            context_policy: Some(json!({
                "enabled": true,
                "threshold_tokens": THRESHOLD_TOKENS,
                "check_after_tools": true,
                "shake": "off"
            })),
            inherit_to_children: Some(true),
        },
        "root-spawn-resume-child-response",
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "root-spawn-resume-child"),
        "root-resume-child-completion",
        "root-resume-child-completion-message",
        "spawned persistent-policy child",
    )
    .await;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, CHILD_TASK)
                && !has_input_type(request, "compaction_trigger")
                && !has_function_call_output(request, "resume-child-tool")
        },
        sse(vec![
            ev_response_created("resume-child-initial-response"),
            ev_exec_command_call("resume-child-tool", "echo completed-before-compaction"),
            ev_completed_with_tokens("resume-child-initial-complete", 80_000),
        ]),
    )
    .await;
    let compact_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_input_type(request, "compaction_trigger"),
        sse(vec![
            ev_response_created("resume-child-compact-response"),
            ev_compaction_output(COMPACTED_SUMMARY),
            ev_completed_with_tokens("resume-child-compact-complete", 20_000),
        ]),
    )
    .await;
    let child_continuation = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, COMPACTED_SUMMARY)
                && !has_input_type(request, "compaction_trigger")
        },
        "resume-child-continuation-response",
        "resume-child-continuation-message",
        "operation complete after compaction",
    )
    .await;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, LIST_PROMPT),
        sse(vec![
            ev_response_created("resumed-root-list-agents-response"),
            collaboration_call("resumed-root-list-agents", "list_agents", &json!({})),
            ev_completed("resumed-root-list-agents-response"),
        ]),
    )
    .await;
    let resumed_list_agents = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "resumed-root-list-agents"),
        "resumed-root-list-agents-completion",
        "resumed-root-list-agents-completion-message",
        "read persisted child policy",
    )
    .await;

    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(|config| {
            configure_multi_agent_v2(config);
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn_with_permission_profile(ROOT_PROMPT, PermissionProfile::workspace_write())
        .await?;
    wait_for_child_turns(&test, &server, 1).await?;

    let compact_requests = compact_request
        .requests()
        .into_iter()
        .filter(|request| {
            captured_is_subagent_request(request)
                && !request.inputs_of_type("compaction_trigger").is_empty()
        })
        .collect::<Vec<_>>();
    assert_eq!(compact_requests.len(), 1);
    let child_continuations = child_continuation
        .requests()
        .into_iter()
        .filter(|request| {
            captured_is_subagent_request(request)
                && request.body_json().to_string().contains(COMPACTED_SUMMARY)
                && request.inputs_of_type("compaction_trigger").is_empty()
        })
        .collect::<Vec<_>>();
    assert_eq!(child_continuations.len(), 1);

    let child_thread_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|thread_id| *thread_id != test.session_configured.thread_id)
        .expect("the compacted child thread should remain available");
    let child = test.thread_manager.get_thread(child_thread_id).await?;
    child.flush_rollout().await?;
    child.shutdown_and_wait().await?;
    drop(child);

    let mut resume_builder = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(configure_multi_agent_v2);
    let resumed = resume_builder.restart(&server, &test).await?;
    resumed
        .thread_manager
        .ensure_multi_agent_v2_child_loaded(child_thread_id)
        .await?;
    resumed.submit_turn(LIST_PROMPT).await?;

    let list_output: Value = serde_json::from_str(
        &resumed_list_agents
            .function_call_output_text("resumed-root-list-agents")
            .expect("resumed list_agents result should be recorded"),
    )?;
    let child = list_output["agents"]
        .as_array()
        .expect("list_agents should return an agents array")
        .iter()
        .find(|agent| {
            agent["agent_name"]
                .as_str()
                .is_some_and(|name| name.ends_with("/resume_child"))
        })
        .expect("the resumed child should be listed");
    let desired_policy = &child["context_reduction"]["desired_policy"];
    assert_eq!(desired_policy["enabled"], true);
    assert_eq!(desired_policy["threshold_tokens"], THRESHOLD_TOKENS);
    assert_eq!(desired_policy["check_after_tools"], true);
    assert_eq!(desired_policy["shake"], "off");
    assert_eq!(desired_policy["provenance"]["threshold_tokens"], "local");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_steer_survives_compaction_with_plugin_tool_smoke() -> Result<()> {
    const ROOT_PROMPT: &str = "spawn a worker that will receive a plugin mention";
    const CHILD_TASK: &str = "QUEUED_PLUGIN_TASK wait for more direction and then use the plugin";
    const LIST_PROMPT: &str = "inspect the child after its queued plugin mention";
    const COMPACTED_SUMMARY: &str = "QUEUED_PLUGIN_COMPACTED_SUMMARY";
    const QUEUED_CONSTRAINT_MARKER: &str =
        "STEER_CONSTRAINT_7D31 preserve tool call and result pairing";
    const PLUGIN_MENTION_PATH: &str = "plugin://sample@test";

    let server_bin = stdio_server_bin()?;
    let plugin_home = Arc::new(TempDir::new()?);
    write_sample_plugin_mcp_fixture(plugin_home.as_ref(), &server_bin)?;

    let server = start_mock_server().await;
    mount_spawn_response(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_PROMPT),
        "root-spawn-queued-plugin-child",
        "queued_plugin_child",
        CHILD_TASK,
        SpawnOverrides {
            context_policy: Some(json!({
                "enabled": true,
                "threshold_tokens": 60_000,
                "check_after_tools": true,
                "shake": "off"
            })),
            inherit_to_children: Some(true),
        },
        "root-spawn-queued-plugin-child-response",
    )
    .await;
    mount_assistant_completion(
        &server,
        |request: &wiremock::Request| {
            has_function_call_output(request, "root-spawn-queued-plugin-child")
        },
        "root-queued-plugin-child-completion",
        "root-queued-plugin-child-completion-message",
        "spawned queued-plugin child",
    )
    .await;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, CHILD_TASK)
                && !has_input_type(request, "compaction_trigger")
        },
        sse(vec![
            ev_response_created("queued-plugin-child-initial-response"),
            ev_exec_command_call(
                "queued-plugin-initial-gate",
                &wait_for_release_command("release-queued-plugin-initial"),
            ),
            ev_completed_with_tokens("queued-plugin-child-initial-complete", 100),
        ]),
    )
    .await;
    let child_after_initial_tool = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && has_function_call_output(request, "queued-plugin-initial-gate")
                && !has_input_type(request, "compaction_trigger")
        },
        sse_response(sse(vec![
            ev_response_created("queued-plugin-child-after-tool-response"),
            ev_assistant_message(
                "queued-plugin-child-after-tool-message",
                "The initial tool finished; continue after the queued direction.",
            ),
            ev_completed_with_tokens("queued-plugin-child-after-tool-complete", 80_000),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    let compact_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_input_type(request, "compaction_trigger"),
        sse(vec![
            ev_response_created("queued-plugin-child-compact-response"),
            ev_compaction_output(COMPACTED_SUMMARY),
            ev_completed_with_tokens("queued-plugin-child-compact-complete", 20_000),
        ]),
    )
    .await;
    let child_after_compaction = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && body_contains(request, COMPACTED_SUMMARY)
                && body_contains(request, QUEUED_CONSTRAINT_MARKER)
                && !has_input_type(request, "compaction_trigger")
        },
        sse(vec![
            ev_response_created("queued-plugin-child-mcp-response"),
            ev_tool_search_call(
                "queued-plugin-after-compact-search",
                &json!({"query": "echo"}),
            ),
            ev_completed("queued-plugin-child-mcp-complete"),
        ]),
    )
    .await;
    let child_after_plugin_search = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && has_tool_search_output(request, "queued-plugin-after-compact-search")
                && !has_input_type(request, "compaction_trigger")
        },
        sse(vec![
            ev_response_created("queued-plugin-child-echo-response"),
            ev_function_call_with_namespace(
                "queued-plugin-echo",
                "mcp__sample",
                "echo",
                &json!({"message": "plugin tool smoke call after compaction"}).to_string(),
            ),
            ev_completed("queued-plugin-child-echo-complete"),
        ]),
    )
    .await;
    let child_after_tool_output = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| {
            is_subagent_request(request)
                && has_function_call_output(request, "queued-plugin-echo")
                && !has_input_type(request, "compaction_trigger")
        },
        "queued-plugin-child-final-response",
        "queued-plugin-child-final-message",
        "finished after the queued plugin tool call",
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, LIST_PROMPT),
        sse(vec![
            ev_response_created("queued-plugin-root-list-response"),
            collaboration_call("queued-plugin-root-list", "list_agents", &json!({})),
            ev_completed("queued-plugin-root-list-response"),
        ]),
    )
    .await;
    let list_agents = mount_assistant_completion(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "queued-plugin-root-list"),
        "queued-plugin-root-list-completion",
        "queued-plugin-root-list-completion-message",
        "inspected queued plugin child",
    )
    .await;

    let test = test_codex()
        .with_home(Arc::clone(&plugin_home))
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.truncation_policy = TruncationPolicyConfig::tokens(96_000);
        })
        .with_config(configure_multi_agent_v2)
        .build(&server)
        .await?;
    test.submit_turn_with_permission_profile(ROOT_PROMPT, PermissionProfile::workspace_write())
        .await?;

    let child_thread_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|thread_id| *thread_id != test.session_configured.thread_id)
        .expect("the queued-plugin child should have been spawned");
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    wait_for_event_with_timeout(
        &child_thread,
        |event| matches!(event, EventMsg::ExecCommandBegin(begin) if begin.call_id == "queued-plugin-initial-gate"),
        Duration::from_secs(20),
    )
    .await;

    std::fs::write(
        test.workspace_path("release-queued-plugin-initial"),
        "released",
    )?;
    let staged_child_request = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(request) = child_after_initial_tool
                .requests()
                .into_iter()
                .find(|request| {
                    captured_is_subagent_request(request)
                        && request
                            .function_call_output_text("queued-plugin-initial-gate")
                            .is_some()
                })
            {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert!(
        !staged_child_request
            .body_json()
            .to_string()
            .contains(QUEUED_CONSTRAINT_MARKER),
        "the staged request should be captured before the queued mention"
    );
    child_thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![
            UserInput::Text {
                text: QUEUED_CONSTRAINT_MARKER.to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Mention {
                name: "sample".to_string(),
                path: PLUGIN_MENTION_PATH.to_string(),
            },
        ]))
        .await?;
    wait_for_child_turns(&test, &server, 1).await?;
    test.submit_turn(LIST_PROMPT).await?;
    let child_request_diagnostics = child_response_request_diagnostics(&server).await;

    let compact_requests = compact_request
        .requests()
        .into_iter()
        .filter(|request| {
            captured_is_subagent_request(request)
                && !request.inputs_of_type("compaction_trigger").is_empty()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        compact_requests.len(),
        1,
        "unexpected compact request count; child requests:\n{child_request_diagnostics}"
    );
    assert!(
        compact_requests[0]
            .body_json()
            .to_string()
            .contains(QUEUED_CONSTRAINT_MARKER),
        "the queued text constraint must reach the remote compaction request; child requests:\n{child_request_diagnostics}"
    );

    let child_requests = child_after_compaction
        .requests()
        .into_iter()
        .filter(|request| {
            captured_is_subagent_request(request)
                && request.body_json().to_string().contains(COMPACTED_SUMMARY)
                && request
                    .body_json()
                    .to_string()
                    .contains(QUEUED_CONSTRAINT_MARKER)
                && !captured_has_tool_search_output(request, "queued-plugin-after-compact-search")
                && request.inputs_of_type("compaction_trigger").is_empty()
        })
        .collect::<Vec<_>>();
    assert_eq!(child_requests.len(), 1);

    let post_compaction_search_requests = child_after_plugin_search
        .requests()
        .into_iter()
        .filter(|request| {
            captured_is_subagent_request(request)
                && captured_has_tool_search_output(request, "queued-plugin-after-compact-search")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        post_compaction_search_requests.len(),
        1,
        "missing post-compaction echo search result; child requests:\n{child_request_diagnostics}"
    );
    assert!(
        namespace_child_tool(
            &post_compaction_search_requests[0]
                .tool_search_output("queued-plugin-after-compact-search"),
            "mcp__sample",
            "echo"
        )
        .is_some(),
        "plugin echo should be searchable in this post-compaction smoke; child requests:\n{child_request_diagnostics}"
    );

    let child_output_requests = child_after_tool_output
        .requests()
        .into_iter()
        .filter(|request| {
            captured_is_subagent_request(request)
                && request
                    .function_call_output_text("queued-plugin-echo")
                    .is_some()
                && request.inputs_of_type("compaction_trigger").is_empty()
        })
        .collect::<Vec<_>>();
    assert_eq!(child_output_requests.len(), 1);
    let child_output_request = &child_output_requests[0];
    assert!(child_output_request.has_function_call("queued-plugin-echo"));
    assert!(
        child_output_request
            .function_call_output_text("queued-plugin-echo")
            .is_some_and(|output| output.contains("plugin tool smoke call after compaction"))
    );

    let agents: Value = serde_json::from_str(
        &list_agents
            .function_call_output_text("queued-plugin-root-list")
            .expect("list_agents should record the queued-plugin child"),
    )?;
    assert!(agents["agents"].as_array().is_some_and(|agents| {
        agents.iter().any(|agent| {
            agent["agent_name"]
                .as_str()
                .is_some_and(|name| name.ends_with("/queued_plugin_child"))
        })
    }));

    Ok(())
}
