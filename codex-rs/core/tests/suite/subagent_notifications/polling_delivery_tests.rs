use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;
use codex_extension_api::ExtensionRegistryBuilder;
use core_test_support::ThreadIdle;

#[path = "polling_read_gate_tests.rs"]
mod read_gate;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeliveryScenario { Recover, ExplicitResumeRace, ExplicitResumeDuringReload, NoCapacity, Archived, Closed, MissingHistory }

#[test_case(DeliveryScenario::Recover; "recover")]
#[test_case(DeliveryScenario::ExplicitResumeRace; "explicit resume race")]
#[test_case(DeliveryScenario::ExplicitResumeDuringReload; "explicit resume during reload")]
#[test_case(DeliveryScenario::NoCapacity; "no capacity")]
#[test_case(DeliveryScenario::Archived; "archived")]
#[test_case(DeliveryScenario::Closed; "closed edge")]
#[test_case(DeliveryScenario::MissingHistory; "missing history")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_completion_delivery_respects_lifecycle(scenario: DeliveryScenario) -> Result<()> {
    let (grandchild_release_tx, grandchild_release_rx) = oneshot::channel();
    let (child_resume_release_tx, child_resume_release_rx) = oneshot::channel();
    let (pressure_release_tx, pressure_release_rx) = oneshot::channel();
    let mut pressure_release_tx = Some(pressure_release_tx);
    let root_spawn_args = serde_json::to_string(&json!({
        "message": "child task marker",
        "task_name": "worker",
        "model": "gpt-5.6-sol",
        "fork_turns": "none",
        "reasoning_effort": "low",
    }))?;
    let child_spawn_args = serde_json::to_string(&json!({
        "message": "grandchild task marker",
        "task_name": "grandchild",
        "model": "gpt-5.5",
        "fork_turns": "none",
        "reasoning_effort": "high",
    }))?;
    let pressure_spawn_args = serde_json::to_string(&json!({
        "message": "capacity pressure task marker",
        "task_name": "pressure",
    }))?;

    let thread_ids = Mutex::new((None::<String>, None::<String>));
    let finished = |id: &str, message: &str| vec![
        streaming_event_chunk(ev_response_created(id)),
        streaming_event_chunk(ev_assistant_message(&format!("{id}-message"), message)),
        streaming_event_chunk(ev_completed(id)),
    ];
    let spawn = |id: &str, call_id: &str, args: &str| vec![
        streaming_event_chunk(ev_response_created(id)),
        streaming_event_chunk(ev_function_call_with_namespace(call_id, MULTI_AGENT_V2_NAMESPACE, "spawn_agent", args)),
        streaming_event_chunk(ev_completed(id)),
    ];
    let (server, _completions) = start_routed_streaming_sse_server(
        vec![
            vec![spawn("polling-root-start", "polling-root-spawn", &root_spawn_args)],
            vec![
                finished("polling-root-after-spawn", "root yielded for child results"),
                spawn("polling-root-pressure", "polling-root-spawn-pressure", &pressure_spawn_args),
                finished("polling-root-pressure-done", "root delegated pressure task"),
            ],
            vec![spawn("polling-child-start", "polling-child-spawn", &child_spawn_args)],
            vec![
                finished("polling-child-after-spawn", "child yielded for grandchild result"),
                vec![
                    streaming_event_chunk(ev_response_created("polling-child-resume")),
                    streaming_event_chunk(ev_assistant_message(
                        "polling-child-final",
                        "worker result marker",
                    )),
                    gated_streaming_chunk(
                        child_resume_release_rx,
                        vec![ev_completed("polling-child-resume")],
                    ),
                ],
            ],
            vec![vec![
                streaming_event_chunk(ev_response_created("polling-grandchild-start")),
                streaming_event_chunk(ev_assistant_message(
                    "polling-grandchild-message",
                    "grandchild result marker",
                )),
                gated_streaming_chunk(
                    grandchild_release_rx,
                    vec![ev_completed("polling-grandchild-start")],
                ),
            ]],
            vec![vec![
                streaming_event_chunk(ev_response_created("polling-pressure-child-start")),
                streaming_event_chunk(ev_assistant_message(
                    "polling-pressure-child-progress",
                    "pressure child is holding its slot",
                )),
                gated_streaming_chunk(
                    pressure_release_rx,
                    vec![
                        ev_assistant_message("polling-pressure-child-result", "pressure child done"),
                        ev_completed("polling-pressure-child-start"),
                    ],
                ),
            ]],
        ],
        move |_headers, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let request_thread_id = body["client_metadata"]["thread_id"].as_str()?;
            let body_text = body.to_string();
            let mut ids = thread_ids.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if ids.0.is_none() && body_text.contains("start the nested polling assignment") {
                ids.0 = Some(request_thread_id.to_string());
                return Some(0);
            }
            if ids.0.as_deref() == Some(request_thread_id) {
                return Some(1);
            }
            if body_text.contains("capacity pressure task marker") {
                return Some(5);
            }
            if ids.1.as_deref() == Some(request_thread_id) {
                return Some(3);
            }
            if body_text.contains("child task marker")
                && !body_text.contains("grandchild task marker")
            {
                ids.1 = Some(request_thread_id.to_string());
                return Some(2);
            }
            if body_text.contains("grandchild task marker") {
                return Some(4);
            }
            None
        },
    )
    .await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let gated_store = Arc::new(read_gate::GatedCompletionReadStore::default());
    let mut builder = test_codex()
        .with_model("koffing")
        .with_extensions(Arc::new(extensions.build()))
        .with_session_source(SessionSource::Cli)
        .with_config(|config| {
            for feature in [Feature::Collab, Feature::MultiAgentV2, Feature::Sqlite] {
                config.features.enable(feature).expect("enable feature");
            }
            config.multi_agent_v2.agent_polling = codex_features::AgentPolling::Enabled;
            config.multi_agent_v2.max_concurrent_threads_per_session = 3;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        });
    if matches!(scenario, DeliveryScenario::ExplicitResumeRace | DeliveryScenario::ExplicitResumeDuringReload) {
        builder = builder.with_thread_store(gated_store.clone());
    }
    let test = builder.build_with_streaming_server(&server).await?;

    let root_thread_id = test.session_configured.thread_id;
    test.submit_turn("start the nested polling assignment").await?;
    ThreadIdle::wait(&test.codex).await;
    let child_thread_id = spawned_thread(&server, "child task marker").await?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let grandchild_thread_id = spawned_thread(&server, "grandchild task marker").await?;
    assert_ne!(grandchild_thread_id, child_thread_id);
    assert_ne!(grandchild_thread_id, root_thread_id);
    wait_for_event(&child_thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    ThreadIdle::wait(&child_thread).await;
    if scenario == DeliveryScenario::Recover {
        child_thread.update_thread_settings(ThreadSettingsOverrides {
            permission_profile: Some(PermissionProfile::read_only()), ..Default::default()
        }).await?;
    }
    let original_config = child_thread.config_snapshot().await;
    drop(child_thread);

    test.submit_text_turn("start capacity pressure nested polling")
        .await?;
    ThreadIdle::wait(&test.codex).await;
    let pressure_child_thread_id = spawned_thread(&server, "capacity pressure task marker").await?;
    let pressure_child = test
        .thread_manager
        .get_thread(pressure_child_thread_id)
        .await?;
    let grandchild_thread = test.thread_manager.get_thread(grandchild_thread_id).await?;
    assert_eq!(pressure_child.agent_status().await, AgentStatus::Running);
    assert_eq!(grandchild_thread.agent_status().await, AgentStatus::Running);
    assert!(
        test.thread_manager
            .get_thread(child_thread_id)
            .await
            .is_err(),
        "capacity pressure should evict the idle Completed parent"
    );
    if scenario != DeliveryScenario::NoCapacity {
        // The finishing grandchild itself cannot be evicted during its completion callback.
        let _ = pressure_release_tx.take().expect("pressure gate").send(());
        wait_for_event(&pressure_child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        ThreadIdle::wait(&pressure_child).await;
        pressure_child.shutdown_and_wait().await?;
        test.thread_manager.remove_thread(&pressure_child_thread_id).await;
    }
    match scenario {
        DeliveryScenario::Archived => {
            test.thread_store.archive_thread(codex_thread_store::ArchiveThreadParams { thread_id: child_thread_id }).await?;
        }
        DeliveryScenario::Closed => {
            let db = codex_core::init_state_db(&test.config).await;
            codex_core::local_agent_graph_store_from_state_db(db.as_ref()).expect("graph store")
                .set_thread_spawn_edge_status(child_thread_id, codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed).await?;
        }
        DeliveryScenario::MissingHistory => {
            test.thread_store.delete_thread(codex_thread_store::DeleteThreadParams { thread_id: child_thread_id }).await?;
        }
        DeliveryScenario::Recover | DeliveryScenario::ExplicitResumeRace | DeliveryScenario::ExplicitResumeDuringReload | DeliveryScenario::NoCapacity => {}
    }
    let mut created = test.thread_manager.subscribe_thread_created();
    let explicit_resume = matches!(scenario, DeliveryScenario::ExplicitResumeRace | DeliveryScenario::ExplicitResumeDuringReload).then(|| {
        let (started, recovery_started) = oneshot::channel();
        let (release, recovery_release) = oneshot::channel();
        *gated_store.gate.lock().expect("arm recovery read") = Some(read_gate::ReadGate {
            thread_id: child_thread_id,
            include_archived: scenario == DeliveryScenario::ExplicitResumeDuringReload,
            started, release: recovery_release,
        });
        let manager = Arc::clone(&test.thread_manager);
        tokio::spawn(async move {
            timeout(Duration::from_secs(5), recovery_started).await??;
            manager.ensure_multi_agent_v2_child_loaded(child_thread_id).await?;
            release.send(()).expect("release paused completion");
            Ok::<_, anyhow::Error>(())
        })
    });
    // Register while the turn is active; idle then follows delivery or its warning.
    tokio::join!(ThreadIdle::wait(&grandchild_thread), async {
        let _ = grandchild_release_tx.send(());
    });
    if !matches!(scenario, DeliveryScenario::Recover | DeliveryScenario::ExplicitResumeRace | DeliveryScenario::ExplicitResumeDuringReload) {
        wait_for_event(&grandchild_thread, |event| matches!(event, EventMsg::Warning(warning) if warning.message.contains("Could not deliver completion to parent"))).await;
        assert!(test.thread_manager.get_thread(child_thread_id).await.is_err());
        grandchild_thread.shutdown_and_wait().await?;
        if let Some(release) = pressure_release_tx.take() { let _ = release.send(()); }
        let _ = test.codex.shutdown_and_wait().await;
        server.shutdown().await;
        return Ok(());
    }
    let recovered_id = timeout(Duration::from_secs(5), created.recv()).await??;
    assert_eq!(recovered_id, child_thread_id);
    if let Some(explicit_resume) = explicit_resume { explicit_resume.await??; }
    let reloaded_child = test.thread_manager.get_thread(child_thread_id).await?;
    let recovered_config = reloaded_child.config_snapshot().await;
    assert_eq!(
        (&recovered_config.model, &recovered_config.reasoning_effort, &recovered_config.service_tier,
         &recovered_config.approval_policy, &recovered_config.permission_profile, selected_environments(&recovered_config)),
        (&original_config.model, &original_config.reasoning_effort, &original_config.service_tier,
         &original_config.approval_policy, &original_config.permission_profile, selected_environments(&original_config)),
    );
    grandchild_thread.shutdown_and_wait().await?;
    let before_resume = server.requests().await;
    assert_eq!(before_resume.iter().filter_map(|request| serde_json::from_slice::<Value>(request).ok()).filter(|request| streaming_request_has_thread_id(request, child_thread_id)).count(), 2);
    reloaded_child.start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
        text: "explicit polling continuation".into(), text_elements: vec![],
    }])).await?;
    let child_final_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, child_thread_id)
            && streaming_request_has_input_type_with_text(
                request,
                "agent_message",
                "grandchild result marker",
            )
    })
    .await
    .context("waiting for the reloaded child's report-processing request")?;
    let _ = child_resume_release_tx.send(());
    let child_report_count = child_final_request["input"]
        .as_array()
        .expect("child resumed request has model input")
        .iter()
        .filter(|item| {
            item["type"] == "agent_message" && item.to_string().contains("grandchild result marker")
        })
        .count();
    assert_eq!(child_report_count, 1);
    let report = child_final_request["input"].as_array().expect("child model input").iter()
        .find(|item| item["type"] == "agent_message" && item.to_string().contains("grandchild result marker"))
        .expect("accepted completion report");
    assert!(report["id"].as_str().expect("completion identity").starts_with("msg_"));

    wait_for_event(&reloaded_child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    ThreadIdle::wait(&reloaded_child).await;
    let _ = test.codex.shutdown_and_wait().await;
    server.shutdown().await;
    Ok(())
}

async fn spawned_thread(server: &StreamingSseServer, marker: &str) -> Result<ThreadId> {
    let request = wait_for_streaming_request_matching(server, |request| {
        streaming_request_has_input_type_with_text(request, "agent_message", marker)
    }).await?;
    let id = ThreadId::from_string(request["client_metadata"]["thread_id"].as_str().expect("spawned thread ID"))?;
    Ok(id)
}

// Ready and FromThread are different representations of the same selected executor. Compare
// its identity and effective permissions so explicit resume may materialize a ready config.
fn selected_environments(snapshot: &codex_core::ThreadConfigSnapshot) -> Vec<Value> {
    snapshot.environments.environments.iter().map(|environment| {
        let permissions = match &environment.config {
            codex_protocol::protocol::EnvironmentConfigState::Ready(config) => config.permission_profile.permission_profile(),
            codex_protocol::protocol::EnvironmentConfigState::FromThread => &snapshot.permission_profile,
            codex_protocol::protocol::EnvironmentConfigState::Pending
            | codex_protocol::protocol::EnvironmentConfigState::Failed(_) => panic!("test executor must be ready"),
        };
        json!({ "id": environment.environment_id, "cwd": environment.cwd,
            "workspace_roots": environment.workspace_roots, "permission_profile": permissions })
    }).collect()
}
