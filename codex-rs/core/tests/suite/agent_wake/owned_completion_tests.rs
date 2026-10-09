use super::*;
use codex_protocol::protocol::AgentStatus;
use core_test_support::TestTargetOs;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(false; "live_producer")]
#[test_case(true; "pending_terminal")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_retains_owned_helper_until_one_terminal_report(evict_pending: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine is unavailable"
    );
    let server = start_mock_server().await;
    let test = test_codex()
        .with_model("koffing")
        .with_config(|config| {
            configure_multi_agent(config, Some(AgentPolling::Disabled));
            config
                .features
                .enable(Feature::AsyncProcessCompletion)
                .unwrap();
            config.multi_agent_v2.max_concurrent_threads_per_session = 2;
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
    let cmd = match test_target_os() {
        TestTargetOs::Windows => {
            "for ($i=0; $i -lt 300; $i++) { if (Test-Path async-release) { echo child-terminal; exit 0 }; Start-Sleep -Milliseconds 50 }; exit 9"
        }
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            "for i in {1..300}; do if test -f async-release; then echo child-terminal; exit 0; fi; sleep 0.05; done; exit 9"
        }
    };
    mount_sse_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ false,
            |body| !body.contains("child-finite"),
        ),
        sse(vec![
            ev_response_created("child-launch"),
            ev_function_call(
                "child-finite",
                "exec_command",
                &json!({"cmd":cmd, "yield_time_ms":250}).to_string(),
            ),
            ev_completed("child-launch"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ false, |body| {
            body.contains("child-finite") && !body.contains("async_tool_completion")
        }),
        sse(vec![
            ev_response_created("child-idle"),
            ev_assistant_message("child-idle-msg", "helper running"),
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
    mount_sse_once_match(
        &server,
        from_thread(root, /*is_root*/ true, |body| {
            body.contains("pressure") && !body.contains("pressure-spawn")
        }),
        sse(vec![
            ev_response_created("pressure"),
            ev_function_call_with_namespace(
                "pressure-spawn",
                NAMESPACE,
                "spawn_agent",
                &json!({"message":"pressure task", "task_name":"pressure"}).to_string(),
            ),
            ev_completed("pressure"),
        ]),
    )
    .await;
    let pressure = mount_sse_once_match(
        &server,
        from_thread(
            root,
            /*is_root*/ true,
            |body| body.contains("pressure-spawn"),
        ),
        sse(vec![
            ev_response_created("pressure-result"),
            ev_assistant_message("pressure-msg", "pressure observed"),
            ev_completed("pressure-result"),
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
        .unwrap();
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_turn_complete(&child).await;
    assert_eq!(child.agent_status().await, AgentStatus::Waiting);
    assert_eq!(root_request_bodies(&server, root).await.len(), 2);
    if evict_pending {
        child.submit(Op::Interrupt).await?;
        wait_for_event(
            &child,
            |event| matches!(event, EventMsg::AgentWakeupsUpdated(update) if update.paused),
        )
        .await;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnStarted(_))
        })
        .await;
        wait_for_turn_complete(&test.codex).await;
    }
    // Residency pressure must not shut down the owner of its in-memory producer/store.
    if !evict_pending {
        test.submit_text_turn("pressure").await?;
        assert!(test.thread_manager.get_thread(child_id).await.is_ok());
    }
    test.fs()
        .write_file(
            &test.workspace_path_uri("async-release")?,
            Vec::new(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    if evict_pending {
        wait_for_event(
            &child,
            |event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == "child-finite"),
        )
        .await;
        test.submit_text_turn("pressure").await?;
        assert!(test.thread_manager.get_thread(child_id).await.is_ok());
        assert!(
            pressure
                .single_request()
                .function_call_output("pressure-spawn")
                .to_string()
                .contains("limit")
        );
        // A paused child has a terminal assignment; an explicit parent task starts new work.
        child.shutdown_and_wait().await?;
        test.codex.shutdown_and_wait().await?;
        return Ok(());
    }
    wait_for_event(
        &child,
        |event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == "child-finite"),
    )
    .await;
    assert_eq!(child.agent_status().await, AgentStatus::Waiting);
    child
        .start_or_steer_turn(codex_core::TurnInputRequest::user_input(vec![
            UserInput::Text {
                text: "accept owned result".to_owned(),
                text_elements: Vec::new(),
            },
        ]))
        .await?;
    wait_for_turn_complete(&child).await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    wait_for_turn_complete(&test.codex).await;
    let bodies = server
        .received_requests()
        .await
        .unwrap()
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
        .unwrap()
        .iter()
        .find(|item| {
            item["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("<async_tool_completion>"))
        })
        .unwrap();
    assert!(terminal.to_string().contains("child-terminal"));
    assert!(terminal.to_string().contains(&child_id.to_string()));
    let root_bodies = root_request_bodies(&server, root).await;
    assert_eq!(root_bodies.len(), 5);
    assert_eq!(
        agent_message_texts(&root_bodies[4])
            .iter()
            .filter(|text| text.contains("child done"))
            .count(),
        1
    );
    assert!(
        pressure
            .single_request()
            .function_call_output("pressure-spawn")
            .to_string()
            .contains("limit")
    );
    Ok(())
}
