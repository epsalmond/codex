use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_app_server;
use super::*;
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
    app.start_fresh_session_with_summary_hint(
        &mut tui,
        &mut app_server,
        /*session_start_source*/ None,
        /*initial_user_message*/ None,
        /*new_thread_name*/ None,
    )
    .await;
    let root_thread_id = app.chat_widget.thread_id().expect("root thread started");
    app.start_fresh_session_with_summary_hint(
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
        let (sink, mut source) = websocket.split();
        let sink = Arc::new(tokio::sync::Mutex::new(sink));
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
                sink.lock()
                    .await
                    .send(Message::Text(response.to_string().into()))
                    .await?;
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
                let response_sink = Arc::clone(&sink);
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
                    response_sink
                        .lock()
                        .await
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .expect("websocket remains open");
                });
            }
        }
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
