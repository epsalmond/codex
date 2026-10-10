use super::*;
use codex_core::TurnInputSubmission;
use codex_core::test_support::TurnStartHold;
use codex_core::test_support::TurnStartHoldPoint;
use codex_core::test_support::hold_next_turn_start;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TurnAbortReason;
use core_test_support::test_codex::TestCodex;
use pretty_assertions::assert_eq;
use std::sync::Arc;

// The helper owns a bounded readiness loop; the model never reads or polls it.
fn gated_helper() -> &'static str {
    match test_target_os() {
        TestTargetOs::Windows => {
            "for ($i=0; $i -lt 300; $i++) { if (Test-Path async-release) { echo terminal-tail; exit 0 }; Start-Sleep -Milliseconds 50 }; exit 9"
        }
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            "for i in {1..300}; do if test -f async-release; then echo terminal-tail; exit 0; fi; sleep 0.05; done; exit 9"
        }
    }
}

async fn release_helper(test: &TestCodex) -> Result<()> {
    test.fs()
        .write_file(
            &test.workspace_path_uri("async-release")?,
            Vec::new(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IdleCase {
    Idle,
    FinalBoundary,
    Paused,
    Rejected,
    Closed,
}

#[test_case::test_case(false, IdleCase::Idle; "legacy_idle")]
#[test_case::test_case(true, IdleCase::Idle; "v2_idle")]
#[test_case::test_case(false, IdleCase::FinalBoundary; "legacy_final_boundary")]
#[test_case::test_case(true, IdleCase::FinalBoundary; "v2_final_boundary")]
#[test_case::test_case(false, IdleCase::Paused; "legacy_pause_resume")]
#[test_case::test_case(true, IdleCase::Paused; "v2_pause_resume")]
#[test_case::test_case(false, IdleCase::Rejected; "legacy_reject_compact_resume")]
#[test_case::test_case(true, IdleCase::Rejected; "v2_reject_compact_resume")]
#[test_case::test_case(true, IdleCase::Closed; "intentional_close")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_owner_receives_native_completion(v2: bool, case: IdleCase) -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    let (release_final, final_gate) = tokio::sync::oneshot::channel();
    let mut final_gate = Some(final_gate);
    let bodies = vec![
        tool_response(
            "finite",
            &json!({"cmd": gated_helper(), "yield_time_ms": 250}),
        ),
        assistant_response("idle"),
        if case == IdleCase::Rejected {
            core_test_support::responses::sse_failed(
                "rejected",
                "invalid_prompt",
                "rejected before creation",
            )
        } else {
            assistant_response("automatic")
        },
        sse(vec![
            ev_response_created("compact"),
            json!({"type": "response.output_item.done", "item": {"type":"compaction", "encrypted_content":"PENDING_SUMMARY"}}),
            ev_completed("compact"),
        ]),
        assistant_response("resumed"),
        sse(vec![
            ev_response_created("accepted-compact"),
            json!({"type": "response.output_item.done", "item": {"type":"compaction", "encrypted_content":"ACCEPTED_SUMMARY"}}),
            ev_completed("accepted-compact"),
        ]),
        assistant_response("after-acceptance"),
    ];
    let (server, _) = start_streaming_sse_server(
        bodies
            .into_iter()
            .enumerate()
            .map(|(index, body)| {
                vec![StreamingSseChunk {
                    gate: (index == 1 && case == IdleCase::FinalBoundary)
                        .then(|| final_gate.take().unwrap()),
                    body,
                }]
            })
            .collect(),
    )
    .await;
    let base_url = format!("{}/v1", server.uri());
    let bootstrap = start_mock_server().await;
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", move |model| {
            model.multi_agent_version = Some(if v2 {
                MultiAgentVersion::V2
            } else {
                MultiAgentVersion::V1
            })
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::AsyncProcessCompletion)
                .unwrap();
            if v2 {
                config.features.enable(Feature::MultiAgentV2).unwrap();
            } else {
                config.features.disable(Feature::MultiAgentV2).unwrap();
            }
            config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Disabled;
            config.model_provider.base_url = Some(base_url);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.request_max_retries = Some(0);
        })
        .build_with_auto_env(&bootstrap)
        .await?;
    submit_unified_exec_turn(&test, "start finite work", PermissionProfile::Disabled).await?;
    let initial = wait_for_raw_unified_exec_output(&test, "finite").await?;
    assert!(initial.process_id.is_some());
    server.wait_for_request_count(2).await;
    if case != IdleCase::FinalBoundary {
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        if v2 {
            assert_eq!(test.codex.agent_status().await, AgentStatus::Waiting);
        }
    }
    if case == IdleCase::Closed {
        test.codex.shutdown_and_wait().await?;
        release_helper(&test).await?;
        assert_eq!(request_bodies(&server).await.len(), 2);
        server.shutdown().await;
        return Ok(());
    }
    if case == IdleCase::Paused {
        test.codex.submit(Op::Interrupt).await?;
        wait_for_event(
            &test.codex,
            |event| matches!(event, EventMsg::AgentWakeupsUpdated(update) if update.paused),
        )
        .await;
    }
    release_helper(&test).await?;
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == "finite"),
    )
    .await;
    if case == IdleCase::FinalBoundary {
        release_final.send(()).unwrap();
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    if case == IdleCase::Paused {
        assert_eq!(request_bodies(&server).await.len(), 2);
        submit_unified_exec_turn(
            &test,
            "resume owned completion",
            PermissionProfile::Disabled,
        )
        .await?;
    }
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = request_bodies(&server).await;
    assert_eq!(requests.len(), 3);
    let delivered = completions(&requests[2]);
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].contains("status=exited(0)"));
    assert!(delivered[0].contains("terminal-tail"));
    assert!(delivered[0].contains(if v2 { "assignment=" } else { "assignment=none" }));
    if case != IdleCase::Paused {
        let texts = requests[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["content"][0]["text"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            texts
                .iter()
                .filter(|text| **text == "start finite work")
                .count(),
            1
        );
        assert!(!texts.contains(&"resume owned completion"));
    }
    if case == IdleCase::Rejected {
        test.codex.submit(Op::Compact).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        submit_unified_exec_turn(&test, "explicit recovery", PermissionProfile::Disabled).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let requests = request_bodies(&server).await;
        assert_eq!(requests.len(), 5);
        let receipt = |body: &Value| {
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["content"][0]["text"].as_str() == Some(delivered[0].as_str()))
                .unwrap()
                .clone()
        };
        assert_eq!(receipt(&requests[2]), receipt(&requests[4]));
        assert!(serde_json::to_vec(&receipt(&requests[4]))?.len() <= 768);
        test.codex.submit(Op::Compact).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        submit_unified_exec_turn(&test, "after acceptance", PermissionProfile::Disabled).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert!(completions(request_bodies(&server).await.last().unwrap()).is_empty());
    }
    server.shutdown().await;
    Ok(())
}

fn user_turn(text: &str) -> TurnInputRequest {
    TurnInputRequest::user_input(vec![UserInput::Text {
        text: text.to_owned(),
        text_elements: Vec::new(),
    }])
}

/// Leaves a V2 root idle with a ready completion whose wake is parked at `point`, after it
/// reserved the slot and before it installs its task.
async fn park_idle_wake_with_ready_completion(
    point: TurnStartHoldPoint,
) -> Result<(TestCodex, StreamingSseServer, TurnStartHold)> {
    let args = json!({ "cmd": gated_helper(), "yield_time_ms": 250 });
    let mut bodies = vec![
        tool_response("race-finite", &args),
        assistant_response("idle"),
    ];
    // One or two samples for the turn that takes the completion, then one follow-up turn.
    bodies.extend((0..4).map(|index| assistant_response(&format!("after-{index}"))));
    let (server, _) = start_streaming_sse_server(
        bodies
            .into_iter()
            .map(|body| vec![StreamingSseChunk { gate: None, body }])
            .collect(),
    )
    .await;
    let base_url = format!("{}/v1", server.uri());
    let bootstrap = start_mock_server().await;
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(move |config| {
            config
                .features
                .enable(Feature::AsyncProcessCompletion)
                .unwrap();
            config.model_provider.base_url = Some(base_url);
        })
        .build_with_auto_env(&bootstrap)
        .await?;
    assert_eq!(
        test.codex.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    submit_unified_exec_turn(&test, "start finite work", PermissionProfile::Disabled).await?;
    let initial = wait_for_raw_unified_exec_output(&test, "race-finite").await?;
    assert!(initial.process_id.is_some());
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(test.codex.agent_status().await, AgentStatus::Waiting);
    let mut hold = hold_next_turn_start(test.session_configured.thread_id, point);
    release_helper(&test).await?;
    tokio::time::timeout(Duration::from_secs(30), hold.reached())
        .await
        .expect("the completion should start an idle wake");
    Ok((test, server, hold))
}

/// Runs a follow-up turn, then checks that its request carries each prompt once and the
/// completion exactly once, and that no request ever carried the completion twice. Returns the
/// events seen until the follow-up completed.
async fn assert_follow_up_sees_completion_once(
    test: &TestCodex,
    server: &StreamingSseServer,
    prompt: &str,
) -> Result<Vec<EventMsg>> {
    let TurnInputSubmission::Started { turn_id } = test
        .codex
        .start_or_steer_turn(user_turn("after the race"))
        .await?
    else {
        panic!("idle follow-up must start a turn");
    };
    let mut seen = Vec::new();
    wait_for_event(&test.codex, |event| {
        seen.push(event.clone());
        matches!(event, EventMsg::TurnComplete(turn) if turn.turn_id == turn_id)
    })
    .await;
    let requests = request_bodies(server).await;
    let texts = |request: &Value| {
        request["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["content"][0]["text"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let last = texts(requests.last().unwrap());
    let delivered = completions(requests.last().unwrap());
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].contains("call=race-finite"));
    assert!(delivered[0].contains("terminal-tail"));
    for prompt in [prompt, "after the race"] {
        assert_eq!(last.iter().filter(|text| *text == prompt).count(), 1);
    }
    assert!(
        requests
            .iter()
            .all(|request| completions(request).len() <= 1)
    );
    Ok(seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_root_turn_inside_a_starting_idle_wake_steers_into_it_and_delivers_once()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    // The window in which explicit input used to be rejected.
    let (test, server, hold) =
        park_idle_wake_with_ready_completion(TurnStartHoldPoint::WakeBeforeInstall).await?;
    let codex = Arc::clone(&test.codex);
    let explicit = tokio::spawn(async move {
        codex
            .start_or_steer_turn(user_turn("explicit during wake"))
            .await
    });
    // Let the explicit input reach the wait, well inside its bound, then let the wake run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    hold.release();
    let TurnInputSubmission::Steered { turn_id } = explicit.await?? else {
        panic!("explicit input inside a starting wake must steer into it");
    };
    let wake_turn_id = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TurnStarted(started) => Some(started.turn_id.clone()),
        _ => None,
    })
    .await;
    assert_eq!(turn_id, wake_turn_id);
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(turn) if turn.turn_id == turn_id),
    )
    .await;
    assert_follow_up_sees_completion_once(&test, &server, "explicit during wake").await?;
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_root_turn_replaces_a_wake_held_past_the_wait_and_the_wake_aborts_cleanly()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    let (test, server, hold) =
        park_idle_wake_with_ready_completion(TurnStartHoldPoint::WakeBeforeInstall).await?;
    // The wake stays parked past the wait bound, so the explicit turn takes the slot and the
    // wake's assignment, then starts on its own.
    let TurnInputSubmission::Started { turn_id } = tokio::time::timeout(
        Duration::from_secs(10),
        test.codex
            .start_or_steer_turn(user_turn("explicit after the wait")),
    )
    .await??
    else {
        panic!("explicit input must start a turn once the wait times out");
    };
    hold.release();
    // The parked wake finds the slot is no longer its reservation and ends without starting.
    let mut explicit_completed = false;
    let mut aborted_wake = None;
    while !explicit_completed || aborted_wake.is_none() {
        let event = wait_for_event_match(&test.codex, |event| {
            matches!(
                event,
                EventMsg::TurnStarted(_) | EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
            )
            .then(|| event.clone())
        })
        .await;
        match event {
            EventMsg::TurnStarted(started) => {
                assert_eq!(started.turn_id, turn_id, "only the explicit turn may start");
            }
            EventMsg::TurnComplete(complete) => {
                assert_eq!(complete.turn_id, turn_id);
                explicit_completed = true;
            }
            EventMsg::TurnAborted(aborted) => {
                assert_eq!(aborted.reason, TurnAbortReason::Interrupted);
                assert_ne!(aborted.turn_id.as_ref(), Some(&turn_id));
                assert_eq!(aborted_wake.replace(aborted.turn_id), None);
            }
            _ => unreachable!("the matcher selects only turn lifecycle events"),
        }
    }
    assert_follow_up_sees_completion_once(&test, &server, "explicit after the wait").await?;
    server.shutdown().await;
    Ok(())
}

/// Holds the wake after it reserved the slot and before it binds its assignment, past the input
/// wait bound, while explicit input takes the slot. The wake must then find it no longer owns the
/// slot and end quietly: no TurnStarted or TurnAborted, no failed-start warning, and no pause of
/// automatic wakeups. The explicit turn takes the completion.
async fn assert_wake_replaced_before_bind_ends_quietly(explicit_binds_first: bool) -> Result<()> {
    let (test, server, wake) =
        park_idle_wake_with_ready_completion(TurnStartHoldPoint::WakeBeforeBind).await?;
    let thread_id = test.session_configured.thread_id;
    let mut input = hold_next_turn_start(thread_id, TurnStartHoldPoint::InputBeforeBind);
    let codex = Arc::clone(&test.codex);
    let explicit = tokio::spawn(async move {
        codex
            .start_or_steer_turn(user_turn("explicit replaces the wake"))
            .await
    });
    // The explicit input waits out its bound, then replaces the reservation and hands over a
    // binding the wake does not have yet.
    tokio::time::timeout(Duration::from_secs(10), input.reached())
        .await
        .expect("explicit input should replace the parked wake");
    let submission = if explicit_binds_first {
        input.release();
        let submission = explicit.await??;
        wake.release_and_wait().await;
        submission
    } else {
        // The wake tries to bind between the replacement and the explicit bind.
        wake.release_and_wait().await;
        input.release();
        explicit.await??
    };
    let TurnInputSubmission::Started { turn_id } = submission else {
        panic!("explicit input must start once it replaced the wake");
    };
    let mut seen = Vec::new();
    wait_for_event(&test.codex, |event| {
        seen.push(event.clone());
        matches!(event, EventMsg::TurnComplete(turn) if turn.turn_id == turn_id)
    })
    .await;
    seen.extend(
        assert_follow_up_sees_completion_once(&test, &server, "explicit replaces the wake").await?,
    );
    let started = seen
        .iter()
        .filter(|event| matches!(event, EventMsg::TurnStarted(_)))
        .count();
    assert_eq!(
        started, 2,
        "only the explicit and follow-up turns may start"
    );
    for event in &seen {
        match event {
            EventMsg::TurnAborted(aborted) => panic!("no turn may abort: {aborted:?}"),
            EventMsg::Warning(warning) => assert!(
                !warning.message.contains("Automatic turn did not start"),
                "the replaced wake must not report a failed start: {}",
                warning.message
            ),
            EventMsg::AgentWakeupsUpdated(update) => {
                assert!(!update.paused, "the replaced wake must not pause wakeups");
            }
            _ => {}
        }
    }
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_held_before_bind_yields_to_explicit_input_that_bound_first() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    assert_wake_replaced_before_bind_ends_quietly(/*explicit_binds_first*/ true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_held_before_bind_cannot_bind_after_explicit_input_replaced_it() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    assert_wake_replaced_before_bind_ends_quietly(/*explicit_binds_first*/ false).await
}
