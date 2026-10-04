#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The default exec drain: exec follows every root turn until the agent tree is quiescent,
//! instead of exiting after the first one. `features.multi_agent_v2.agent_polling = "enabled"`
//! opts out.
//!
//! The child's answer is gated on exec reporting `turn.completed` for root turn 1, so the report
//! always wakes a new root turn instead of landing inside turn 1.

use std::io::BufRead;
use std::io::BufReader;
use std::process::Child;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_routed_streaming_sse_server;
use core_test_support::test_codex_exec::TestCodexExecBuilder;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::sync::oneshot;

const DRAIN_OPT_OUT: &str = "features.multi_agent_v2.agent_polling=\"enabled\"";
const NAMESPACE: &str = "collaboration";
const SPAWN_CALL_ID: &str = "drain-spawn-call";
const ROOT_PROMPT: &str = "delegate the drain worker task";
const TURN_ONE: &str = "turn one: worker is running";
const TURN_TWO: &str = "turn two: integrated the worker result";
const EXEC_TIMEOUT: Duration = Duration::from_secs(60);

const ROOT_ROUTE: usize = 0;
const CHILD_ROUTE: usize = 1;

fn response(events: Vec<Value>) -> Vec<StreamingSseChunk> {
    vec![StreamingSseChunk {
        gate: None,
        body: sse(events),
    }]
}

/// Root turn 1: spawn a worker, then end with `TURN_ONE`.
fn spawn_turn() -> Vec<Vec<StreamingSseChunk>> {
    let spawn_args = json!({"message": "do the drain work", "task_name": "worker"}).to_string();
    vec![
        response(vec![
            ev_response_created("root-spawn"),
            ev_function_call_with_namespace(SPAWN_CALL_ID, NAMESPACE, "spawn_agent", &spawn_args),
            ev_completed("root-spawn"),
        ]),
        response(vec![
            ev_response_created("root-status"),
            ev_assistant_message("root-status-msg", TURN_ONE),
            ev_completed("root-status"),
        ]),
    ]
}

/// The worker answers once `gate` fires; it never answers if the sender is held.
fn child_answer(gate: oneshot::Receiver<()>) -> Vec<StreamingSseChunk> {
    vec![StreamingSseChunk {
        gate: Some(gate),
        body: sse(vec![
            ev_response_created("child"),
            ev_assistant_message("child-msg", "worker result"),
            ev_completed("child"),
        ]),
    }]
}

fn failed_response(id: &str) -> Vec<StreamingSseChunk> {
    response(vec![json!({
        "type": "response.failed",
        "response": {
            "id": id,
            "error": {"code": "insufficient_quota", "message": "synthetic failure"}
        }
    })])
}

/// Serves the root's responses in order on one route and the worker's on another. The root
/// thread is learned from the first request, which carries the exec prompt.
async fn start_server(
    root_responses: Vec<Vec<StreamingSseChunk>>,
    child_responses: Vec<Vec<StreamingSseChunk>>,
) -> StreamingSseServer {
    let root = Arc::new(Mutex::new(None::<String>));
    let (server, _completions) = start_routed_streaming_sse_server(
        vec![root_responses, child_responses],
        move |_headers, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let thread_id = body["client_metadata"]["thread_id"].as_str()?.to_string();
            let mut root = root
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if root.is_none() && body.to_string().contains(ROOT_PROMPT) {
                *root = Some(thread_id.clone());
            }
            Some(if root.as_deref() == Some(thread_id.as_str()) {
                ROOT_ROUTE
            } else {
                CHILD_ROUTE
            })
        },
    )
    .await;
    server
}

/// A drained `codex exec --json` run whose stdout is read line by line.
struct RunningExec {
    child: Child,
    lines: mpsc::Receiver<String>,
    stdout: Vec<String>,
    stderr: JoinHandle<String>,
}

impl RunningExec {
    fn spawn(test: &TestCodexExecBuilder, server: &StreamingSseServer, extra: &[&str]) -> Self {
        Self::spawn_with_env(test, server, extra, &[])
    }

    fn spawn_with_env(
        test: &TestCodexExecBuilder,
        server: &StreamingSseServer,
        extra: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        let base = format!("{}/v1", server.uri());
        let mut child = std::process::Command::new(
            codex_utils_cargo_bin::cargo_bin("codex-exec").expect("codex-exec binary"),
        )
        .current_dir(test.cwd_path())
        .env("CODEX_HOME", test.home_path())
        .env("CODEX_SQLITE_HOME", test.home_path())
        .env(codex_login::CODEX_API_KEY_ENV_VAR, "dummy")
        .envs(env.iter().copied())
        .arg("-c")
        .arg(format!(
            "openai_base_url={}",
            serde_json::to_string(&base).expect("base url")
        ))
        .arg("--skip-git-repo-check")
        .arg("--json")
        .args(extra)
        .arg(ROOT_PROMPT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codex-exec");

        let (line_tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().expect("piped stdout");
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr_pipe), &mut text);
            text
        });
        Self {
            child,
            lines,
            stdout: Vec::new(),
            stderr,
        }
    }

    /// Reads stdout until the first `turn.completed` event.
    fn wait_for_turn_completed(&mut self) {
        loop {
            let line = self
                .lines
                .recv_timeout(EXEC_TIMEOUT)
                .expect("exec should finish root turn 1");
            let done = serde_json::from_str::<Value>(&line)
                .is_ok_and(|event| event["type"] == "turn.completed");
            self.stdout.push(line);
            if done {
                return;
            }
        }
    }

    fn pid(&self) -> libc::pid_t {
        libc::pid_t::try_from(self.child.id()).expect("pid")
    }

    /// Waits for exit and returns the status, every stdout event, and stderr.
    fn finish(mut self, timeout: Duration) -> (ExitStatus, Vec<Value>, String) {
        let pid = self.pid();
        let (status_tx, status_rx) = mpsc::channel();
        let mut child = self.child;
        std::thread::spawn(move || {
            let _ = status_tx.send(child.wait());
        });
        let Ok(status) = status_rx.recv_timeout(timeout) else {
            // SAFETY: `pid` is our child, which has not been reaped because it is still running.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            panic!("exec should exit within {timeout:?}");
        };
        let stderr = self.stderr.join().expect("stderr reader");
        self.stdout.extend(self.lines.iter());
        let events = self
            .stdout
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        (status.expect("wait for codex-exec"), events, stderr)
    }
}

fn count_type(events: &[Value], event_type: &str) -> usize {
    events
        .iter()
        .filter(|event| event["type"] == event_type)
        .count()
}

/// With the drain on, exec waits for the child to wake the root and reports the wake turn's
/// answer; JSONL reports one `turn.started`/`turn.completed` pair per root turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drained_exec_reports_the_wake_turn_answer() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let (release_child, child_gate) = oneshot::channel();
    let mut root_responses = spawn_turn();
    root_responses.push(response(vec![
        ev_response_created("root-wake"),
        ev_assistant_message("root-wake-msg", TURN_TWO),
        ev_completed("root-wake"),
    ]));
    let server = start_server(root_responses, vec![child_answer(child_gate)]).await;

    let test = test_codex_exec();
    let last_message = test.cwd_path().join("last-message.txt");
    let last_message_arg = last_message.to_str().expect("utf-8 path");
    let mut exec = RunningExec::spawn(&test, &server, &["--output-last-message", last_message_arg]);
    exec.wait_for_turn_completed();
    release_child.send(()).expect("child request is waiting");
    let (status, events, stderr) = exec.finish(EXEC_TIMEOUT);

    assert!(status.success(), "{stderr}");
    assert_eq!(
        (
            count_type(&events, "turn.started"),
            count_type(&events, "turn.completed")
        ),
        (2, 2),
        "{events:#?}"
    );
    let messages: Vec<&str> = events
        .iter()
        .filter(|event| {
            event["type"] == "item.completed" && event["item"]["type"] == "agent_message"
        })
        .filter_map(|event| event["item"]["text"].as_str())
        .collect();
    assert_eq!(messages, vec![TURN_ONE, TURN_TWO]);
    assert_eq!(std::fs::read_to_string(&last_message)?, TURN_TWO);
    server.shutdown().await;
    Ok(())
}

/// A failed wake turn fails the run (exit 1), but the completed first turn's answer is still the
/// final message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_wake_turn_exits_non_zero_with_the_completed_answer() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let (release_child, child_gate) = oneshot::channel();
    let mut root_responses = spawn_turn();
    root_responses.push(failed_response("root-wake"));
    let server = start_server(root_responses, vec![child_answer(child_gate)]).await;

    let test = test_codex_exec();
    let last_message = test.cwd_path().join("last-message.txt");
    let last_message_arg = last_message.to_str().expect("utf-8 path");
    let mut exec = RunningExec::spawn(&test, &server, &["--output-last-message", last_message_arg]);
    exec.wait_for_turn_completed();
    release_child.send(()).expect("child request is waiting");
    let (status, events, stderr) = exec.finish(EXEC_TIMEOUT);

    assert_eq!(status.code(), Some(1), "{stderr}");
    assert_eq!(count_type(&events, "turn.failed"), 1, "{events:#?}");
    assert_eq!(std::fs::read_to_string(&last_message)?, TURN_ONE);
    server.shutdown().await;
    Ok(())
}

/// A failed root turn detaches its still-running child, whose report can never reach the root,
/// so exec exits 1 without waiting for that child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_root_turn_does_not_wait_for_detached_children() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    // Holding the sender keeps the child running for the whole test.
    let (_hold_child, child_gate) = oneshot::channel();
    let mut root_responses = spawn_turn();
    root_responses[1] = failed_response("root-status");
    let server = start_server(root_responses, vec![child_answer(child_gate)]).await;

    let test = test_codex_exec();
    let exec = RunningExec::spawn(&test, &server, &[]);
    let (status, events, stderr) = exec.finish(Duration::from_secs(30));

    assert_eq!(status.code(), Some(1), "{stderr}");
    assert_eq!(count_type(&events, "turn.failed"), 1, "{events:#?}");
    server.shutdown().await;
    Ok(())
}

/// Ctrl-C while the root is idle and a child still works stops the drain: exec tears down
/// without waiting for the child, exits 1, and keeps the completed turn's answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_ctrl_c_stops_the_drain() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    // Holding the sender keeps the child running for the whole test.
    let (_hold_child, child_gate) = oneshot::channel();
    let server = start_server(spawn_turn(), vec![child_answer(child_gate)]).await;

    let test = test_codex_exec();
    let last_message = test.cwd_path().join("last-message.txt");
    let last_message_arg = last_message.to_str().expect("utf-8 path");
    let mut exec = RunningExec::spawn(&test, &server, &["--output-last-message", last_message_arg]);
    exec.wait_for_turn_completed();
    assert!(
        exec.stdout.iter().any(|line| line.contains(TURN_ONE)),
        "{:#?}",
        exec.stdout
    );

    // SAFETY: `pid` is our live child process, and SIGINT is what a terminal Ctrl-C sends.
    assert_eq!(unsafe { libc::kill(exec.pid(), libc::SIGINT) }, 0);
    let (status, events, stderr) = exec.finish(Duration::from_secs(15));

    assert_eq!(status.code(), Some(1), "{stderr}");
    assert_eq!(count_type(&events, "turn.completed"), 1, "{events:#?}");
    assert_eq!(std::fs::read_to_string(&last_message)?, TURN_ONE);
    server.shutdown().await;
    Ok(())
}

/// `resume` and `fork` bind the lifecycle owner like a fresh start, so a resumed or forked root
/// drains its children too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_and_forked_exec_drain_the_wake_turn() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    for subcommand in ["resume", "fork"] {
        let test = test_codex_exec();
        let first_server = start_server(
            vec![response(vec![
                ev_response_created("first"),
                ev_assistant_message("first-msg", "first run"),
                ev_completed("first"),
            ])],
            Vec::new(),
        )
        .await;
        let (status, events, stderr) =
            RunningExec::spawn(&test, &first_server, &[]).finish(EXEC_TIMEOUT);
        assert!(status.success(), "{stderr}");
        first_server.shutdown().await;
        let source_id = events
            .iter()
            .find(|event| event["type"] == "thread.started")
            .and_then(|event| event["thread_id"].as_str())
            .expect("first run reports its thread")
            .to_string();

        let (release_child, child_gate) = oneshot::channel();
        let mut root_responses = spawn_turn();
        root_responses.push(response(vec![
            ev_response_created("root-wake"),
            ev_assistant_message("root-wake-msg", TURN_TWO),
            ev_completed("root-wake"),
        ]));
        let server = start_server(root_responses, vec![child_answer(child_gate)]).await;
        let args: &[&str] = if subcommand == "resume" {
            &["resume", "--last"]
        } else {
            &["fork", &source_id]
        };
        let mut exec = RunningExec::spawn(&test, &server, args);
        exec.wait_for_turn_completed();
        release_child.send(()).expect("child request is waiting");
        let (status, events, stderr) = exec.finish(EXEC_TIMEOUT);

        assert!(status.success(), "{subcommand}: {stderr}");
        assert_eq!(
            count_type(&events, "turn.completed"),
            2,
            "{subcommand}: {events:#?}"
        );
        assert!(
            events.iter().any(|event| {
                event["type"] == "item.completed" && event["item"]["text"] == TURN_TWO
            }),
            "{subcommand}: {events:#?}"
        );
        server.shutdown().await;
    }
    Ok(())
}

/// With the opt-out, exec never subscribes to work state, the root keeps `wait_agent`, and exec
/// exits after one root turn without waiting for the still-running child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opted_out_exec_runs_one_turn_with_wait_agent() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    // Holding the sender keeps the child running for the whole test.
    let (_hold_child, child_gate) = oneshot::channel();
    let server = start_server(spawn_turn(), vec![child_answer(child_gate)]).await;

    let test = test_codex_exec();
    let exec = RunningExec::spawn_with_env(
        &test,
        &server,
        &["-c", DRAIN_OPT_OUT],
        &[(
            "RUST_LOG",
            "codex_app_server::message_processor=trace,codex_app_server::outgoing_message=trace",
        )],
    );
    let (status, events, stderr) = exec.finish(Duration::from_secs(30));

    assert!(status.success(), "{stderr}");
    assert_eq!(
        (
            count_type(&events, "turn.started"),
            count_type(&events, "turn.completed")
        ),
        (1, 1),
        "{events:#?}"
    );
    // The trace names no request methods, so count them: initialize, thread/start, and turn/start
    // precede the first turn, with no thread/subscribeWorkState between them. An unobserved root
    // also never gets work-state notifications.
    let requests_before_turn = stderr
        .lines()
        .take_while(|line| !line.contains("app-server event: turn/started"))
        .filter(|line| line.contains("app-server typed request"))
        .count();
    assert_eq!(requests_before_turn, 3, "{stderr}");
    assert!(
        !stderr.contains("app-server event: thread/workStateChanged"),
        "{stderr}"
    );
    let requests = server.requests().await;
    let first_root_request = String::from_utf8_lossy(&requests[0]);
    assert!(
        first_root_request.contains(ROOT_PROMPT) && first_root_request.contains("wait_agent"),
        "the opted-out root should be offered wait_agent"
    );
    server.shutdown().await;
    Ok(())
}
