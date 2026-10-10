use super::*;
use codex_protocol::protocol::MultiAgentVersion;
use core_test_support::TestTargetOs;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;

#[path = "async_process_completion/idle_tests.rs"]
mod idle_tests;

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
    CompletedOnly,
    Rejected,
    Interrupted,
    RejectedReplacement,
    InterruptedReplacement,
}

#[test_case::test_case(Outcome::Success; "success")]
#[test_case::test_case(Outcome::Nonzero; "nonzero")]
#[test_case::test_case(Outcome::Cancelled; "cancelled")]
#[test_case::test_case(Outcome::Compatibility; "compatibility")]
#[test_case::test_case(Outcome::Compacted; "compacted")]
#[test_case::test_case(Outcome::CompletedOnly; "completed_only")]
#[test_case::test_case(Outcome::Rejected; "rejected_before_created")]
#[test_case::test_case(Outcome::Interrupted; "interrupted_before_created")]
#[test_case::test_case(Outcome::RejectedReplacement; "rejected_root_replacement")]
#[test_case::test_case(Outcome::InterruptedReplacement; "interrupted_root_replacement")]
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
    let recovery = matches!(outcome, Outcome::Rejected | Outcome::Interrupted);
    let same_assignment = recovery || outcome == Outcome::CompletedOnly;
    let replacement = matches!(
        outcome,
        Outcome::RejectedReplacement | Outcome::InterruptedReplacement
    );
    let interrupted = matches!(
        outcome,
        Outcome::Interrupted | Outcome::InterruptedReplacement
    );
    let rejection = matches!(outcome, Outcome::Rejected | Outcome::RejectedReplacement);
    let cancelled_native =
        recovery || replacement || matches!(outcome, Outcome::Cancelled | Outcome::CompletedOnly);
    let manual_compact = recovery || replacement || outcome == Outcome::CompletedOnly;
    let exit_code = if outcome == Outcome::Nonzero { 7 } else { 0 };
    let seconds = if cancelled_native {
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
    bodies.push(match outcome {
        Outcome::Rejected | Outcome::RejectedReplacement => {
            core_test_support::responses::sse_failed(
                "r3",
                "invalid_prompt",
                "rejected before creation",
            )
        }
        Outcome::CompletedOnly | Outcome::Interrupted | Outcome::InterruptedReplacement => {
            sse(vec![
                ev_assistant_message("r3", "continued"),
                ev_completed("r3"),
            ])
        }
        Outcome::Success
        | Outcome::Nonzero
        | Outcome::Cancelled
        | Outcome::Compatibility
        | Outcome::Compacted => assistant_response("r3"),
    });
    if manual_compact {
        bodies.push(sse(vec![ev_response_created("manual"), json!({
            "type":"response.output_item.done", "item":{"type":"compaction", "encrypted_content":"ASYNC_RESULT_SUMMARY"},
        }), ev_completed("manual")]));
    }
    bodies.push(assistant_response("r4"));
    if recovery || replacement {
        bodies.push(sse(vec![ev_response_created("accepted-compact"), json!({
            "type":"response.output_item.done", "item":{"type":"compaction", "encrypted_content":"ACCEPTED_SUMMARY"},
        }), ev_completed("accepted-compact")]));
        bodies.push(assistant_response("r5"));
    }
    let (response_release, response_gate) = tokio::sync::oneshot::channel();
    let mut response_gate = Some(response_gate);
    let mut gate = Some(gate);
    let chunks = bodies
        .into_iter()
        .enumerate()
        .map(|(index, body)| {
            vec![StreamingSseChunk {
                gate: if index == 1 {
                    gate.take()
                } else if index == 2 && interrupted {
                    response_gate.take()
                } else {
                    None
                },
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
        .with_model_info_override("gpt-5.6-sol", move |model| {
            if same_assignment {
                model.multi_agent_version = Some(MultiAgentVersion::V1);
            }
            model.truncation_policy =
                codex_protocol::openai_models::TruncationPolicyConfig::tokens(100000);
        })
        .with_config(move |config| {
            config
                .features
                .set_enabled(Feature::AsyncProcessCompletion, enabled)
                .expect("native completion fixture can select the feature");
            config.model_provider.base_url = Some(base_url);
            if same_assignment {
                // These compatibility retries retain the legacy assignment.
                config
                    .features
                    .disable(Feature::MultiAgentV2)
                    .expect("same-assignment fixture can select V1");
            }
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
    if same_assignment {
        assert_eq!(
            test.codex.multi_agent_version(),
            Some(MultiAgentVersion::V1)
        );
    } else if replacement {
        assert_eq!(
            test.codex.multi_agent_version(),
            Some(MultiAgentVersion::V2)
        );
    }
    server.wait_for_request_count(2).await;
    if cancelled_native {
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
    if interrupted {
        server.wait_for_request_count(3).await;
        test.codex.submit(Op::Interrupt).await?;
    }
    let mut recorded_id = None;
    let mut rejected = false;
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
        rejected |= matches!(event, EventMsg::Error(_));
        matches!(event, EventMsg::TurnComplete(_))
            || (interrupted && matches!(event, EventMsg::TurnAborted(_)))
    })
    .await;
    if rejection {
        assert!(rejected);
    }
    drop(response_release);
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
        let status = if cancelled_native {
            "cancelled".to_owned()
        } else {
            format!("exited({exit_code})")
        };
        assert!(text.contains(&format!("status={status}")));
        assert!(text.contains("call=finite"));
        if same_assignment {
            assert!(text.contains("assignment=none"));
        } else if replacement {
            assert!(!text.contains("assignment=none"));
        }
        if !cancelled_native {
            assert!(text.contains("terminal-tail"));
        }
        let item = final_request["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["content"][0]["text"].as_str() == Some(text.as_str()))
            .unwrap();
        assert!(serde_json::to_vec(item)?.len() <= 768);
        assert!(item["internal_chat_message_metadata_passthrough"]["create_time"].is_number());
        assert!(item["internal_chat_message_metadata_passthrough"]["turn_id"].is_string());
    }
    if manual_compact {
        test.codex.submit(Op::Compact).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    submit_unified_exec_turn(&test, "continue again", PermissionProfile::Disabled).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = request_bodies(&server).await;
    if outcome == Outcome::CompletedOnly {
        assert!(completions(requests.last().unwrap()).is_empty());
    } else {
        assert_eq!(completions(requests.last().unwrap()), delivered);
        if recovery || replacement {
            let receipt = |request: &Value| {
                request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| {
                        item["content"][0]["text"]
                            .as_str()
                            .is_some_and(|text| text.starts_with("<async_tool_completion>"))
                    })
                    .unwrap()
                    .clone()
            };
            assert_eq!(receipt(final_request), receipt(requests.last().unwrap()));
        }
    }
    if recovery || replacement {
        test.codex.submit(Op::Compact).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        submit_unified_exec_turn(
            &test,
            "after accepted recovery",
            PermissionProfile::Disabled,
        )
        .await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert!(completions(request_bodies(&server).await.last().unwrap()).is_empty());
    }
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

#[test_case::test_case(Outcome::Success; "created")]
#[test_case::test_case(Outcome::CompletedOnly; "completed_only")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_native_completion_paused_by_an_idle_interrupt_is_recovered_by_an_explicit_v2_root(
    outcome: Outcome,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    let seconds = if cfg!(windows) { 11 } else { 3 };
    let args = json!({ "cmd": finite_command(seconds, 0), "yield_time_ms": 250 });
    let bootstrap = start_mock_server().await;
    let recovery = if outcome == Outcome::CompletedOnly {
        sse(vec![
            ev_assistant_message("recovered", "continued"),
            ev_completed("recovered"),
        ])
    } else {
        assistant_response("recovered")
    };
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk { gate: None, body: tool_response("idle-finite", &args) }],
        vec![StreamingSseChunk { gate: None, body: assistant_response("idle") }],
        vec![StreamingSseChunk { gate: None, body: recovery }],
        vec![StreamingSseChunk { gate: None, body: sse(vec![ev_response_created("compact"), json!({
            "type":"response.output_item.done", "item":{"type":"compaction", "encrypted_content":"IDLE_RESULT_SUMMARY"},
        }), ev_completed("compact")]) }],
        vec![StreamingSseChunk { gate: None, body: assistant_response("after-compact") }],
    ]).await;
    let base_url = format!("{}/v1", server.uri());
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(move |config| {
            config
                .features
                .enable(Feature::AsyncProcessCompletion)
                .expect("idle completion fixture can enable the feature");
            config.model_provider.base_url = Some(base_url);
        })
        .build_with_auto_env(&bootstrap)
        .await?;
    assert_eq!(
        test.codex.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    submit_unified_exec_turn(
        &test,
        "start finite work and finish the turn",
        PermissionProfile::Disabled,
    )
    .await?;
    let initial = wait_for_raw_unified_exec_output(&test, "idle-finite").await?;
    assert!(initial.process_id.is_some());
    let origin = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TurnComplete(turn) => Some(turn.turn_id.clone()),
        _ => None,
    })
    .await;
    // Covers explicit-root recovery after an idle interrupt: in wake mode `Op::Interrupt` also
    // ends the idle wake assignment (synthetic `TurnAborted`) and pauses automatic wakes, so the
    // explicit root turn, not an idle wake, recovers the result.
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::AgentWakeupsUpdated(update) if update.paused),
    )
    .await;
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == "idle-finite"),
    )
    .await;
    assert!(completions(request_bodies(&server).await.last().unwrap()).is_empty());
    submit_unified_exec_turn(&test, "recover retained work", PermissionProfile::Disabled).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = request_bodies(&server).await;
    let delivered = completions(requests.last().unwrap());
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].contains("call=idle-finite"));
    assert!(delivered[0].contains("status=exited(0)"));
    assert!(delivered[0].contains("terminal-tail"));
    assert!(!delivered[0].contains("assignment=none"));
    assert!(delivered[0].contains(&format!("turn={origin}")));
    let item = requests.last().unwrap()["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["content"][0]["text"].as_str() == Some(delivered[0].as_str()))
        .unwrap();
    assert_eq!(
        item["internal_chat_message_metadata_passthrough"]["turn_id"],
        origin
    );
    assert!(item["internal_chat_message_metadata_passthrough"]["create_time"].is_number());
    assert!(serde_json::to_vec(item)?.len() <= 768);
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    submit_unified_exec_turn(
        &test,
        "after accepted completion",
        PermissionProfile::Disabled,
    )
    .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(completions(request_bodies(&server).await.last().unwrap()).is_empty());
    server.shutdown().await;
    Ok(())
}

#[test_case::test_case(Outcome::Success; "explicit_authority_survives")]
#[test_case::test_case(Outcome::Compatibility; "automatic_authority_is_not_promoted")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_steering_reentry_keeps_the_original_completion_policy(
    outcome: Outcome,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    let explicit = outcome == Outcome::Success;
    let (release, gate) = tokio::sync::oneshot::channel();
    // Stop runs after run_turn's final pending-input check. Hold it until a late steer is queued.
    let (hook_server, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(gate),
        body: String::new(),
    }]])
    .await;
    let hook_url = format!("{}/v1/responses", hook_server.uri());
    let bootstrap = start_mock_server().await;
    let args = json!({ "cmd": finite_command(30, 0), "yield_time_ms": 250 });
    let pause = sse(vec![
        ev_response_created("pause"),
        ev_assistant_message("pause", "pause-before-reentry"),
        ev_completed("pause"),
    ]);
    let bodies = [
        tool_response("reentry-finite", &args),
        assistant_response("idle"),
        pause,
        assistant_response("reentered"),
        assistant_response("explicit-recovery"),
    ];
    let (server, _) = start_streaming_sse_server(
        bodies
            .into_iter()
            .map(|body| vec![StreamingSseChunk { gate: None, body }])
            .collect(),
    )
    .await;
    let base_url = format!("{}/v1", server.uri());
    // Command hooks run on the app host and require a host-native working directory.
    let test = test_codex().with_model("gpt-5.6-sol")
        .with_pre_build_hook(move |home| {
            let script = home.join("reentry_stop.py");
            fs::write(&script, format!(r#"import json, sys, urllib.request
payload = json.load(sys.stdin)
if payload.get("last_assistant_message") == "pause-before-reentry":
    urllib.request.urlopen(urllib.request.Request({hook_url:?}, data=json.dumps(payload).encode()), timeout=30).read()
print("{{}}")
"#)).unwrap();
            fs::write(home.join("hooks.json"), json!({ "hooks": { "Stop": [{ "hooks": [{
                "type": "command", "command": format!("python3 \"{}\"", script.display()),
            }] }] } }).to_string()).unwrap();
        }).with_config(move |config| {
            config.features.enable(Feature::AsyncProcessCompletion).expect("reentry fixture can enable completion");
            core_test_support::hooks::trust_discovered_hooks(config);
            config.model_provider.base_url = Some(base_url);
        }).build(&bootstrap).await?;
    assert_eq!(
        test.codex.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    submit_unified_exec_turn(&test, "start work", PermissionProfile::Disabled).await?;
    let initial = wait_for_raw_unified_exec_output(&test, "reentry-finite").await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    if explicit {
        submit_unified_exec_turn(&test, "continue", PermissionProfile::Disabled).await?;
    } else {
        let item = serde_json::from_value(
            json!({ "type": "message", "role": "user", "content": [{"type": "input_text", "text": "automatic context"}] }),
        )?;
        test.codex
            .start_turn_if_idle(TurnInputRequest::new(
                codex_protocol::turn_input::TurnInput::ResponseItem(item),
            ))
            .await?;
    }
    tokio::time::timeout(
        Duration::from_secs(20),
        hook_server.wait_for_request_count(1),
    )
    .await
    .context("local Stop hook must reach the late-steer gate")?;
    let turn = request_bodies(&hook_server).await[0]["turn_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        test.codex
            .terminate_background_terminal(initial.process_id.unwrap().parse()?)
            .await
    );
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == "reentry-finite"),
    )
    .await;
    let steer = test
        .codex
        .steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "late human steer".to_owned(),
                text_elements: Vec::new(),
            }]),
            turn.clone(),
        )
        .await?;
    assert_eq!(
        steer,
        codex_protocol::turn_input::SteerSubmission::Steered { turn_id: turn }
    );
    release.send(()).unwrap();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = request_bodies(&server).await;
    assert_eq!(requests.len(), 4);
    assert!(completions(&requests[2]).is_empty());
    assert_eq!(completions(&requests[3]).len(), usize::from(explicit));
    assert!(requests[3].to_string().contains("late human steer"));
    if !explicit {
        submit_unified_exec_turn(&test, "recover now", PermissionProfile::Disabled).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert_eq!(
            completions(request_bodies(&server).await.last().unwrap()).len(),
            1
        );
    }
    server.shutdown().await;
    hook_server.shutdown().await;
    Ok(())
}
