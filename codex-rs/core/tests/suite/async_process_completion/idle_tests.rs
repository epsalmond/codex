use super::*;
use codex_protocol::protocol::AgentStatus;
use core_test_support::test_codex::TestCodex;
use pretty_assertions::assert_eq;

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
