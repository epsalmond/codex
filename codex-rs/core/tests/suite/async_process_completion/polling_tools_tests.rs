//! Empty `write_stdin` polls under automatic completion: the model's polls of a session whose
//! result is delivered automatically are refused, while input, TTY polls, and polls from inside a
//! Code Mode cell keep working.
use super::*;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_custom_tool_call;
use pretty_assertions::assert_eq;

const REFUSAL: &str =
    "session 1000 is running; its exit status and output tail will be delivered automatically.";

fn stdin_response(id: &str, args: &Value) -> String {
    sse(vec![
        ev_response_created(id),
        ev_function_call(id, "write_stdin", &args.to_string()),
        ev_completed(id),
    ])
}

fn function_output(request: &ResponsesRequest, call_id: &str) -> String {
    request
        .function_call_output_content_and_success(call_id)
        .and_then(|(content, _)| content)
        .unwrap_or_else(|| panic!("missing output for {call_id}"))
}

fn write_stdin_chars_description(request: &ResponsesRequest) -> String {
    request.body_json()["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "write_stdin")
        .and_then(|tool| tool["parameters"]["properties"]["chars"]["description"].as_str())
        .expect("write_stdin chars description")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_stdin_empty_poll_refused_under_automatic_completion() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX-only command fixture");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let requests = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "start",
                &json!({ "cmd": "sleep 30; echo late", "yield_time_ms": 250 }),
            ),
            stdin_response(
                "poll",
                &json!({ "session_id": 1000, "chars": "", "yield_time_ms": 5000 }),
            ),
            assistant_response("done"),
        ],
    )
    .await;

    let started = std::time::Instant::now();
    submit_unified_exec_turn(&test, "poll the session", PermissionProfile::Disabled).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        !write_stdin_chars_description(&requests[0]).contains("retrieves session output"),
        "the model-facing spec must not offer empty polls"
    );
    assert_eq!(function_output(&requests[2], "poll"), REFUSAL);
    // The refusal returns at once instead of waiting out the poll's yield time.
    assert!(started.elapsed() < Duration::from_secs(5));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_stdin_input_and_tty_poll_still_work_under_automatic_completion() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX-only command fixture");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let requests = mount_sse_sequence(
        &server,
        vec![
            // Session 1000 has automatic delivery; non-empty input is not refused, it reaches
            // the session's usual closed-stdin handling.
            tool_response("pipe", &json!({ "cmd": "sleep 30", "yield_time_ms": 250 })),
            stdin_response(
                "pipe-input",
                &json!({ "session_id": 1000, "chars": "hello\n", "yield_time_ms": 250 }),
            ),
            // TTY session 1001 has no automatic delivery, so it takes input and empty polls.
            tool_response(
                "tty",
                &json!({
                    "cmd": "read line; sleep 1; echo tty-got:$line",
                    "yield_time_ms": 250,
                    "tty": true,
                }),
            ),
            stdin_response(
                "tty-input",
                &json!({ "session_id": 1001, "chars": "hello\n", "yield_time_ms": 250 }),
            ),
            stdin_response(
                "tty-poll",
                &json!({ "session_id": 1001, "chars": "", "yield_time_ms": 5000 }),
            ),
            assistant_response("done"),
        ],
    )
    .await;

    submit_unified_exec_turn(&test, "use both sessions", PermissionProfile::Disabled).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = requests.requests();
    assert_eq!(requests.len(), 6);
    let pipe_input = function_output(&requests[2], "pipe-input");
    assert!(pipe_input.contains("stdin is closed"), "{pipe_input}");
    assert!(
        !pipe_input.contains("delivered automatically"),
        "{pipe_input}"
    );
    let tty_poll = function_output(&requests[5], "tty-poll");
    assert!(tty_poll.contains("tty-got:hello"), "{tty_poll}");
    assert!(!tty_poll.contains("delivered automatically"), "{tty_poll}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_write_stdin_empty_poll_still_works_under_automatic_completion() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX-only command fixture");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
        })
        .build_with_auto_env(&server)
        .await?;
    let code = r#"
const process = await tools.exec_command({cmd: "sleep 1; echo nested-done", yield_time_ms: 1});
const polled = await tools.write_stdin({session_id: process.session_id, yield_time_ms: 10000});
text(JSON.stringify(polled));
"#;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("cell"),
                ev_custom_tool_call("cell", "exec", code),
                ev_completed("cell"),
            ]),
            assistant_response("done"),
        ],
    )
    .await;

    submit_unified_exec_turn(&test, "poll inside a cell", PermissionProfile::Disabled).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1].custom_tool_call_output("cell").to_string();
    assert!(output.contains("nested-done"), "{output}");
    assert!(!output.contains("delivered automatically"), "{output}");
    Ok(())
}
