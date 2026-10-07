//! Integration coverage for `[subagent_context_reduction]`: spawned children run the
//! shake-then-compact roll-over at a lowered limit, and fail instead of looping when
//! compaction cannot get them back under it. The root agent keeps its prior roll-over.

use anyhow::Context;
use anyhow::Result;
use codex_core::config::SubagentContextReductionConfig;
use codex_features::Feature;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
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
    Vec::from([
        file_read("elidable", "older.txt"),
        file_read("recent", "recent.txt"),
    ])
}

fn file_read(call_id: &str, path: &str) -> Value {
    let args = json!({"cmd": format!("{READ_COMMAND} {path}"), "max_output_tokens": 100_000});
    ev_exec_command_call_with_args(call_id, &args)
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
            model.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
        })
        .with_config(move |config| {
            for feature in [Feature::Collab, Feature::MultiAgentV2] {
                config
                    .features
                    .enable(feature)
                    .expect("test config should allow collaboration features");
            }
            config.tool_output_token_limit = Some(100_000);
            // Keep the root idle until the fixture explicitly starts its next turn.
            config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Enabled;
            config.multi_agent_v2.exec_root_wakes_on_report = false;
            // `wait_agent` waits for new mailbox activity; a child that already finished
            // may have delivered its completion before the call.
            config.multi_agent_v2.default_wait_timeout_ms = 1_000;
            config.multi_agent_v2.noninteractive_default_wait_timeout_ms = 1_000;
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
    let body = bodies
        .first()
        .context("root should sample list_agents output")?;
    let output = call_output(body, "root-list_agents")
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
        reduction["after_tokens"]
            .as_i64()
            .is_some_and(|tokens| tokens < threshold),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_shakes_mid_turn_without_compacting() -> Result<()> {
    skip_if_no_network!(Ok(()));
    const THRESHOLD: i64 = 80_000;

    let server = start_mock_server().await;
    mount_root_spawn(&server, "worker", CHILD_TASK).await;
    let has_elidable_output = |body: &Value| call_output(body, "elidable").is_some();
    let has_recent_output = |body: &Value| call_output(body, "recent").is_some();
    let first = move |body: &Value| is_child(body) && !has_elidable_output(body);
    mount(
        &server,
        "child-read-elidable",
        first,
        vec![file_read("elidable", "older.txt")],
    )
    .await;
    let second =
        move |body: &Value| is_child(body) && has_elidable_output(body) && !has_recent_output(body);
    mount(
        &server,
        "child-read-recent",
        second,
        vec![
            file_read("recent", "recent.txt"),
            ev_completed_with_tokens("child-read-recent", 100_000),
        ],
    )
    .await;
    let sampled =
        move |body: &Value| is_child(body) && has_elidable_output(body) && has_recent_output(body);
    let done = ev_assistant_message("child-done", "child finished");
    mount(&server, "child-done", sampled, vec![done]).await;

    let test = reduction_test(/*enabled*/ true, THRESHOLD as u64)
        .build_with_auto_env(&server)
        .await?;
    write_read_files(&test)?;
    std::fs::write(
        test.workspace_path("older.txt"),
        format!("{ELIDABLE_MARKER}\n").repeat(/*n*/ 4_000),
    )?;
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
        (vec![false, false, false], true, false, true),
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
    let error = "This subagent's turn ended because its context was still over its 50000-token context limit after compaction. Use `followup_task` to ask it for a brief report of its partial results";
    assert_eq!(
        (
            child.iter().map(is_compaction).collect::<Vec<_>>(),
            parent
                .iter()
                .map(|body| {
                    let body = body.to_string();
                    (body.contains("Agent errored:"), body.contains(error))
                })
                .collect::<Vec<_>>(),
        ),
        (vec![false, true], vec![(true, true)]),
    );
    // The child's role text states the cap it just exceeded.
    assert!(
        child[0]
            .to_string()
            .contains("Your context budget is 50000 tokens")
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

#[test_case(codex_protocol::config_types::AutoCompactTokenLimitScope::Total; "total")]
#[test_case(codex_protocol::config_types::AutoCompactTokenLimitScope::BodyAfterPrefix; "body_after_prefix")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_task_over_child_cap_is_recorded_without_ordinary_sampling(
    scope: codex_protocol::config_types::AutoCompactTokenLimitScope,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let task = format!(
        "{CHILD_TASK}\n{} ",
        "preserve the concrete task constraint\n".repeat(/*n*/ 20_000)
    );
    mount_root_spawn(&server, "worker", &task).await;
    mount(
        &server,
        "child-compact",
        |body| is_child(body) && is_compaction(body),
        vec![compaction_output(
            &format!("{SUMMARY}\n").repeat(/*n*/ 20_000),
        )],
    )
    .await;
    let test = reduction_test(/*enabled*/ true, /*threshold_tokens*/ 50_000)
        .with_model_info_override(MODEL, |model| {
            // The root must have room to inspect the deliberately oversized child task.
            model.context_window = Some(2_000_000);
            model.max_context_window = Some(2_000_000);
        })
        .with_config(move |config| config.model_auto_compact_token_limit_scope = scope)
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn(ROOT_PROMPT).await?;
    wait_for_child_turn(&test).await?;
    let child = response_bodies(&server, is_child).await;
    // The giant initial input was absent at the old pre-turn check. The maintenance
    // request contains it; an insufficient rebuilt request cannot reach sampling.
    assert_eq!(
        child.iter().map(is_compaction).collect::<Vec<_>>(),
        vec![true]
    );
    assert!(
        child[0]
            .to_string()
            .contains("preserve the concrete task constraint")
    );
    assert_eq!(
        listed_last_reduction(&test, &server, 50_000).await?.0,
        "insufficient"
    );
    Ok(())
}

#[test_case(false; "native_tools")]
#[test_case(true; "code_mode")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_output_crossing_cap_compacts_before_the_next_sample(code_mode: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    const CONSTRAINT: &str = "Keep the task constraint; remaining work is to validate the result.";
    mount_root_spawn(&server, "worker", CONSTRAINT).await;
    mount(
        &server, "child-read", |body| is_child(body) && !is_compaction(body)
            && call_output(body, "recent").is_none() && !body.to_string().contains(SUMMARY),
        vec![if code_mode {
            core_test_support::responses::ev_custom_tool_call("recent", "exec", &format!(
                "// @exec: {{\"max_output_tokens\": 100000}}\ntext(await tools.exec_command({{cmd: \"{READ_COMMAND} recent.txt\", max_output_tokens: 100000}}));"
            ))
        } else { file_read("recent", "recent.txt") }],
    ).await;
    mount(
        &server,
        "child-compact",
        |body| is_child(body) && is_compaction(body),
        vec![compaction_output(SUMMARY)],
    )
    .await;
    mount(
        &server,
        "child-done",
        |body| is_child(body) && !is_compaction(body) && body.to_string().contains(SUMMARY),
        vec![ev_assistant_message("child-done", "finished")],
    )
    .await;
    let test = reduction_test(/*enabled*/ true, /*threshold_tokens*/ 50_000)
        .with_model_info_override(MODEL, move |model| {
            model.truncation_policy = TruncationPolicyConfig::tokens(/*limit*/ 96_000);
            if code_mode {
                model.tool_mode = Some(codex_protocol::openai_models::ToolMode::CodeMode);
            }
        })
        .with_config(move |config| {
            if code_mode {
                assert!(config.features.enable(Feature::CodeModeHost).is_ok());
                assert!(config.features.enable(Feature::CodeMode).is_ok());
                assert!(
                    config
                        .features
                        .enable(Feature::ExecutedToolCallMetadata)
                        .is_ok()
                );
            }
        })
        .build_with_auto_env(&server)
        .await?;
    std::fs::write(
        test.workspace_path("recent.txt"),
        format!("{RECENT_MARKER}\n").repeat(/*n*/ 12_000),
    )?;
    test.submit_turn(ROOT_PROMPT).await?;
    wait_for_child_turn(&test).await?;
    let child = response_bodies(&server, is_child).await;
    let custom_outputs = child
        .iter()
        .flat_map(|body| body["input"].as_array().into_iter().flatten())
        .filter(|item| item["type"] == "custom_tool_call_output")
        .map(|item| {
            item["output"]
                .to_string()
                .chars()
                .take(256)
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        child.iter().map(is_compaction).collect::<Vec<_>>(),
        vec![false, true, false],
        "custom outputs: {custom_outputs:?}"
    );
    assert!(child[1].to_string().contains(RECENT_MARKER));
    assert!(child[2].to_string().contains(CONSTRAINT));
    assert_eq!(
        listed_last_reduction(&test, &server, 50_000).await?.0,
        "compacted"
    );
    Ok(())
}

#[test_case(true, AutoCompactTokenLimitScope::Total; "independent_child_cap")]
#[test_case(true, AutoCompactTokenLimitScope::BodyAfterPrefix; "body_scope_does_not_replace_active_cap")]
#[test_case(false, AutoCompactTokenLimitScope::Total; "disabled_child_policy_keeps_smaller_model_window")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_only_tools_cross_the_independent_child_cap(
    enabled: bool,
    scope: AutoCompactTokenLimitScope,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount(
        &server,
        "compact",
        is_compaction,
        vec![compaction_output(SUMMARY)],
    )
    .await;
    let test = reduction_test(enabled, /*threshold_tokens*/ 1_000)
        .with_session_source(codex_protocol::protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::Other("admission-test".to_string()),
        ))
        .with_config(move |config| {
            config.base_instructions = Some("Be concise.".to_string());
            config.model_auto_compact_token_limit_scope = scope;
            assert!(config.features.disable(Feature::Collab).is_ok());
            assert!(config.features.disable(Feature::MultiAgentV2).is_ok());
            if !enabled {
                config.model_context_window = Some(1_000);
            }
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Keep the task constraint and validate the remaining work.")
        .await?;
    let requests = response_bodies(&server, |_| true).await;
    assert_eq!(
        requests.iter().map(is_compaction).collect::<Vec<_>>(),
        vec![true]
    );
    // The recorded message content fits; the tool schemas in the final request don't.
    let snapshot = test.codex.conversation_history_snapshot().await;
    let content_bytes: usize = snapshot
        .items()
        .map(|item| match item {
            codex_protocol::models::ResponseItem::Message { content, .. } => content
                .iter()
                .map(|part| match part {
                    codex_protocol::models::ContentItem::InputText { text }
                    | codex_protocol::models::ContentItem::OutputText { text } => text.len(),
                    _ => 0,
                })
                .sum(),
            _ => 0,
        })
        .sum();
    assert!(
        content_bytes < 4_000,
        "recorded content must fit the 1000-token cap: {content_bytes}"
    );
    // A new turn carrying only a warning has unchanged reducible context.
    test.codex
        .start_turn_if_idle(codex_protocol::turn_input::TurnInputRequest::new(
            codex_protocol::turn_input::TurnInput::ResponseItem(
                codex_protocol::models::ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![codex_protocol::models::ContentItem::InputText {
                        text: "A warning without new eligible context".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            ),
        ))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(response_bodies(&server, |_| true).await.len(), 1);
    mount(
        &server,
        "compact-new-input",
        is_compaction,
        vec![compaction_output(SUMMARY)],
    )
    .await;
    test.submit_turn("New eligible task instructions").await?;
    assert_eq!(response_bodies(&server, |_| true).await.len(), 2);
    Ok(())
}

#[test_case(false; "failed")]
#[test_case(true; "cancelled")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_compaction_failure_reports_unknown_post_size(cancel: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_root_spawn(&server, "worker", CHILD_TASK).await;
    mount(
        &server,
        "child-over-limit",
        |body| is_child(body) && !is_compaction(body),
        vec![
            ev_function_call(CHILD_TOOL_CALL, "test_tool", "{}"),
            ev_completed_with_tokens("child-over-limit", 100_000),
        ],
    )
    .await;
    let started = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&started);
    wiremock::Mock::given(|request: &wiremock::Request| is_compaction(&json_body(request)))
        .respond_with(move |_: &wiremock::Request| {
            notify.notify_one();
            let response =
                wiremock::ResponseTemplate::new(/*s*/ 500).set_body_string("compaction failed");
            if cancel {
                response.set_delay(Duration::from_secs(/*secs*/ 30))
            } else {
                response
            }
        })
        .up_to_n_times(/*n*/ 1)
        .mount(&server)
        .await;
    let test = reduction_test(/*enabled*/ true, /*threshold_tokens*/ 50_000)
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn(ROOT_PROMPT).await?;
    if cancel {
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), started.notified()).await?;
        let root = test.session_configured.thread_id;
        let id = test
            .thread_manager
            .list_thread_ids()
            .await
            .into_iter()
            .find(|id| *id != root)
            .context("child should be compacting")?;
        let child = test.thread_manager.get_thread(id).await?;
        child
            .submit(codex_protocol::protocol::Op::Interrupt)
            .await?;
        wait_for_event(&child, |event| matches!(event, EventMsg::TurnAborted(_))).await;
    } else {
        wait_for_child_turn(&test).await?;
    }
    assert_eq!(
        response_bodies(&server, is_child)
            .await
            .iter()
            .map(is_compaction)
            .collect::<Vec<_>>(),
        vec![false, true]
    );
    // Nullable post-size must not compare as a measured size below the cap.
    let outcome = if cancel { "cancelled" } else { "failed" };
    assert_eq!(
        listed_last_reduction(&test, &server, 50_000).await?,
        (outcome.to_string(), true, false)
    );
    Ok(())
}

#[test_case(false; "persistent")]
#[test_case(true; "ephemeral")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn captured_child_selection_can_raise_disable_and_restore_without_losing_baseline(
    ephemeral: bool,
) -> Result<()> {
    use codex_protocol::context_settings::ContextSettingsOverrides;
    use codex_protocol::context_settings::ContextSettingsUpdate;
    use codex_protocol::context_settings::ShakeThreshold;
    use codex_protocol::protocol::ThreadSettingsOverrides;
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_root_spawn(&server, "worker", CHILD_TASK).await;
    mount(
        &server,
        "child-done",
        is_child,
        vec![ev_assistant_message("child-done", "finished")],
    )
    .await;
    let test = reduction_test(/*enabled*/ true, /*threshold_tokens*/ 30000)
        .with_config(move |config| {
            config.ephemeral = ephemeral;
            config.model_auto_compact_token_limit = Some(90000);
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .update_thread_settings(ThreadSettingsOverrides {
            context_settings: Some(ContextSettingsUpdate::Patch {
                overrides: ContextSettingsOverrides {
                    shake_threshold: Some(ShakeThreshold::Percent { percent: 37 }),
                    ..Default::default()
                },
            }),
            ..Default::default()
        })
        .await?;
    test.submit_turn(ROOT_PROMPT).await?;
    wait_for_child_turn(&test).await?;
    let child_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|id| *id != test.session_configured.thread_id)
        .context("child")?;
    let child = test.thread_manager.get_thread(child_id).await?;
    let before = child.context_settings().await;
    assert_eq!(
        (
            before.effective.shake_threshold,
            before.compaction_scope_token_limit
        ),
        (ShakeThreshold::Percent { percent: 37 }, Some(30000))
    );
    let initial = child.restorable_thread_settings().await;
    child
        .update_thread_settings(ThreadSettingsOverrides {
            context_settings: Some(ContextSettingsUpdate::Patch {
                overrides: ContextSettingsOverrides {
                    child_reduction_threshold_tokens: Some(70000),
                    ..Default::default()
                },
            }),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        child.context_settings().await.compaction_scope_token_limit,
        Some(70000)
    );
    let raised_snapshot = child.thread_settings_snapshot().await;
    child
        .update_thread_settings(ThreadSettingsOverrides {
            context_settings: Some(ContextSettingsUpdate::Patch {
                overrides: ContextSettingsOverrides {
                    child_reduction_enabled: Some(false),
                    ..Default::default()
                },
            }),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        child.context_settings().await.compaction_scope_token_limit,
        Some(90000)
    );
    let disabled = child.restorable_thread_settings().await;
    let disabled_view = child.context_settings().await;
    let mut fork_config = test.config.clone();
    fork_config.context_settings = child.thread_settings_snapshot().await.context_settings;
    let earlier = codex_history::InitialHistory::Resumed(codex_history::ResumedHistory {
        conversation_id: child_id,
        history: Arc::new(vec![codex_history::RolloutItem::EventMsg(
            EventMsg::ThreadSettingsApplied(codex_protocol::protocol::ThreadSettingsAppliedEvent {
                thread_id: Some(child_id),
                thread_settings: raised_snapshot,
            }),
        )]),
        rollout_path: None,
        last_activity_at: None,
    });
    let fork = test
        .thread_manager
        .fork_thread_from_history(
            codex_core::ForkSnapshot::Interrupted,
            codex_core::StartThreadOptions::new(fork_config),
            earlier,
        )
        .await?;
    let root_fork = fork.thread.context_settings().await;
    assert_eq!(
        (
            root_fork.compaction_scope_token_limit,
            root_fork.effective.child_reduction_threshold_tokens,
            root_fork.effective.child_reduction_enabled,
            root_fork.inherited_child_active_cap,
        ),
        (Some(90000), 70000, true, None)
    );
    fork.thread.shutdown_and_wait().await?;

    child.restore_thread_settings(initial).await?;
    assert_eq!(child.context_settings().await, before);
    child.checkpoint_thread_settings().await?;
    if !ephemeral {
        child.restore_thread_settings(disabled).await?;
        child.checkpoint_thread_settings().await?;
        let rollout_path = child.rollout_path();
        child.shutdown_and_wait().await?;
        let history = codex_rollout::RolloutRecorder::get_rollout_history(
            rollout_path.as_ref().context("persistent child rollout")?,
        )
        .await?;
        // Reload config can be built from a parent whose defaults changed after the spawn.
        let mut reload = test.config.clone();
        reload.subagent_context_reduction.threshold_tokens = 5000;
        reload.model_auto_compact_token_limit = Some(1000);
        let resumed = test
            .thread_manager
            .resume_thread_with_history(
                reload,
                history,
                test.thread_manager.auth_manager(),
                /*parent_trace*/ None,
                codex_protocol::mcp::ClientMcpExtensions::default(),
            )
            .await?;
        assert_eq!(resumed.thread.context_settings().await, disabled_view);
        resumed.thread.shutdown_and_wait().await?;
    }
    Ok(())
}

#[test_case(true; "off_to_percent")]
#[test_case(false; "percent_to_off")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_child_global_shake_threshold_survives_reload_and_reset(off: bool) -> Result<()> {
    use codex_config::config_toml::AutoShakeThresholdToml;
    use codex_core::TurnInputRequest;
    use codex_protocol::context_settings::ContextSettingsOverrides;
    use codex_protocol::context_settings::ContextSettingsUpdate;
    use codex_protocol::context_settings::ShakeThreshold;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::ThreadSettingsOverrides;
    use codex_protocol::user_input::UserInput;
    use core_test_support::responses::mount_sse_once;
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_root_spawn(&server, "worker", CHILD_TASK).await;
    mount(
        &server,
        "child-done",
        is_child,
        vec![ev_assistant_message("child-done", "finished")],
    )
    .await;
    let original = if off {
        AutoShakeThresholdToml::Off
    } else {
        AutoShakeThresholdToml::Percent(1)
    };
    let expected = if off {
        ShakeThreshold::Off
    } else {
        ShakeThreshold::Percent { percent: 1 }
    };
    let test = reduction_test(/*enabled*/ false, /*threshold_tokens*/ 30000)
        .with_config(move |config| {
            config.model_context_window = Some(100000);
            config.model_auto_compact_token_limit = Some(900000);
            config.auto_shake.threshold = Some(original);
            config.auto_shake.models.insert(
                "gpt-5.6".into(),
                codex_core::config::AutoShakeModelConfig {
                    threshold: Some(AutoShakeThresholdToml::Inherit),
                    min_elidable_percent: None,
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn(ROOT_PROMPT).await?;
    wait_for_child_turn(&test).await?;
    let child_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|id| *id != test.session_configured.thread_id)
        .context("child")?;
    let child = test.thread_manager.get_thread(child_id).await?;
    child.checkpoint_thread_settings().await?;
    let path = child.rollout_path().context("persistent child rollout")?;
    child.shutdown_and_wait().await?;
    let history = codex_rollout::RolloutRecorder::get_rollout_history(&path).await?;
    let mut reload = test.config.clone();
    reload.auto_shake.threshold = Some(if off {
        AutoShakeThresholdToml::Percent(1)
    } else {
        AutoShakeThresholdToml::Off
    });
    let resumed = test
        .thread_manager
        .resume_thread_with_history(
            reload,
            history,
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            codex_protocol::mcp::ClientMcpExtensions::default(),
        )
        .await?;
    let thread = resumed.thread;
    let view = thread.context_settings().await;
    assert_eq!(
        (
            view.requested.shake_threshold,
            view.effective.shake_threshold
        ),
        (None, expected)
    );
    thread
        .update_thread_settings(ThreadSettingsOverrides {
            context_settings: Some(ContextSettingsUpdate::Patch {
                overrides: ContextSettingsOverrides {
                    shake_threshold: Some(ShakeThreshold::Tokens { tokens: 99999 }),
                    ..Default::default()
                },
            }),
            ..Default::default()
        })
        .await?;
    thread
        .update_thread_settings(ThreadSettingsOverrides {
            context_settings: Some(ContextSettingsUpdate::Reset),
            ..Default::default()
        })
        .await?;
    assert_eq!(thread.context_settings().await, view);
    const MARKER: &str = "BASELINE_SHAKE_MARKER";
    let items: Vec<ResponseItem> = serde_json::from_value(json!([
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "read the output"}]},
        {"type": "function_call", "id": "baseline-call", "call_id": "baseline-call", "name": "exec_command", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "baseline-call", "output": format!("{MARKER}\n").repeat(/*n*/ 8000)},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "tail context ".repeat(/*n*/ 6000)}]}
    ]))?;
    thread.inject_response_items(items).await?;
    let request = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("next", "done"),
            ev_completed("next"),
        ]),
    )
    .await;
    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "continue".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(
        request
            .single_request()
            .body_json()
            .to_string()
            .contains(MARKER),
        off
    );
    thread.shutdown_and_wait().await?;
    Ok(())
}
