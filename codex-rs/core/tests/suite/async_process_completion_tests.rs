use super::*;
use core_test_support::TestTargetOs;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;

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

fn completions(request: &Value) -> Vec<String> {
    request["input"]
        .as_array()
        .unwrap()
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
        ev_response_created(id),
        ev_function_call(id, "exec_command", &args.to_string()),
        ev_completed(id),
    ])
}

fn assistant_response(id: &str) -> String {
    sse(vec![
        ev_response_created(id),
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
    Compacted,
}

#[test_case::test_case(Outcome::Success; "success")]
#[test_case::test_case(Outcome::Nonzero; "nonzero")]
#[test_case::test_case(Outcome::Cancelled; "cancelled")]
#[test_case::test_case(Outcome::Compatibility; "compatibility")]
#[test_case::test_case(Outcome::Compacted; "compacted")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_completion_is_appended_in_the_active_turn_without_stdin(
    outcome: Outcome,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    let enabled = outcome != Outcome::Compatibility;
    let exit_code = if outcome == Outcome::Nonzero { 7 } else { 0 };
    let seconds = if outcome == Outcome::Cancelled {
        30
    } else if cfg!(windows) {
        11
    } else {
        1
    };
    let args = json!({ "cmd": finite_command(seconds, exit_code), "yield_time_ms": 250 });
    let command = if outcome == Outcome::Compacted {
        match test_target_os() {
            TestTargetOs::Windows => "[Console]::Write(('padding ' * 70000))",
            TestTargetOs::Linux | TestTargetOs::MacOs => "printf 'padding %.0s' {1..70000}",
        }
        .to_owned()
    } else {
        finite_command(0, 0)
    };
    let independent = json!({ "cmd": command, "yield_time_ms": 3000, "max_output_tokens": 100000 });
    let (release, gate) = tokio::sync::oneshot::channel();
    let mut bodies = vec![
        tool_response("finite", &args),
        tool_response("independent", &independent),
    ];
    if outcome == Outcome::Compacted {
        bodies.push(sse(vec![ev_response_created("compact"), json!({
            "type":"response.output_item.done", "item":{"type":"compaction", "encrypted_content":"ASYNC_RESULT_SUMMARY"},
        }), ev_completed("compact")]));
    }
    bodies.extend([assistant_response("r3"), assistant_response("r4")]);
    let mut gate = Some(gate);
    let chunks = bodies
        .into_iter()
        .enumerate()
        .map(|(index, body)| {
            vec![StreamingSseChunk {
                gate: if index == 1 { gate.take() } else { None },
                body,
            }]
        })
        .collect();
    let (server, _) = start_streaming_sse_server(chunks).await;
    let base_url = format!("{}/v1", server.uri());
    // Keep automatic executor selection while reusing the existing gated SSE fixture.
    let bootstrap = start_mock_server().await;
    let mut builder = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.truncation_policy =
                codex_protocol::openai_models::TruncationPolicyConfig::tokens(100000);
        })
        .with_config(move |config| {
            config
                .features
                .set_enabled(Feature::AsyncProcessCompletion, enabled);
            config.model_provider.base_url = Some(base_url);
            if outcome == Outcome::Compacted {
                config.tool_output_token_limit = Some(100000);
                config.subagent_context_reduction =
                    codex_core::config::SubagentContextReductionConfig {
                        enabled: true,
                        threshold_tokens: 50000,
                    };
            }
        });
    if outcome == Outcome::Compacted {
        builder = builder.with_session_source(codex_protocol::protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::Review,
        ));
    }
    let test = builder.build_with_auto_env(&bootstrap).await?;
    submit_unified_exec_turn(&test, "start finite work", PermissionProfile::Disabled).await?;
    let initial = wait_for_raw_unified_exec_output(&test, "finite").await?;
    assert!(initial.process_id.is_some());
    server.wait_for_request_count(2).await;
    if outcome == Outcome::Cancelled {
        assert!(
            test.codex
                .terminate_background_terminal(initial.process_id.unwrap().parse()?)
                .await
        );
    }
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == "finite"),
    )
    .await;
    release.send(()).unwrap();
    let mut recorded_id = None;
    wait_for_event(&test.codex, |event| {
        if let EventMsg::RawResponseItem(raw) = event {
            let value = serde_json::to_value(&raw.item).unwrap();
            if value["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("<async_tool_completion>"))
            {
                recorded_id.get_or_insert(value["id"].clone());
            }
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = request_bodies(&server).await;
    let final_request = requests.last().unwrap();
    if outcome != Outcome::Compacted {
        assert!(
            final_request["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call_output"
                    && item["call_id"] == "independent")
        );
    }
    let delivered = completions(final_request);
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
    let requests = request_bodies(&server).await;
    assert_eq!(completions(requests.last().unwrap()), delivered);
    if outcome == Outcome::Compacted {
        let compact = requests
            .iter()
            .find(|request| {
                request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "compaction_trigger")
            })
            .unwrap();
        assert!(
            compact["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "compaction_trigger")
        );
        let terminal_id = |request: &Value| {
            request["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| {
                    item["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.starts_with("<async_tool_completion>"))
                })
                .unwrap()["id"]
                .clone()
        };
        assert_eq!(
            recorded_id.unwrap(),
            terminal_id(&requests[requests.len() - 2])
        );
        assert!(
            requests[requests.len() - 2]
                .to_string()
                .contains("ASYNC_RESULT_SUMMARY")
        );
    }
    server.shutdown().await;
    Ok(())
}

async fn request_bodies(server: &StreamingSseServer) -> Vec<Value> {
    server
        .requests()
        .await
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .collect()
}
