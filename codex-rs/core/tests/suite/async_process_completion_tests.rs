use super::*;
use core_test_support::TestTargetOs;
use core_test_support::test_target_os;

fn finite_command(delay_seconds: u32, exit_code: i32) -> String {
    match test_target_os() {
        TestTargetOs::Windows => {
            format!("Start-Sleep -Seconds {delay_seconds}; echo terminal-tail; exit {exit_code}")
        }
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            format!("sleep {delay_seconds}; echo terminal-tail; exit {exit_code}")
        }
    }
}

fn completions(request: &core_test_support::responses::ResponsesRequest) -> Vec<String> {
    request
        .input()
        .iter()
        .filter_map(|item| {
            let text = item["content"][0]["text"].as_str()?;
            text.starts_with("<async_tool_completion>")
                .then(|| text.to_owned())
        })
        .collect()
}

fn tool_response(id: &str, args: &Value) -> String {
    sse(vec![
        ev_function_call(id, "exec_command", &args.to_string()),
        ev_completed(id),
    ])
}

fn assistant_response(id: &str) -> String {
    sse(vec![
        ev_assistant_message(id, "continued"),
        ev_completed(id),
    ])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Success,
    Nonzero,
    Cancelled,
    Compatibility,
}

#[rstest::rstest]
#[case(Outcome::Success)]
#[case(Outcome::Nonzero)]
#[case(Outcome::Cancelled)]
#[case(Outcome::Compatibility)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_completion_is_appended_in_the_active_turn_without_stdin(
    #[case] outcome: Outcome,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    let enabled = outcome != Outcome::Compatibility;
    let exit_code = if outcome == Outcome::Nonzero { 7 } else { 0 };
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(move |config| {
        config
            .features
            .set_enabled(Feature::AsyncProcessCompletion, enabled);
    });
    let test = builder.build_with_auto_env(&server).await?;
    let seconds = if outcome == Outcome::Cancelled {
        30
    } else if test_target_os() == TestTargetOs::Windows {
        11
    } else {
        1
    };
    let args = json!({ "cmd": finite_command(seconds, exit_code), "yield_time_ms": 250 });
    // This independent command keeps an active turn past the first native exit.
    let independent = json!({ "cmd": finite_command(2, 0), "yield_time_ms": 3000 });
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response("finite", &args),
            tool_response("independent", &independent),
            assistant_response("r3"),
            assistant_response("r4"),
        ],
    )
    .await;
    submit_unified_exec_turn(&test, "start finite work", PermissionProfile::Disabled).await?;
    let initial = wait_for_raw_unified_exec_output(&test, "finite").await?;
    assert!(initial.process_id.is_some());
    if outcome == Outcome::Cancelled {
        assert!(
            test.codex
                .terminate_background_terminal(initial.process_id.unwrap().parse()?)
                .await
        );
    }
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(completions(&mock.requests()[1]).is_empty());
    let inline = mock.requests()[2].function_call_output("independent");
    assert!(
        parse_unified_exec_output(extract_output_text(&inline).unwrap())?
            .process_id
            .is_none()
    );
    let delivered = completions(&mock.requests()[2]);
    assert_eq!(delivered.len(), usize::from(enabled));
    if let Some(text) = delivered.first() {
        let status = if outcome == Outcome::Cancelled {
            "cancelled".to_owned()
        } else {
            format!("exited({exit_code})")
        };
        assert!(text.contains(&format!("status={status}")));
        assert!(text.contains("call=finite"));
        if outcome != Outcome::Cancelled {
            assert!(text.contains("terminal-tail"));
        }
        assert!(text.len() <= 768);
    }
    submit_unified_exec_turn(&test, "continue again", PermissionProfile::Disabled).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(completions(&mock.requests()[3]), delivered);
    Ok(())
}
