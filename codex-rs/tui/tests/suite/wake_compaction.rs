//! Runs the embedded app server, agent wake loop and terminal renderer against a gated model.

use super::focus_palette::PtyCodex;
use super::focus_palette::write_test_config;
use anyhow::Result;
use anyhow::bail;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_routed_streaming_sse_server;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;

const PARENT_IDLE: &str = "The parent is idle.";
const CHILD_REPORT: &str = "The child finished its check.";
const PARENT_RESUMED: &str = "The parent resumed after compaction.";
const COMPACTING: &str = "Compacting context";

enum Route {
    Parent,
    Child,
    Title,
    Compact,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_child_wake_shows_mid_turn_compaction_in_the_tui() -> Result<()> {
    core_test_support::skip_if_sandbox!(Ok(()));
    let (release_child, child_gate) = oneshot::channel();
    let (release_compact, compact_gate) = oneshot::channel();
    let (release_resume, resume_gate) = oneshot::channel();
    let root = vec![
        stream(
            "spawn",
            responses::ev_function_call_with_namespace(
                "spawn-worker",
                "collaboration",
                "spawn_agent",
                &json!({"message": "Check one item.", "task_name": "worker"}).to_string(),
            ),
            0,
            None,
        ),
        stream(
            "root-idle",
            responses::ev_assistant_message("idle", PARENT_IDLE),
            0,
            None,
        ),
        stream(
            "wake",
            responses::ev_exec_command_call_with_args("wake-tool", &json!({"cmd": "printf ready"})),
            500_000,
            None,
        ),
        stream(
            "root-resumed",
            responses::ev_assistant_message("resumed", PARENT_RESUMED),
            50,
            Some(resume_gate),
        ),
    ];
    let child = vec![stream(
        "child-done",
        responses::ev_assistant_message("child-report", CHILD_REPORT),
        0,
        Some(child_gate),
    )];
    let title = vec![stream(
        "title",
        responses::ev_assistant_message("title-message", r#"{"title":"Wake compaction"}"#),
        0,
        None,
    )];
    let compact = vec![stream(
        "compact",
        json!({
            "type": "response.output_item.done",
            "item": {"type": "compaction", "encrypted_content": "COMPACTED"}
        }),
        50,
        Some(compact_gate),
    )];
    let (server, _completions) =
        start_routed_streaming_sse_server(vec![root, child, title, compact], |_, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let metadata: Value =
                serde_json::from_str(body["client_metadata"]["x-codex-turn-metadata"].as_str()?)
                    .ok()?;
            let route = match metadata["thread_source"].as_str()? {
                "subagent" => Route::Child,
                "thread_title" => Route::Title,
                "user"
                    if body["input"]
                        .as_array()?
                        .iter()
                        .any(|item| item["type"] == "compaction_trigger") =>
                {
                    Route::Compact
                }
                "user" => Route::Parent,
                _ => return None,
            };
            Some(route as usize)
        })
        .await;

    let workspace = tempfile::tempdir()?;
    let cwd = workspace.path().canonicalize()?;
    let home = tempfile::tempdir()?;
    write_test_config(home.path(), &cwd)?;
    let config_path = home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    std::fs::write(
        config_path,
        format!(
            "model_auto_compact_token_limit = 100000\n\
             approval_policy = \"never\"\n\
             sandbox_mode = \"read-only\"\n\
             features.collab = true\n\
             features.multi_agent_v2 = true\n\
             features.code_mode = false\n\
             features.async_compaction = false\n\
             multi_agent_v2.agent_polling = \"disabled\"\n\
             {config}\n\
             [model_providers.fixture]\n\
             name = \"Compaction fixture\"\n\
             base_url = \"{}/v1\"\n\
             wire_api = \"responses\"\n\
             supports_websockets = false\n\
             request_max_retries = 0\n\
             stream_max_retries = 0\n\
             capabilities.remote_compaction = \"v2\"\n",
            server.uri(),
        ),
    )?;
    let mut terminal = PtyCodex::start(
        &cwd,
        home,
        &[
            "--no-daemon",
            "--no-alt-screen",
            "-c",
            "model_provider=\"fixture\"",
            "-c",
            "model=\"gpt-5.4\"",
            "Delegate one check and then go idle.",
        ],
    )?;
    terminal.wait_for_startup()?;
    wait_for_screen(
        &mut terminal,
        "idle parent before child completion",
        |screen| screen.contains(PARENT_IDLE) && !screen.contains("esc to interrupt"),
    )?;
    release_child.send(()).expect("release direct child");
    terminal.wait_for_screen(COMPACTING)?;
    wait_for_screen(&mut terminal, "compaction timer advancing", |screen| {
        screen
            .lines()
            .any(|line| line.contains("Compacting context (") && !line.contains("(0s"))
    })?;
    release_compact.send(()).expect("release compaction");
    wait_for_screen(&mut terminal, "working state restored", |screen| {
        screen.contains("Context compacted")
            && screen.contains("Working")
            && !screen.contains(COMPACTING)
    })?;
    release_resume
        .send(())
        .expect("release continued parent turn");
    terminal.wait_for_screen(PARENT_RESUMED)?;

    let requests = server.requests().await;
    let requests: Vec<Value> = requests
        .iter()
        .map(|body| serde_json::from_slice(body))
        .collect::<serde_json::Result<_>>()?;
    let wake = requests
        .iter()
        .find(|request| {
            request["input"].as_array().is_some_and(|input| {
                input.iter().any(|item| {
                    item["type"] == "agent_message" && item.to_string().contains(CHILD_REPORT)
                })
            })
        })
        .expect("child report starts a wake request");
    let compact = requests
        .iter()
        .find(|request| {
            request["input"].as_array().is_some_and(|input| {
                input
                    .iter()
                    .any(|item| item["type"] == "compaction_trigger")
            })
        })
        .expect("threshold starts remote compaction");
    pretty_assertions::assert_eq!(
        wake["client_metadata"]["turn_id"],
        compact["client_metadata"]["turn_id"],
    );
    assert!(
        compact["input"].as_array().unwrap().iter().any(|item| {
            item["type"] == "function_call_output" && item["call_id"] == "wake-tool"
        })
    );
    server.shutdown().await;
    Ok(())
}

fn stream(
    id: &str,
    item: Value,
    tokens: i64,
    gate: Option<oneshot::Receiver<()>>,
) -> Vec<StreamingSseChunk> {
    vec![StreamingSseChunk {
        gate,
        body: responses::sse(vec![
            responses::ev_response_created(id),
            item,
            responses::ev_completed_with_tokens(id, tokens),
        ]),
    }]
}

fn wait_for_screen(
    terminal: &mut PtyCodex,
    description: &str,
    ready: impl Fn(&str) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        terminal.read_output(Duration::from_millis(50))?;
        if ready(&terminal.screen_contents()) {
            return Ok(());
        }
        terminal.ensure_running()?;
    }
    bail!(
        "missing {description}; screen:\n{}",
        terminal.screen_contents()
    )
}
