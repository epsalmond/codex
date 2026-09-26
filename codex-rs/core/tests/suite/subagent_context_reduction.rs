//! Integration coverage for `[subagent_context_reduction]`: spawned children run the
//! shake-then-compact roll-over at a lowered limit, and fail instead of looping when
//! compaction cannot get them back under it. The root agent keeps its prior roll-over.

use anyhow::Context;
use anyhow::Result;
use codex_core::config::SubagentContextReductionConfig;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_exec_command_call_with_args;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use test_case::test_case;
use wiremock::MockServer;

const MODEL: &str = "gpt-5.6-sol";
const ROOT_PROMPT: &str = "spawn the worker";
const CHILD_TASK: &str = "CHILD_TASK_MARKER do the assigned work";
const SPAWN_CALL: &str = "root-spawn";
const CHILD_TOOL_CALL: &str = "child-tool";
const SUMMARY: &str = "CHILD_COMPACTION_SUMMARY";
const ELIDABLE_MARKER: &str = "ELIDABLE_TOOL_OUTPUT_MARKER_71";
const RECENT_MARKER: &str = "RECENT_TOOL_OUTPUT_MARKER_92";
const READ_COMMAND: &str = if cfg!(windows) { "type" } else { "cat" };

fn json_body(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap_or_default()
}

/// The `function_call_output` item for `call_id` in a request body.
fn call_output<'a>(body: &'a Value, call_id: &str) -> Option<&'a Value> {
    body["input"].as_array()?.iter().find(|item| {
        item["type"] == "function_call_output" && item["call_id"].as_str() == Some(call_id)
    })
}

fn is_compaction(body: &Value) -> bool {
    body["input"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["type"] == "compaction_trigger")
    })
}

/// Subagent requests carry the spawning turn in their turn metadata.
fn is_child(body: &Value) -> bool {
    body["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .and_then(|metadata| serde_json::from_str::<Value>(metadata).ok())
        .is_some_and(|metadata| metadata["parent_turn_id"].is_string())
}

/// Mounts one SSE response with id `id` for the first request whose body satisfies
/// `matcher`.
async fn mount(
    server: &MockServer,
    id: &str,
    matcher: impl Fn(&Value) -> bool + Send + Sync + 'static,
    mut events: Vec<Value>,
) {
    events.insert(0, ev_response_created(id));
    if !events
        .iter()
        .any(|event| event["type"] == "response.completed")
    {
        events.push(ev_completed(id));
    }
    mount_response_once_match(
        server,
        move |request: &wiremock::Request| matcher(&json_body(request)),
        sse_response(sse(events)),
    )
    .await;
}

fn collaboration_call(call_id: &str, tool_name: &str, arguments: Value) -> Value {
    ev_function_call_with_namespace(call_id, "collaboration", tool_name, &arguments.to_string())
}

fn spawn_call(call_id: &str, task_name: &str, message: &str) -> Value {
    let arguments = json!({"message": message, "task_name": task_name, "fork_turns": "none"});
    collaboration_call(call_id, "spawn_agent", arguments)
}

fn compaction_output(summary: &str) -> Value {
    json!({
        "type": "response.output_item.done",
        "item": {"type": "compaction", "encrypted_content": summary}
    })
}

/// Two reads: ~87k tokens of old output that auto-shake can elide, then a ~29k-token
/// recent tail it protects. `write_read_files` creates the files.
fn file_reads() -> Vec<Value> {
    Vec::from(
        [("elidable", "older.txt"), ("recent", "recent.txt")].map(|(call_id, path)| {
            let args =
                json!({"cmd": format!("{READ_COMMAND} {path}"), "max_output_tokens": 100_000});
            ev_exec_command_call_with_args(call_id, &args)
        }),
    )
}

fn write_read_files(test: &TestCodex) -> Result<()> {
    let older = format!("{ELIDABLE_MARKER}\n").repeat(12_000);
    std::fs::write(test.workspace_path("older.txt"), older)?;
    let recent = format!("{RECENT_MARKER}\n").repeat(4_000);
    std::fs::write(test.workspace_path("recent.txt"), recent)?;
    Ok(())
}

fn reduction_test(enabled: bool, threshold_tokens: u64) -> TestCodexBuilder {
    test_codex()
        .with_model(MODEL)
        .with_model_info_override(MODEL, |model| {
            model.truncation_policy = TruncationPolicyConfig::tokens(/*limit*/ 96_000);
        })
        .with_config(move |config| {
            for feature in [Feature::Collab, Feature::MultiAgentV2] {
                config
                    .features
                    .enable(feature)
                    .expect("test config should allow collaboration features");
            }
            config.tool_output_token_limit = Some(100_000);
            // `wait_agent` waits for new mailbox activity; a child that already finished
            // may have delivered its completion before the call.
            config.multi_agent_v2.default_wait_timeout_ms = 1_000;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.subagent_context_reduction = SubagentContextReductionConfig {
                enabled,
                threshold_tokens,
            };
        })
}

/// The root spawns one child, then finishes its turn.
async fn mount_root_spawn(server: &MockServer, task_name: &str, task: &str) {
    let spawn = spawn_call(SPAWN_CALL, task_name, task);
    let not_spawned = |body: &Value| {
        body.to_string().contains(ROOT_PROMPT) && call_output(body, SPAWN_CALL).is_none()
    };
    mount(server, "root-spawn", not_spawned, vec![spawn]).await;
    let spawned = |body: &Value| call_output(body, SPAWN_CALL).is_some();
    let done = ev_assistant_message("root-spawned", "spawned");
    mount(server, "root-spawned", spawned, vec![done]).await;
}

/// The agent's first sample emits `outputs` and reports usage over every threshold used
/// here, the remote compaction returns `summary`, and the next sample finishes the turn.
async fn mount_over_limit(
    server: &MockServer,
    agent: fn(&Value) -> bool,
    mut outputs: Vec<Value>,
    summary: String,
) {
    let initial = move |body: &Value| {
        agent(body) && !is_compaction(body) && !body.to_string().contains(SUMMARY)
    };
    let usage = ev_completed_with_tokens("first", /*total_tokens*/ 100_000);
    outputs.push(usage);
    mount(server, "first", initial, outputs).await;
    let compaction = move |body: &Value| agent(body) && is_compaction(body);
    let usage = ev_completed_with_tokens("compact", /*total_tokens*/ 1_000);
    mount(
        server,
        "compact",
        compaction,
        vec![compaction_output(&summary), usage],
    )
    .await;
    let resumed = move |body: &Value| {
        agent(body) && body.to_string().contains(SUMMARY) && !is_compaction(body)
    };
    let done = ev_assistant_message("done-message", "finished");
    mount(server, "done", resumed, vec![done]).await;
}

async fn wait_for_child_turn(test: &TestCodex) -> Result<()> {
    let root = test.session_configured.thread_id;
    let ids = test.thread_manager.list_thread_ids().await;
    let child = ids
        .into_iter()
        .find(|id| *id != root)
        .context("no child spawned")?;
    let child = test.thread_manager.get_thread(child).await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(())
}

async fn response_bodies(server: &MockServer, filter: impl Fn(&Value) -> bool) -> Vec<Value> {
    let requests = server.received_requests().await.unwrap_or_default();
    requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .map(json_body)
        .filter(|body| filter(body))
        .collect()
}

/// Runs a root turn that calls one collaboration tool, and returns the bodies of the
/// root requests that carry its output.
async fn root_tool_turn(
    test: &TestCodex,
    server: &MockServer,
    tool_name: &str,
) -> Result<Vec<Value>> {
    let prompt = format!("root calls {tool_name}");
    let call_id = format!("root-{tool_name}");
    let call = collaboration_call(&call_id, tool_name, json!({}));
    let (pending_prompt, pending_call) = (prompt.clone(), call_id.clone());
    let pending = move |body: &Value| {
        body.to_string().contains(&pending_prompt) && call_output(body, &pending_call).is_none()
    };
    mount(server, &call_id, pending, vec![call]).await;
    let done_id = format!("{call_id}-done");
    let done = ev_assistant_message(&done_id, "done");
    let answered = call_id.clone();
    let answered = move |body: &Value| call_output(body, &answered).is_some();
    mount(server, &done_id, answered, vec![done]).await;
    test.submit_turn(&prompt).await?;
    Ok(response_bodies(server, |body| call_output(body, &call_id).is_some()).await)
}

/// The worker's last reduction as listed to the root:
/// `(outcome, before_tokens > threshold, after_tokens < threshold)`.
async fn listed_last_reduction(
    test: &TestCodex,
    server: &MockServer,
    threshold: i64,
) -> Result<(String, bool, bool)> {
    let bodies = root_tool_turn(test, server, "list_agents").await?;
    let output = call_output(&bodies[0], "root-list_agents")
        .and_then(|item| item["output"].as_str())
        .context("list_agents output should reach the root")?;
    let listed: Value = serde_json::from_str(output)?;
    let worker = listed["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|agent| {
            agent["agent_name"]
                .as_str()
                .is_some_and(|name| name.ends_with("/worker"))
        })
        .context("list_agents should list the worker")?;
    let reduction = &worker["context"]["last_reduction"];
    Ok((
        reduction["outcome"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        reduction["before_tokens"].as_i64() > Some(threshold),
        reduction["after_tokens"].as_i64() < Some(threshold),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_shakes_mid_turn_without_compacting() -> Result<()> {
    skip_if_no_network!(Ok(()));
    const THRESHOLD: i64 = 80_000;

    let server = start_mock_server().await;
    mount_root_spawn(&server, "worker", CHILD_TASK).await;
    let sampled = |body: &Value| is_child(body) && call_output(body, "elidable").is_some();
    let first = move |body: &Value| is_child(body) && !sampled(body);
    mount(&server, "child-reads", first, file_reads()).await;
    let done = ev_assistant_message("child-done", "child finished");
    mount(&server, "child-done", sampled, vec![done]).await;

    let test = reduction_test(/*enabled*/ true, THRESHOLD as u64)
        .build_with_auto_env(&server)
        .await?;
    write_read_files(&test)?;
    test.submit_turn(ROOT_PROMPT).await?;
    wait_for_child_turn(&test).await?;

    let child = response_bodies(&server, is_child).await;
    let continuation = child
        .last()
        .context("child should continue after the tools")?;
    let output = |call_id| {
        call_output(continuation, call_id)
            .map(|item| item["output"].to_string())
            .unwrap_or_default()
    };
    assert_eq!(
        (
            child.iter().map(is_compaction).collect::<Vec<_>>(),
            output("elidable").contains("[shaken ~"),
            output("elidable").contains(ELIDABLE_MARKER),
            output("recent").contains(RECENT_MARKER),
        ),
        (vec![false, false], true, false, true),
    );
    assert_eq!(
        listed_last_reduction(&test, &server, THRESHOLD).await?,
        ("shaken".to_string(), true, true),
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_compacts_when_shake_cannot_reduce_enough() -> Result<()> {
    skip_if_no_network!(Ok(()));
    const THRESHOLD: i64 = 50_000;

    let server = start_mock_server().await;
    mount_root_spawn(&server, "worker", CHILD_TASK).await;
    // Reported usage is over the limit, but nothing in history is elidable.
    let tool_call = ev_function_call(CHILD_TOOL_CALL, "test_tool", "{}");
    mount_over_limit(&server, is_child, vec![tool_call], SUMMARY.to_string()).await;

    let test = reduction_test(/*enabled*/ true, THRESHOLD as u64)
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn(ROOT_PROMPT).await?;
    wait_for_child_turn(&test).await?;

    let child = response_bodies(&server, is_child).await;
    assert_eq!(
        child.iter().map(is_compaction).collect::<Vec<_>>(),
        vec![false, true, false]
    );
    assert_eq!(
        listed_last_reduction(&test, &server, THRESHOLD).await?,
        ("compacted".to_string(), true, true),
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_fails_when_compaction_leaves_it_over_the_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    mount_root_spawn(&server, "worker", CHILD_TASK).await;
    // The compacted history alone is far above the 50k threshold.
    let tool_call = ev_function_call(CHILD_TOOL_CALL, "test_tool", "{}");
    let summary = format!("{SUMMARY}\n").repeat(20_000);
    mount_over_limit(&server, is_child, vec![tool_call], summary).await;

    let test = reduction_test(/*enabled*/ true, /*threshold_tokens*/ 50_000)
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn(ROOT_PROMPT).await?;
    wait_for_child_turn(&test).await?;
    let parent = root_tool_turn(&test, &server, "wait_agent").await?;

    // One sample and one compaction, then the turn stops instead of resampling, and
    // the parent receives the failure as the child's final answer.
    let child = response_bodies(&server, is_child).await;
    let error = CodexErr::ContextWindowExceeded.to_string();
    assert_eq!(
        (
            child.iter().map(is_compaction).collect::<Vec<_>>(),
            parent
                .iter()
                .map(|body| {
                    let body = body.to_string();
                    (body.contains("Agent errored:"), body.contains(&error))
                })
                .collect::<Vec<_>>(),
        ),
        (vec![false, true], vec![(true, true)]),
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_compacts_without_shaking_and_keeps_sampling_when_still_over() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let is_root = |body: &Value| !is_child(body);

    let server = start_mock_server().await;
    // Usage plus the read output clears the root's 160k auto-shake threshold, so a
    // mid-turn shake would elide the older read and bring it under its 50k limit. The
    // compacted history alone is still far above that limit.
    let summary = format!("{SUMMARY}\n").repeat(20_000);
    mount_over_limit(&server, is_root, file_reads(), summary).await;

    let test = reduction_test(/*enabled*/ true, /*threshold_tokens*/ 50_000)
        .with_config(|config| config.model_auto_compact_token_limit = Some(50_000))
        .build_with_auto_env(&server)
        .await?;
    write_read_files(&test)?;
    test.submit_turn("root reads files").await?;

    // Compaction sees the unshaken read, and the root resamples after it.
    let root = response_bodies(&server, is_root).await;
    assert_eq!(
        root.iter()
            .map(|body| (
                is_compaction(body),
                body.to_string().contains(ELIDABLE_MARKER)
            ))
            .collect::<Vec<_>>(),
        vec![(false, false), (true, true), (false, false)],
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_new_context_window_request_skips_shake() -> Result<()> {
    skip_if_no_network!(Ok(()));
    const THRESHOLD: i64 = 80_000;

    let server = start_mock_server().await;
    mount_root_spawn(&server, "worker", CHILD_TASK).await;
    // The reads would let a shake bring the child under its limit, but the explicit
    // request resets the window instead.
    let mut first = file_reads();
    first.push(ev_function_call("new-window", "new_context", "{}"));
    mount(&server, "child-reads", is_child, first).await;
    let done = ev_assistant_message("child-done", "child finished");
    mount(&server, "child-done", is_child, vec![done]).await;

    let test = reduction_test(/*enabled*/ true, THRESHOLD as u64)
        .with_config(|config| {
            config
                .features
                .enable(Feature::TokenBudget)
                .expect("test config should allow token budget");
        })
        .build_with_auto_env(&server)
        .await?;
    write_read_files(&test)?;
    test.submit_turn(ROOT_PROMPT).await?;
    wait_for_child_turn(&test).await?;

    let child = response_bodies(&server, is_child).await;
    assert_eq!(
        child
            .iter()
            .map(|body| body.to_string().contains(RECENT_MARKER))
            .collect::<Vec<_>>(),
        vec![false, false],
    );
    assert_eq!(
        listed_last_reduction(&test, &server, THRESHOLD).await?,
        ("compacted".to_string(), true, true),
    );
    Ok(())
}

#[test_case(true; "enabled")]
#[test_case(false; "disabled")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_children_inherit_limits_and_root_keeps_its_own(enabled: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const ROOT_LIMIT: i64 = 500_000;
    const THRESHOLD: u64 = 100_000;
    const FIRST_TASK: &str = "FIRST_TASK spawn a grandchild";
    const SECOND_TASK: &str = "SECOND_TASK leaf work";

    let server = start_mock_server().await;
    mount_root_spawn(&server, "first", FIRST_TASK).await;
    let child_turn = |task| {
        move |body: &Value| {
            is_child(body)
                && body.to_string().contains(task)
                && call_output(body, "first-spawn").is_none()
        }
    };
    let child_spawn = spawn_call("first-spawn", "second", SECOND_TASK);
    mount(
        &server,
        "first-spawn",
        child_turn(FIRST_TASK),
        vec![child_spawn],
    )
    .await;
    let leaf_done = ev_assistant_message("second-done", "leaf finished");
    mount(
        &server,
        "second-done",
        child_turn(SECOND_TASK),
        vec![leaf_done],
    )
    .await;
    let child_done = ev_assistant_message("first-done", "spawned");
    let spawned = |body: &Value| call_output(body, "first-spawn").is_some();
    mount(&server, "first-done", spawned, vec![child_done]).await;

    let test = reduction_test(enabled, THRESHOLD)
        .with_config(|config| config.model_auto_compact_token_limit = Some(ROOT_LIMIT))
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn(ROOT_PROMPT).await?;
    let root = test.session_configured.thread_id;
    let children = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let ids = test.thread_manager.list_thread_ids().await;
            if ids.len() == 3 {
                break ids.into_iter().filter(|id| *id != root).collect::<Vec<_>>();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("root should spawn a child that spawns a grandchild")?;

    let limits = |config: Arc<codex_core::config::Config>| {
        (
            config.model_auto_compact_token_limit,
            config.auto_shake.max_threshold_tokens,
        )
    };
    let mut child_limits = Vec::new();
    for child in children {
        child_limits.push(limits(
            test.thread_manager.get_thread(child).await?.config().await,
        ));
    }
    let threshold = i64::try_from(THRESHOLD)?;
    let expected_child = if enabled {
        (Some(threshold), Some(threshold))
    } else {
        (Some(ROOT_LIMIT), None)
    };
    assert_eq!(
        (limits(test.codex.config().await), child_limits),
        ((Some(ROOT_LIMIT), None), vec![expected_child; 2]),
    );
    Ok(())
}
