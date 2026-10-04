use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_app_server;
use super::*;
use app_test_support::create_fake_parented_rollout_with_source;
use codex_state::SqliteConfig;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn visible_picker_retries_an_empty_preview_after_a_new_completion() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.start_fresh_session(
        &mut tui,
        &mut app_server,
        /*session_start_source*/ None,
        /*initial_user_message*/ None,
        /*new_thread_name*/ None,
    )
    .await;
    let root_thread_id = app.chat_widget.thread_id().expect("root thread started");
    app.start_fresh_session(
        &mut tui,
        &mut app_server,
        /*session_start_source*/ None,
        /*initial_user_message*/ None,
        /*new_thread_name*/ None,
    )
    .await;
    let child_thread_id = app.chat_widget.thread_id().expect("child thread started");
    app.primary_thread_id = Some(root_thread_id);
    app.upsert_agent_picker_thread(
        child_thread_id,
        Some("worker".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.agent_navigation
        .start_picker_preview_generation(root_thread_id);
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(/*selected*/ None));

    app.refresh_agent_picker_previews(&app_server, root_thread_id, vec![child_thread_id]);
    let first_preview_event = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            let event = app_event_rx.recv().await.expect("app event channel");
            if matches!(&event, AppEvent::AgentPickerPreviewsLoaded { .. }) {
                break Ok::<_, color_eyre::eyre::Report>(event);
            }
            Box::pin(app.handle_event(&mut tui, &mut app_server, event)).await?;
        }
    })
    .await??;
    Box::pin(app.handle_event(&mut tui, &mut app_server, first_preview_event)).await?;
    assert!(
        app.agent_navigation
            .picker_details(&child_thread_id)
            .is_none_or(|details| details.response_preview.is_none())
    );

    app.refresh_agent_picker_previews(&app_server, root_thread_id, vec![child_thread_id]);
    assert_eq!(
        recorded_params(&requests, "thread/turns/list")
            .iter()
            .filter(|params| params["threadId"] == child_thread_id.to_string())
            .count(),
        1,
        "discovery in the same open must not retry an empty or failed history read"
    );

    app.enqueue_thread_notification(
        child_thread_id,
        turn_completed_notification(child_thread_id, "later-turn", TurnStatus::Completed),
    )
    .await?;
    let fallback_event = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            let event = app_event_rx.recv().await.expect("app event channel");
            if matches!(&event, AppEvent::AgentPickerPreviewNeeded(_)) {
                break Ok::<_, color_eyre::eyre::Report>(event);
            }
            Box::pin(app.handle_event(&mut tui, &mut app_server, event)).await?;
        }
    })
    .await??;
    Box::pin(app.handle_event(&mut tui, &mut app_server, fallback_event)).await?;
    let second_preview_event = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            let event = app_event_rx.recv().await.expect("app event channel");
            if matches!(&event, AppEvent::AgentPickerPreviewsLoaded { .. }) {
                break Ok::<_, color_eyre::eyre::Report>(event);
            }
            Box::pin(app.handle_event(&mut tui, &mut app_server, event)).await?;
        }
    })
    .await??;
    Box::pin(app.handle_event(&mut tui, &mut app_server, second_preview_event)).await?;

    assert_eq!(
        recorded_params(&requests, "thread/turns/list")
            .iter()
            .filter(|params| params["threadId"] == child_thread_id.to_string())
            .count(),
        2,
        "a distinct visible completion permits one fallback history read"
    );
    assert!(
        recorded_params(&requests, "thread/turns/list")
            .iter()
            .filter(|params| params["threadId"] == child_thread_id.to_string())
            .all(|params| {
                params["limit"] == 20
                    && params["sortDirection"] == "desc"
                    && params["itemsView"] == "full"
            })
    );
    assert!(
        recorded_params(&requests, "thread/resume").is_empty(),
        "preview backfill must not resume an agent"
    );
    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn overlapping_preview_backfills_share_the_sixteen_request_limit() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let root = ThreadId::new();
    let children = (0..17).map(|_| ThreadId::new()).collect::<Vec<_>>();
    app.primary_thread_id = Some(root);
    for child in &children {
        app.upsert_agent_picker_thread(
            *child,
            /*agent_nickname*/ None,
            Some("worker".to_string()),
            /*is_closed*/ false,
        );
    }
    app.agent_navigation.start_picker_preview_generation(root);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let release_gate = Arc::new(Semaphore::new(/*permits*/ 0));
    let active = Arc::new(AtomicUsize::new(/*v*/ 0));
    let max_active = Arc::new(AtomicUsize::new(/*v*/ 0));
    let server_gate = Arc::clone(&release_gate);
    let server_active = Arc::clone(&active);
    let server_max_active = Arc::clone(&max_active);
    let proxy = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let websocket = accept_async(stream).await?;
        let (mut sink, mut source) = websocket.split();
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Message>();
        let writer = tokio::spawn(async move {
            while let Some(message) = write_rx.recv().await {
                sink.send(message).await?;
            }
            Ok::<_, color_eyre::Report>(())
        });
        while let Some(frame) = source.next().await {
            let text = match frame? {
                Message::Text(text) => text,
                Message::Close(_) => break,
                _ => continue,
            };
            let request: serde_json::Value = serde_json::from_str(&text)?;
            let Some(method) = request["method"].as_str() else {
                continue;
            };
            if method == "initialize" {
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": {
                        "userAgent": "codex-tui-test",
                        "codexHome": "/tmp/codex-tui-test"
                    }
                });
                write_tx
                    .send(Message::Text(response.to_string().into()))
                    .expect("test WebSocket writer remains open");
            } else if method == "thread/turns/list" {
                let current = server_active.fetch_add(/*val*/ 1, Ordering::SeqCst) + 1;
                server_max_active.fetch_max(current, Ordering::SeqCst);
                let thread_id = request["params"]["threadId"]
                    .as_str()
                    .expect("history request has a thread id")
                    .to_string();
                observed_tx
                    .send(thread_id)
                    .expect("test receiver remains open");
                let request_id = request["id"].clone();
                let response_tx = write_tx.clone();
                let response_gate = Arc::clone(&server_gate);
                let response_active = Arc::clone(&server_active);
                tokio::spawn(async move {
                    response_gate
                        .acquire()
                        .await
                        .expect("gate remains open")
                        .forget();
                    response_active.fetch_sub(/*val*/ 1, Ordering::SeqCst);
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "result": {
                            "data": [],
                            "nextCursor": null,
                            "backwardsCursor": null
                        }
                    });
                    response_tx
                        .send(Message::Text(response.to_string().into()))
                        .expect("test WebSocket writer remains open");
                });
            }
        }
        drop(write_tx);
        writer.await??;
        Ok::<_, color_eyre::Report>(())
    });
    let client = crate::connect_remote_app_server(crate::RemoteAppServerEndpoint::WebSocket {
        websocket_url,
        auth_token: None,
    })
    .await?;
    let app_server =
        AppServerSession::new(client, crate::app_server_session::ThreadParamsMode::Remote);

    app.refresh_agent_picker_previews(&app_server, root, children[..10].to_vec());
    app.refresh_agent_picker_previews(&app_server, root, children[10..].to_vec());

    for _ in 0..16 {
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), observed_rx.recv())
            .await?
            .expect("history request observed");
    }
    assert_eq!(max_active.load(Ordering::SeqCst), 16);
    assert!(
        tokio::time::timeout(Duration::from_millis(/*millis*/ 100), observed_rx.recv())
            .await
            .is_err()
    );

    release_gate.add_permits(/*n*/ 1);
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), observed_rx.recv())
        .await?
        .expect("seventeenth request starts when one permit is released");
    assert_eq!(max_active.load(Ordering::SeqCst), 16);
    release_gate.add_permits(/*n*/ 16);

    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
            loop {
                if matches!(
                    app_event_rx.recv().await,
                    Some(AppEvent::AgentPickerPreviewsLoaded { .. })
                ) {
                    break;
                }
            }
        })
        .await?;
    }
    app_server.shutdown().await?;
    proxy.await??;
    assert_eq!(active.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn close_agent_archives_child_returns_main_and_preserves_failures() -> Result<()> {
    let (mut app, mut rx, _) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.start_fresh_session(
        &mut tui,
        &mut app_server,
        /*session_start_source*/ None,
        /*initial_user_message*/ None,
        /*new_thread_name*/ None,
    )
    .await;
    let root = app.chat_widget.thread_id().unwrap();
    // A missing backend thread must leave the cached row available after failure.
    let missing = ThreadId::new();
    app.upsert_agent_picker_thread(
        missing,
        Some("missing".into()),
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    while rx.try_recv().is_ok() {}
    app.close_agent_picker_thread(&mut tui, &mut app_server, missing)
        .await?;
    assert!(
        app.agent_navigation
            .visible_threads()
            .iter()
            .any(|(id, _)| *id == missing)
    );
    assert!(next_history_message(&mut rx).contains("Failed to close agent"));
    // Use a saved rollout so archive exercises the public app-server request.
    let child = ThreadId::from_string(
        &create_fake_parented_rollout_with_source(
            &app.config.codex_home,
            "2026-10-03T01-00-00",
            "2026-10-03T01:00:00Z",
            "archive child",
            Some(&app.config.model_provider_id),
            /*git_info*/ None,
            RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("child".into()),
                agent_role: None,
            }),
            root.into(),
            root,
        )
        .expect("child rollout"),
    )?;
    let grandchild = ThreadId::from_string(
        &create_fake_parented_rollout_with_source(
            &app.config.codex_home,
            "2026-10-03T01-00-01",
            "2026-10-03T01:00:01Z",
            "archive grandchild",
            Some(&app.config.model_provider_id),
            /*git_info*/ None,
            RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child,
                depth: 2,
                agent_path: None,
                agent_nickname: Some("grandchild".into()),
                agent_role: None,
            }),
            root.into(),
            child,
        )
        .expect("grandchild rollout"),
    )?;
    app.primary_thread_id = Some(root);
    app.upsert_agent_picker_thread(
        root, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    app.upsert_agent_picker_thread(
        child,
        Some("child".into()),
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    let sibling = ThreadId::from_string(
        &create_fake_parented_rollout_with_source(
            &app.config.codex_home,
            "2026-10-03T01-00-02",
            "2026-10-03T01:00:02Z",
            "archive sibling",
            Some(&app.config.model_provider_id),
            /*git_info*/ None,
            RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("sibling".into()),
                agent_role: None,
            }),
            root.into(),
            root,
        )
        .expect("sibling rollout"),
    )?;
    // Load saved fixtures before navigation, as real spawned children are loaded.
    for id in [child, grandchild, sibling] {
        app_server
            .resume_thread(
                &app.local_settings,
                app.config.clone(),
                id,
                app.resume_model_settings(),
            )
            .await?;
    }
    app.select_agent_thread(&mut tui, &mut app_server, child)
        .await?;
    app.select_agent_thread(&mut tui, &mut app_server, grandchild)
        .await?;
    assert_eq!(app.active_thread_id, Some(grandchild));
    app.upsert_agent_picker_thread(
        sibling,
        Some("sibling".into()),
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    let stale_child = app_server
        .thread_read(child, /*include_turns*/ false)
        .await?;
    app.pending_server_profiles.insert(
        grandchild,
        PermissionProfileSelection {
            profile_id: "server-only".into(),
            approval_policy: None,
            approvals_reviewer: None,
            display_label: "server-only".into(),
        },
    );
    app.close_agent_picker_thread(&mut tui, &mut app_server, child)
        .await?;
    assert_eq!(recorded_params(&requests, "thread/archive").len(), 1);
    assert_eq!(app.active_thread_id, Some(grandchild));
    app.pending_server_profiles.clear();
    app.close_agent_picker_thread(&mut tui, &mut app_server, root)
        .await?;
    assert_eq!(recorded_params(&requests, "thread/archive").len(), 1);
    // Highlight the middle row while another child is displayed.
    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(Some(/*value*/ 2)));
    app.close_agent_picker_thread(&mut tui, &mut app_server, child)
        .await?;
    assert_eq!(recorded_params(&requests, "thread/archive").len(), 2);
    assert_eq!(app.active_thread_id, Some(root));
    assert!(
        !app.agent_navigation
            .visible_threads()
            .iter()
            .any(|(id, _)| *id == child)
    );
    assert_eq!(app.chat_widget.active_view_id(), Some("agent-picker"));
    assert_eq!(
        app.chat_widget
            .selected_index_for_present_view("agent-picker"),
        Some(2)
    );
    let completion = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            let event = rx.recv().await.expect("app event channel");
            if matches!(&event, AppEvent::AgentPickerThreadsLoaded { .. }) {
                break event;
            }
        }
    })
    .await?;
    Box::pin(app.handle_event(&mut tui, &mut app_server, completion)).await?;
    assert!(
        !app.agent_navigation
            .visible_threads()
            .iter()
            .any(|(id, _)| *id == grandchild)
    );
    assert_eq!(
        app.chat_widget
            .selected_index_for_present_view("agent-picker"),
        Some(2),
        "refresh must keep the row when the selected descendant is removed"
    );
    insta::assert_snapshot!(
        "agent_picker_after_subtree_archive",
        crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, /*width*/ 80)
    );
    assert_eq!(
        app.thread_event_channels[&grandchild].attachment(),
        ThreadEventAttachment::ReplayOnly
    );
    assert!(
        !app_server
            .thread_read(child, /*include_turns*/ true)
            .await?
            .turns
            .is_empty()
    );
    let stale_request = app.agent_navigation.begin_picker_refresh(root).unwrap();
    let revisions = app.agent_navigation.status_revision_snapshot();
    app.handle_agent_picker_visibility_notification(
        &app_server,
        &ServerNotification::ThreadArchived(
            codex_app_server_protocol::ThreadArchivedNotification {
                thread_id: child.to_string(),
            },
        ),
    );
    app.apply_agent_picker_thread_refresh(
        &app_server,
        root,
        stale_request,
        revisions,
        Ok(crate::app_event::AgentPickerThreadRefresh {
            threads: vec![stale_child],
            archived_thread_ids: HashSet::new(),
        }),
    );
    assert!(
        !app.agent_navigation
            .visible_threads()
            .iter()
            .any(|(id, _)| *id == child)
    );

    app.chat_widget
        .show_selection_view(app.agent_picker_selection_view_params(Some(/*value*/ 2)));
    app.close_agent_picker_thread(&mut tui, &mut app_server, sibling)
        .await?;
    assert_eq!(
        app.chat_widget
            .selected_index_for_present_view("agent-picker"),
        Some(1),
        "archiving the last row must clamp to the last survivor"
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}
