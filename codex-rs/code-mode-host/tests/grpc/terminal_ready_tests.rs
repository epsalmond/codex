use super::*;
use codex_code_mode::CellOutputKind;
use codex_code_mode::CellTerminalReady;
use codex_code_mode::CellTerminalStatus;
use codex_code_mode::TerminalReadySupport;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
enum Event {
    Ready(CellTerminalReady),
    Notification,
    Closed(CellId),
}

struct Delegate {
    events: mpsc::UnboundedSender<Event>,
    notifications: Semaphore,
}

impl CodeModeSessionDelegate for Delegate {
    fn invoke_tool<'a>(
        &'a self,
        _call: CodeModeNestedToolCall,
        _cancel: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { Ok(json!(null)) })
    }

    fn notify<'a>(
        &'a self,
        _call: String,
        _cell: CellId,
        _text: String,
        cancel: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            self.events.send(Event::Notification).unwrap();
            tokio::select! {
                permit = self.notifications.acquire() => { permit.unwrap().forget(); }
                _ = cancel.cancelled() => {}
            }
            Ok(())
        })
    }

    fn cell_terminal_ready(&self, ready: CellTerminalReady) {
        self.events.send(Event::Ready(ready)).unwrap();
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.events.send(Event::Closed(cell_id.clone())).unwrap();
    }
}

async fn next(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
    timeout(TEST_TIMEOUT, events.recv()).await.unwrap().unwrap()
}

#[tokio::test]
async fn terminal_ready_before_initial_yield_preserves_both_intervals_and_one_wait() -> Result<()> {
    let host = HostHarness::start("grpc://127.0.0.1:0").await?;
    let session = GrpcCodeModeSessionProvider::new(host.endpoint)
        .create_session()
        .await
        .map_err(anyhow::Error::msg)?;
    let (events, mut receiver) = mpsc::unbounded_channel();
    let delegate = Arc::new(Delegate {
        events,
        notifications: Semaphore::new(/*permits*/ 0),
    });
    for (tail, status, kind) in [
        (
            r#"text("after");"#,
            CellTerminalStatus::Completed,
            CellOutputKind::Text,
        ),
        (
            r#"text("after"); throw new Error("boom");"#,
            CellTerminalStatus::Error,
            CellOutputKind::Text,
        ),
        (
            r#"image("data:image/png;base64,aW1hZ2U="); audio("data:audio/wav;base64,YXVkaW8=");"#,
            CellTerminalStatus::Completed,
            CellOutputKind::Media,
        ),
    ] {
        let started = session
            .execute(
                request(&format!(r#"text("before"); yield_control(); {tail}"#)),
                delegate.clone(),
                /*preempt*/ None,
            )
            .await
            .map_err(anyhow::Error::msg)?;
        let id = started.cell_id.clone();
        assert_eq!(
            started.terminal_ready_support,
            TerminalReadySupport::Supported
        );
        // Deliberately receive readiness before consuming the initial response.
        assert_eq!(
            next(&mut receiver).await,
            Event::Ready(CellTerminalReady {
                cell_id: id.clone(),
                status,
                output_kind: kind
            })
        );
        let initial = started
            .initial_response()
            .await
            .map_err(anyhow::Error::msg)?
            .with_code_mode_host_duration(Duration::ZERO);
        assert_eq!(
            initial,
            RuntimeResponse::Yielded {
                cell_id: id.clone(),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "before".into()
                }],
                code_mode_host_duration: Some(Duration::ZERO)
            }
        );
        let outcome = session
            .wait(
                WaitRequest {
                    cell_id: id.clone(),
                    yield_time_ms: 1,
                },
                /*preempt*/ None,
            )
            .await
            .map_err(anyhow::Error::msg)?;
        let WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id,
            content_items,
            error_text,
            ..
        }) = outcome
        else {
            anyhow::bail!("one post-ready wait must return the terminal result");
        };
        assert_eq!(cell_id, id);
        if kind == CellOutputKind::Media {
            assert_eq!(
                content_items,
                vec![
                    FunctionCallOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,aW1hZ2U=".into(),
                        detail: Some(codex_code_mode::ImageDetail::High)
                    },
                    FunctionCallOutputContentItem::InputAudio {
                        audio_url: "data:audio/wav;base64,YXVkaW8=".into()
                    },
                ]
            );
        } else {
            assert_eq!(
                content_items,
                vec![FunctionCallOutputContentItem::InputText {
                    text: "after".into()
                }]
            );
        }
        assert_eq!(error_text.is_some(), status == CellTerminalStatus::Error);
        if let Some(error) = error_text {
            assert!(error.contains("boom"));
        }
        assert_eq!(next(&mut receiver).await, Event::Closed(id));
        assert!(receiver.try_recv().is_err());
    }
    session.shutdown().await.map_err(anyhow::Error::msg)?;
    Ok(())
}

#[tokio::test]
async fn terminal_ready_settles_notifications_and_skips_inline_and_termination() -> Result<()> {
    let host = HostHarness::start("grpc://127.0.0.1:0").await?;
    let session = GrpcCodeModeSessionProvider::new(host.endpoint)
        .create_session()
        .await
        .map_err(anyhow::Error::msg)?;
    let (events, mut receiver) = mpsc::unbounded_channel();
    let delegate = Arc::new(Delegate {
        events,
        notifications: Semaphore::new(/*permits*/ 0),
    });
    let started = session
        .execute(
            request(r#"yield_control(); notify("hold"); text("done");"#),
            delegate.clone(),
            /*preempt*/ None,
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let id = started.cell_id.clone();
    assert!(matches!(
        started
            .initial_response()
            .await
            .map_err(anyhow::Error::msg)?,
        RuntimeResponse::Yielded { .. }
    ));
    assert_eq!(next(&mut receiver).await, Event::Notification);
    // A second execution proves the session event stream progressed while the
    // first delegate is held; readiness must still be latched for that delegate.
    let inline = session
        .execute(
            request(r#"text("inline");"#),
            delegate.clone(),
            /*preempt*/ None,
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let inline_id = inline.cell_id.clone();
    assert!(matches!(
        inline
            .initial_response()
            .await
            .map_err(anyhow::Error::msg)?,
        RuntimeResponse::Result { .. }
    ));
    assert_eq!(next(&mut receiver).await, Event::Closed(inline_id));
    delegate.notifications.add_permits(/*n*/ 1);
    assert_eq!(
        next(&mut receiver).await,
        Event::Ready(CellTerminalReady {
            cell_id: id.clone(),
            status: CellTerminalStatus::Completed,
            output_kind: CellOutputKind::Text
        })
    );
    let observation = WaitRequest {
        cell_id: id.clone(),
        yield_time_ms: 1,
    };
    let (first, second) = tokio::join!(
        session.wait(observation.clone(), /*preempt*/ None),
        session.wait(observation, /*preempt*/ None),
    );
    assert_eq!(
        [first, second]
            .into_iter()
            .filter(|outcome| matches!(
                outcome,
                Ok(WaitOutcome::LiveCell(RuntimeResponse::Result { .. }))
            ))
            .count(),
        1
    );
    assert_eq!(next(&mut receiver).await, Event::Closed(id));

    let started = session
        .execute(
            request("yield_control(); await new Promise(() => {});"),
            delegate.clone(),
            /*preempt*/ None,
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let id = started.cell_id.clone();
    started
        .initial_response()
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(matches!(
        session
            .terminate(id.clone())
            .await
            .map_err(anyhow::Error::msg)?,
        WaitOutcome::LiveCell(RuntimeResponse::Terminated { .. })
    ));
    assert_eq!(next(&mut receiver).await, Event::Closed(id));

    let running = session
        .execute(
            request("yield_control(); await new Promise(() => {});"),
            delegate,
            /*preempt*/ None,
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let running_id = running.cell_id.clone();
    running
        .initial_response()
        .await
        .map_err(anyhow::Error::msg)?;
    session.shutdown().await.map_err(anyhow::Error::msg)?;
    assert_eq!(next(&mut receiver).await, Event::Closed(running_id.clone()));
    assert!(
        session
            .wait(
                WaitRequest {
                    cell_id: running_id,
                    yield_time_ms: 1
                },
                /*preempt*/ None
            )
            .await
            .is_err()
    );
    assert!(receiver.try_recv().is_err());
    Ok(())
}
