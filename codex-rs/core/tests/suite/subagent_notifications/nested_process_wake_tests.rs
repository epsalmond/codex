//! Compose native completion, nested reports, and Exec drain in one uninterrupted trace.

use super::*;
use crate::suite::work_lifecycle_producers::CALL_ID;
use crate::suite::work_lifecycle_producers::NativeGate;
use crate::suite::work_lifecycle_producers::assert_close_rejected;
use crate::suite::work_lifecycle_producers::close_drained_tree;
use crate::suite::work_lifecycle_producers::completion_texts;
use crate::suite::work_lifecycle_producers::observe;
use crate::suite::work_lifecycle_producers::requests;
use crate::suite::work_lifecycle_producers::terminal_turn;
use crate::suite::work_lifecycle_producers::yielded_output;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_protocol::protocol::MultiAgentVersion;
use core_test_support::ThreadIdle;
use core_test_support::process::process_is_alive;
use pretty_assertions::assert_eq;
use tokio::io::AsyncWriteExt;

const START: &str = "start composed process wake trace";
const CHILD_TASK: &str = "composed child assignment";
const GRANDCHILD_TASK: &str = "composed grandchild assignment";
const GRANDCHILD_RESULT: &str = "grandchild accepted terminal-native-tail";
const CHILD_RESULT: &str = "child accepted terminal-native-tail";
const ROOT_RESULT: &str = "root accepted terminal-native-tail";

fn message(id: &str, text: &str) -> Vec<Value> {
    vec![
        ev_response_created(id),
        ev_assistant_message(id, text),
        ev_completed(id),
    ]
}

fn events(events: Vec<Value>) -> Vec<StreamingSseChunk> {
    vec![StreamingSseChunk {
        gate: None,
        body: sse(events),
    }]
}

fn report_count(request: &Value, text: &str) -> usize {
    request["input"]
        .as_array()
        .expect("model input")
        .iter()
        .filter(|item| item["type"] == "agent_message" && item.to_string().contains(text))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_process_completion_wakes_grandchild_child_and_root_then_drains() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let gate = NativeGate::new().await?;
    let (accept_process, process_acceptance) = oneshot::channel();
    let (finish_grandchild, grandchild_final) = oneshot::channel();
    let (accept_grandchild, grandchild_acceptance) = oneshot::channel();
    let (accept_child, child_acceptance) = oneshot::channel();
    let spawn = |id, task, name| {
        events(vec![
            ev_response_created(id),
            ev_function_call_with_namespace(
                id,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &json!({"message": task, "task_name": name}).to_string(),
            ),
            ev_completed(id),
        ])
    };
    let routing = Arc::new(Mutex::new((None::<String>, None::<String>)));
    let (server, _) = start_routed_streaming_sse_server(
        vec![
            vec![
                spawn("composed-root-spawn", CHILD_TASK, "worker"),
                events(message("composed-root-yield", "root yielded for child")),
                vec![gated_streaming_chunk(
                    child_acceptance,
                    message("composed-root-final", ROOT_RESULT),
                )],
            ],
            vec![
                spawn("composed-child-spawn", GRANDCHILD_TASK, "grandchild"),
                events(message(
                    "composed-child-yield",
                    "child yielded for grandchild",
                )),
                vec![gated_streaming_chunk(
                    grandchild_acceptance,
                    message("composed-child-final", CHILD_RESULT),
                )],
            ],
            vec![
                events(vec![
                    ev_response_created("composed-process-start"),
                    ev_function_call(
                        CALL_ID,
                        "exec_command",
                        &gate.arguments(false, true, 0)?.to_string(),
                    ),
                    ev_completed("composed-process-start"),
                ]),
                events(message(
                    "composed-grandchild-yield",
                    "grandchild yielded for process",
                )),
                vec![
                    gated_streaming_chunk(
                        process_acceptance,
                        vec![ev_response_created("composed-grandchild-resume")],
                    ),
                    gated_streaming_chunk(
                        grandchild_final,
                        vec![
                            ev_assistant_message("composed-grandchild-final", GRANDCHILD_RESULT),
                            ev_completed("composed-grandchild-resume"),
                        ],
                    ),
                ],
            ],
        ],
        move |_headers, body| {
            let body: Value = serde_json::from_slice(body).ok()?;
            let id = body["client_metadata"]["thread_id"].as_str()?;
            let text = body.to_string();
            let mut ids = routing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if ids.0.is_none() && text.contains(START) {
                ids.0 = Some(id.to_owned());
            }
            if ids.0.as_deref() == Some(id) {
                return Some(0);
            }
            if ids.1.is_none() && text.contains(CHILD_TASK) && !text.contains(GRANDCHILD_TASK) {
                ids.1 = Some(id.to_owned());
            }
            if ids.1.as_deref() == Some(id) {
                return Some(1);
            }
            text.contains(GRANDCHILD_TASK).then_some(2)
        },
    )
    .await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_session_source(SessionSource::Exec)
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.multi_agent_version = Some(MultiAgentVersion::V2);
            model.tool_mode = Some(ToolMode::Direct);
        })
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            for feature in [
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::UnifiedExec,
                Feature::AsyncProcessCompletion,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("enable trace feature");
            }
            config
                .permissions
                .set_permission_profile(PermissionProfile::Disabled)
                .expect("local socket gate");
            config.multi_agent_v2.agent_polling = AgentPolling::Disabled;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_streaming_server(&server)
        .await?;
    let root_id = test.session_configured.thread_id;
    let observation = test
        .codex
        .work_observation()
        .context("Exec work observation")?;
    let (_, mut updates) = observation.subscribe();
    crate::suite::unified_exec::submit_unified_exec_turn(&test, START, PermissionProfile::Disabled)
        .await?;
    let (pid, mut stream) = gate.connected().await?;
    let initial = terminal_turn(&test).await?;
    assert!(initial.error.is_none(), "{initial:?}");
    ThreadIdle::wait(&test.codex).await;
    test.codex.note_root_turn_output_forwarded(&initial.turn_id);

    let child_request = wait_for_streaming_request_matching(&server, |request| {
        !streaming_request_has_thread_id(request, root_id)
            && streaming_request_has_input_type_with_text(request, "agent_message", CHILD_TASK)
    })
    .await?;
    let grandchild_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_input_type_with_text(request, "agent_message", GRANDCHILD_TASK)
    })
    .await?;
    let thread_id = |request: &Value| {
        ThreadId::from_string(
            request["client_metadata"]["thread_id"]
                .as_str()
                .expect("thread ID"),
        )
    };
    let child_id = thread_id(&child_request)?;
    let grandchild_id = thread_id(&grandchild_request)?;
    assert_ne!(root_id, child_id);
    assert_ne!(child_id, grandchild_id);
    assert_ne!(root_id, grandchild_id);
    let child = test.thread_manager.get_thread(child_id).await?;
    let grandchild = test.thread_manager.get_thread(grandchild_id).await?;
    ThreadIdle::wait(&child).await;
    ThreadIdle::wait(&grandchild).await;
    let grandchild_turn = wait_for_event_match(&grandchild, |event| match event {
        EventMsg::TurnComplete(turn) => Some(turn.clone()),
        _ => None,
    })
    .await;
    for thread in [&test.codex, &child, &grandchild] {
        assert_eq!(thread.agent_status().await, AgentStatus::Waiting);
    }
    let running = assert_close_rejected(&observation);
    assert_eq!(running.active_root_turns, 0);
    assert_eq!(running.pending_terminal_outputs, 0);
    assert!(running.running_finite_work > 0, "{running:?}");
    assert!(process_is_alive(&pid)?);
    let initial_requests = requests(&server).await?;
    assert_eq!(initial_requests.len(), 6, "two requests per yielded agent");
    assert!(
        initial_requests
            .iter()
            .all(|request| report_count(request, "Message Type: FINAL_ANSWER") == 0)
    );
    let yielded = initial_requests
        .iter()
        .find(|request| {
            streaming_request_has_thread_id(request, grandchild_id)
                && request["input"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["call_id"] == CALL_ID && item["type"] == "function_call_output"
                    })
                })
        })
        .context("grandchild receives its yielded process handle")?;
    let output = yielded_output(yielded);
    let process_id = output
        .lines()
        .find_map(|line| line.strip_prefix("Process running with session ID "))
        .context("yielded process ID")?;

    stream.get_mut().write_all(b"finish\n").await?;
    let process_request = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, grandchild_id)
            && !completion_texts(request).is_empty()
    })
    .await?;
    let receipts = completion_texts(&process_request);
    assert_eq!(
        receipts.len(),
        1,
        "one native terminal result wakes grandchild"
    );
    let receipt = receipts[0];
    assert!(serde_json::to_vec(&json!({"text": receipt}))?.len() <= 3072);
    for identity in [
        format!("call={CALL_ID}"),
        format!("owner={grandchild_id}"),
        format!("turn={}", grandchild_turn.turn_id),
        format!("process={process_id}"),
        "kind=process".to_owned(),
        "status=exited(0)".to_owned(),
        "terminal-native-tail".to_owned(),
    ] {
        assert!(receipt.contains(&identity), "missing {identity}: {receipt}");
    }
    let retained = assert_close_rejected(&observation);
    assert!(retained.pending_notifications > 0, "{retained:?}");
    accept_process.send(()).expect("accept the native result");
    observe(&mut updates, |snapshot| snapshot.pending_notifications == 0).await?;
    assert_close_rejected(&observation);
    finish_grandchild
        .send(())
        .expect("grandchild reports its accepted result");

    let child_wake = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, child_id)
            && report_count(request, GRANDCHILD_RESULT) > 0
    })
    .await?;
    assert_eq!(report_count(&child_wake, GRANDCHILD_RESULT), 1);
    assert_eq!(
        report_count(&child_wake, "grandchild yielded for process"),
        0
    );
    assert_eq!(
        requests(&server).await?.len(),
        8,
        "child wake follows process acceptance"
    );
    assert_close_rejected(&observation);
    accept_grandchild
        .send(())
        .expect("child accepts grandchild report");
    let root_wake = wait_for_streaming_request_matching(&server, |request| {
        streaming_request_has_thread_id(request, root_id) && report_count(request, CHILD_RESULT) > 0
    })
    .await?;
    assert_eq!(report_count(&root_wake, CHILD_RESULT), 1);
    assert_eq!(report_count(&root_wake, "child yielded for grandchild"), 0);
    assert_close_rejected(&observation);
    accept_child.send(()).expect("root accepts child report");
    let final_turn = terminal_turn(&test).await?;
    assert!(final_turn.error.is_none(), "{final_turn:?}");
    assert_eq!(final_turn.last_agent_message.as_deref(), Some(ROOT_RESULT));
    assert_ne!(initial.turn_id, final_turn.turn_id);
    let awaiting_output = assert_close_rejected(&observation);
    assert_eq!(awaiting_output.pending_terminal_outputs, 1);
    test.codex
        .note_root_turn_output_forwarded(&final_turn.turn_id);
    let drained = observe(&mut updates, |snapshot| snapshot.quiescent).await?;
    assert_eq!(drained.outstanding_work, 0);
    close_drained_tree(&observation, drained);
    test.codex.shutdown_and_wait().await?;

    let all_requests = requests(&server).await?;
    assert_eq!(all_requests.len(), 9, "no duplicate or empty wake turns");
    assert_eq!(
        all_requests.iter().flat_map(completion_texts).count(),
        1,
        "terminal result is delivered once across the complete trace"
    );
    for id in [root_id, child_id, grandchild_id] {
        assert_eq!(
            all_requests
                .iter()
                .filter(|request| streaming_request_has_thread_id(request, id))
                .count(),
            3
        );
    }
    for request in &all_requests {
        assert!(
            namespace_child_tool(request, MULTI_AGENT_V2_NAMESPACE, "wait_agent").is_none(),
            "model polling is disabled"
        );
        assert!(
            request["input"]
                .as_array()
                .expect("model input")
                .iter()
                .all(|item| {
                    item["type"] != "function_call"
                        || !matches!(
                            item["name"].as_str(),
                            Some("write_stdin" | "wait_agent" | "list_agents" | "wait")
                        )
                }),
            "no model polling calls"
        );
    }
    assert!(!process_is_alive(&pid)?);
    server.shutdown().await;
    Ok(())
}
