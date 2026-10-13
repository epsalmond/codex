use super::*;
use codex_code_mode::*;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::streaming_sse::start_routed_streaming_sse_server;
use pretty_assertions::assert_eq;
use std::sync::Mutex;
use test_case::test_case;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct ControlledCell {
    delegate: Mutex<Option<Arc<dyn CodeModeSessionDelegate>>>,
    terminating: tokio::sync::Notify,
    release_termination: tokio::sync::Notify,
}

struct ControlledProvider(Arc<ControlledCell>);
impl CodeModeSessionProvider for ControlledProvider {
    fn create_session(&self) -> CodeModeSessionProviderFuture<'_> {
        Box::pin(async { Ok(Arc::clone(&self.0) as Arc<dyn CodeModeSession>) })
    }

    fn create_session_with_limits<'a>(
        &'a self,
        _limits: CodeModeSessionCellExecutionLimits,
    ) -> CodeModeSessionProviderFuture<'a> {
        self.create_session()
    }
}

impl CodeModeSession for ControlledCell {
    fn execute<'a>(
        &'a self,
        _request: ExecuteRequest,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async move {
            *self.delegate.lock().expect("delegate lock") = Some(delegate);
            let cell_id = CellId::new("controlled-cell".to_owned());
            Ok(StartedCell::from_future(cell_id.clone(), async move {
                Ok(RuntimeResponse::Yielded {
                    cell_id,
                    content_items: Vec::new(),
                    code_mode_host_duration: None,
                })
            }))
        })
    }

    fn wait<'a>(
        &'a self,
        _request: WaitRequest,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { Err("controlled cell is only terminated by Core".to_owned()) })
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            self.terminating.notify_one();
            self.release_termination.notified().await;
            self.delegate
                .lock()
                .expect("delegate lock")
                .take()
                .expect("live cell")
                .cell_closed(&cell_id);
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id,
                content_items: Vec::new(),
                code_mode_host_duration: None,
            }))
        })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async {
            self.delegate.lock().expect("delegate lock").take();
            Ok(())
        })
    }
}

#[derive(Clone, Copy)]
enum FailureSource {
    Host,
    Model,
}

#[test_case(FailureSource::Host; "host")]
#[test_case(FailureSource::Model; "model")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_waits_for_yielded_cell_termination_without_interrupt_feature(
    source: FailureSource,
) -> Result<()> {
    let (release_model, model_gate) = oneshot::channel();
    let (server, _) = start_routed_streaming_sse_server(vec![vec![
        vec![
            chunk(ev_response_created("cell-start")),
            chunk(ev_custom_tool_call("cell-call", "exec", "yield_control(); await new Promise(() => {});")),
            chunk(ev_completed("cell-start")),
        ],
        vec![
            chunk(ev_response_created("model-held")),
            chunk(ev_reasoning_item_added("model-active", &["still working"])),
            gated_chunk(model_gate, vec![json!({
                "type": "response.failed",
                "response": {"id": "model-held", "error": {"code": "insufficient_quota", "message": "model failure"}}
            })]),
        ],
    ]], |_, _| Some(0)).await;
    let cell = Arc::new(ControlledCell::default());
    let provider_cell = Arc::clone(&cell);
    let test = test_codex()
        .with_model_info_override("gpt-5.4", |model| {
            model.tool_mode = Some(codex_protocol::openai_models::ToolMode::CodeMode)
        })
        .with_thread_manager(move |manager| {
            manager.with_code_mode_session_provider(Arc::new(ControlledProvider(provider_cell)))
        })
        .with_config(|config| {
            configure_multi_agent(config, Some(AgentPolling::Disabled));
            config
                .features
                .disable(Feature::CodeModeInterrupt)
                .expect("disable interrupt");
            // Keep the supplied provider rather than replacing it with an executable lookup.
            config
                .features
                .disable(Feature::CodeModeHost)
                .expect("disable host");
        })
        .build_with_streaming_server(&server)
        .await?;
    let TurnInputSubmission::Started {
        turn_id,
        root_turn_id: _,
    } = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "start a yielded cell".to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?
    else {
        panic!("turn must start");
    };
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::ItemStarted(item) if matches!(item.item, TurnItem::Reasoning(_)))).await;
    match source {
        FailureSource::Host => {
            test.codex
                .submit(Op::FailTurn {
                    turn_id: turn_id.clone(),
                    error: codex_protocol::protocol::ErrorEvent {
                        message: "host failure".to_owned(),
                        codex_error_info: None,
                        misalignment: None,
                    },
                })
                .await?;
        }
        FailureSource::Model => {
            release_model.send(()).expect("model response held");
        }
    }
    tokio::time::timeout(Duration::from_secs(10), cell.terminating.notified()).await?;
    if matches!(source, FailureSource::Model) {
        deliver_child_mail(&test.codex, "/root/worker", "late failed-root report", true).await;
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_event(&test.codex, |event| matches!(
                event,
                EventMsg::TurnComplete(_)
            ))
        )
        .await
        .is_err(),
        "failed completion must wait for cell termination"
    );
    cell.release_termination.notify_one();
    let EventMsg::TurnComplete(completed) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(completed.turn_id, turn_id);
    assert!(completed.error.is_some());
    assert!(cell.delegate.lock().expect("delegate lock").is_none());
    assert_no_turn_starts(&test.codex, Duration::from_millis(100)).await;
    assert_eq!(streaming_request_bodies(&server).await.len(), 2);
    server.shutdown().await;
    Ok(())
}
