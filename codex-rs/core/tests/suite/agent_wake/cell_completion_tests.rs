use super::*;
use crate::suite::async_cell_completion_tests::ControlledCells;
use crate::suite::async_cell_completion_tests::ControlledProvider;
use crate::suite::async_cell_completion_tests::configure_controlled_cells;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionCellExecutionLimits;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeSessionProviderFuture;
use codex_code_mode::TerminalReadySupport;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::AgentStatus;
use core_test_support::responses::ev_custom_tool_call;
use pretty_assertions::assert_eq;

const CHILD_CELL_CALL: &str = "child-cell";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_child_cell_completion_reports_through_assignment() -> Result<()> {
    let server = start_mock_server().await;
    let cells = Arc::new(ControlledCells::default());
    let provided = Arc::clone(&cells);
    let test = test_codex()
        .with_model("koffing")
        .with_model_info_override("koffing", |model| {
            model.tool_mode = Some(ToolMode::CodeMode)
        })
        .with_thread_manager(move |manager| {
            manager.with_code_mode_session_provider(Arc::new(ControlledProvider(provided)))
        })
        .with_config(|config| {
            configure_multi_agent(config, Some(AgentPolling::Disabled));
            configure_controlled_cells(config);
        })
        .build_with_auto_env(&server)
        .await?;
    let root = test.session_configured.thread_id;
    mount_spawn_then_status(&server, root).await;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, root_before_report),
        sse(vec![
            ev_response_created("root-idle"),
            ev_assistant_message("root-idle-msg", "worker running"),
            ev_completed("root-idle"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ false,
            |body| !body.contains(CHILD_CELL_CALL),
        ),
        sse(vec![
            ev_response_created("child-launch"),
            ev_custom_tool_call(CHILD_CELL_CALL, "exec", "text('initial'); yield_control();"),
            ev_completed("child-launch"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ false, |body| {
            body.contains(CHILD_CELL_CALL) && !body.contains("async_tool_completion")
        }),
        sse(vec![
            ev_response_created("child-idle"),
            ev_assistant_message("child-idle-msg", "cell running"),
            ev_completed("child-idle"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ false,
            |body| body.contains("async_tool_completion"),
        ),
        sse(vec![
            ev_response_created("child-wake"),
            ev_assistant_message("child-done-msg", "child done"),
            ev_completed("child-wake"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ true,
            |body| body.contains(FINAL_ANSWER),
        ),
        sse(vec![
            ev_response_created("root-wake"),
            ev_assistant_message("root-done-msg", "integrated"),
            ev_completed("root-wake"),
        ]),
    )
    .await;

    test.submit_turn(ROOT_PROMPT).await?;
    let child_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|id| *id != root)
        .expect("root should have spawned a child thread");
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_turn_complete(&child).await;
    // The yielded cell keeps the child's assignment open until its result is reported.
    assert_eq!(child.agent_status().await, AgentStatus::Waiting);
    assert_eq!(cells.executions(), 1);

    cells.finish_all();
    cells.wait_for_read().await;
    wait_for_turn_complete(&child).await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    wait_for_turn_complete(&test.codex).await;

    let bodies = server
        .received_requests()
        .await
        .expect("mock server should record requests")
        .iter()
        .filter_map(request_json)
        .collect::<Vec<_>>();
    let child_bodies = bodies
        .iter()
        .filter(|body| request_thread_id(body) == Some(child_id.to_string().as_str()))
        .collect::<Vec<_>>();
    assert_eq!(child_bodies.len(), 3);
    let terminal = child_bodies[2]["input"]
        .as_array()
        .expect("child request input should be an array")
        .iter()
        .filter_map(|item| item["content"][0]["text"].as_str())
        .filter(|text| text.starts_with("<async_tool_completion>"))
        .collect::<Vec<_>>();
    assert_eq!(terminal.len(), 1);
    assert!(terminal[0].contains(&format!("owner={child_id}")));
    assert!(terminal[0].contains(&format!("call={CHILD_CELL_CALL} kind=cell")));
    assert!(!terminal[0].contains("assignment=none"));
    assert!(terminal[0].contains("controlled-tail"));
    let root_bodies = root_request_bodies(&server, root).await;
    assert_eq!(root_bodies.len(), 3);
    assert_eq!(
        agent_message_texts(&root_bodies[2])
            .iter()
            .filter(|text| text.contains("child done"))
            .count(),
        1
    );
    Ok(())
}

/// Reports readiness at the session level, so the child sees `exec_terminate` rather than `wait`.
struct ReadyControlledProvider(Arc<ControlledCells>);

impl CodeModeSessionProvider for ReadyControlledProvider {
    fn create_session(&self) -> CodeModeSessionProviderFuture<'_> {
        Box::pin(async { Ok(Arc::clone(&self.0) as Arc<dyn CodeModeSession>) })
    }

    fn create_session_with_limits<'a>(
        &'a self,
        _limits: CodeModeSessionCellExecutionLimits,
    ) -> CodeModeSessionProviderFuture<'a> {
        self.create_session()
    }

    fn terminal_ready_capability(&self) -> TerminalReadySupport {
        TerminalReadySupport::Supported
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingReport {
    Final,
    Blocked,
    /// No pending work: the first report is accepted unchanged.
    Idle,
}

const EARLY_REPORT_CALL: &str = "early-report";
const LATE_REPORT_CALL: &str = "late-report";
const CHILD_REPORT: &str = "child report after settled work";

fn report_events(response: &str, call_id: &str, kind: &str) -> Vec<Value> {
    vec![
        ev_response_created(response),
        ev_function_call(
            call_id,
            "report_to_parent",
            &json!({ "kind": kind, "message": CHILD_REPORT }).to_string(),
        ),
        ev_completed(response),
    ]
}

fn function_output_text(body: &Value, call_id: &str) -> Option<String> {
    body["input"].as_array()?.iter().find_map(|item| {
        (item["type"] == "function_call_output" && item["call_id"] == call_id)
            .then(|| item["output"].as_str().map(str::to_owned))
            .flatten()
    })
}

/// A terminal report while a yielded cell is pending is rejected without ending the turn; after
/// the cell settles the same report is accepted and reaches the parent once.
#[test_case::test_case(PendingReport::Final; "final rejected while cell pending")]
#[test_case::test_case(PendingReport::Blocked; "blocked rejected while cell pending")]
#[test_case::test_case(PendingReport::Idle; "final accepted without pending work")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_child_terminal_report_is_rejected_while_it_owns_pending_work(
    case: PendingReport,
) -> Result<()> {
    let kind = if case == PendingReport::Blocked {
        "blocked"
    } else {
        "final"
    };
    let header = if case == PendingReport::Blocked {
        "Message Type: BLOCKED"
    } else {
        FINAL_ANSWER
    };
    let server = start_mock_server().await;
    let cells = Arc::new(ControlledCells::default());
    let provided = Arc::clone(&cells);
    let test = test_codex()
        .with_model("koffing")
        .with_model_info_override("koffing", |model| {
            model.tool_mode = Some(ToolMode::CodeMode)
        })
        .with_thread_manager(move |manager| {
            manager.with_code_mode_session_provider(Arc::new(ReadyControlledProvider(provided)))
        })
        .with_config(|config| {
            configure_multi_agent(config, Some(AgentPolling::Disabled));
            configure_controlled_cells(config);
        })
        .build_with_auto_env(&server)
        .await?;
    let root = test.session_configured.thread_id;
    mount_spawn_then_status(&server, root).await;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, move |body| {
            body.contains(SPAWN_CALL_ID) && !body.contains(header)
        }),
        sse(vec![
            ev_response_created("root-idle"),
            ev_assistant_message("root-idle-msg", "worker running"),
            ev_completed("root-idle"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ true,
            move |body| body.contains(header),
        ),
        sse(vec![
            ev_response_created("root-wake"),
            ev_assistant_message("root-done-msg", "integrated"),
            ev_completed("root-wake"),
        ]),
    )
    .await;
    if case == PendingReport::Idle {
        mount_sse_once_match(
            &server,
            from_thread(root, /*is_root*/ false, |_| true),
            sse(report_events("child-report", LATE_REPORT_CALL, kind)),
        )
        .await;
    } else {
        mount_sse_once_match(
            &server,
            from_thread(
                root,
                /*is_root*/ false,
                |body| !body.contains(CHILD_CELL_CALL),
            ),
            sse(vec![
                ev_response_created("child-launch"),
                ev_custom_tool_call(CHILD_CELL_CALL, "exec", "text('initial'); yield_control();"),
                ev_completed("child-launch"),
            ]),
        )
        .await;
        mount_sse_once_match(
            &server,
            from_thread(root, /*is_root*/ false, |body| {
                body.contains(CHILD_CELL_CALL) && !body.contains(EARLY_REPORT_CALL)
            }),
            sse(report_events("child-early", EARLY_REPORT_CALL, kind)),
        )
        .await;
        mount_sse_once_match(
            &server,
            from_thread(root, /*is_root*/ false, |body| {
                body.contains(EARLY_REPORT_CALL) && !body.contains("async_tool_completion")
            }),
            sse(vec![
                ev_response_created("child-idle"),
                ev_assistant_message("child-idle-msg", "cell running"),
                ev_completed("child-idle"),
            ]),
        )
        .await;
        mount_sse_once_match(
            &server,
            from_thread(
                root,
                /*is_root*/ false,
                |body| body.contains("async_tool_completion"),
            ),
            sse(report_events("child-wake", LATE_REPORT_CALL, kind)),
        )
        .await;
    }

    test.submit_turn(ROOT_PROMPT).await?;
    let child_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|id| *id != root)
        .expect("root should have spawned a child thread");
    let child = test.thread_manager.get_thread(child_id).await?;
    let child_bodies = || async {
        server
            .received_requests()
            .await
            .expect("mock server should record requests")
            .iter()
            .filter_map(request_json)
            .filter(|body| request_thread_id(body) == Some(child_id.to_string().as_str()))
            .collect::<Vec<_>>()
    };
    if case != PendingReport::Idle {
        wait_for_turn_complete(&child).await;
        // The rejected report left the turn running; it ended without a report and now waits.
        assert_eq!(child.agent_status().await, AgentStatus::Waiting);
        let bodies = child_bodies().await;
        assert_eq!(bodies.len(), 3);
        let rejection =
            function_output_text(&bodies[2], EARLY_REPORT_CALL).expect("rejected report output");
        assert_eq!(
            rejection,
            format!(
                "report_to_parent(kind=\"{kind}\") was not recorded: your parent would not receive it until the work below has finished and its results have reached you.\nPending:\n- code cell controlled:cell-0\nTo report, end your turn without calling report_to_parent; you will be woken when this work finishes, and can report then. Or stop it first, then report: `exec_terminate` for code cells."
            )
        );
        assert!(
            root_request_bodies(&server, root)
                .await
                .iter()
                .all(|body| !body.to_string().contains(header)),
            "a rejected report must not reach the parent"
        );

        cells.finish_all();
        cells.wait_for_read().await;
    }
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    wait_for_turn_complete(&test.codex).await;

    let late = child_bodies().await;
    assert_eq!(late.len(), if case == PendingReport::Idle { 1 } else { 4 });
    let root_bodies = root_request_bodies(&server, root).await;
    assert_eq!(root_bodies.len(), 3);
    assert_eq!(
        agent_message_texts(&root_bodies[2])
            .iter()
            .filter(|text| text.contains(CHILD_REPORT))
            .count(),
        1
    );
    Ok(())
}
