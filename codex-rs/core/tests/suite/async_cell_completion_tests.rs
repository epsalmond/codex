//! Automatic delivery of yielded Code Mode cell results.
use anyhow::Context;
use anyhow::Result;
use codex_code_mode::*;
use codex_core::config::Config;
use codex_features::Feature;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Encoded bound of one completion message, including its metadata.
const MAX_COMPLETION_BYTES: usize = 3072;
/// A valid 1x1 PNG, so the media result survives image processing.
const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
/// Long enough for a spurious wake turn to have issued its request.
const SETTLE: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Case {
    Hosted,
    ScriptError,
    NestedRead,
    Early,
    Inline,
    Unsupported,
    Oversized,
    Escaped,
    Media,
    WrongReady,
    WrongResponse,
    Manual,
    CloseBeforeReturn,
    Terminate,
    Off,
    InitialError,
    ClosedEarly,
    YieldedRead,
    Missing,
    ProviderError,
    TerminateFirst,
    Shutdown,
    Rejected,
    CompletedOnly,
    /// The session reports readiness support but this cell does not, so `wait` is hidden and the
    /// core watcher collects the result.
    Fallback,
}

struct Provider(Arc<Cell>);
struct Cell {
    case: Case,
    admitted: Mutex<Option<CellId>>,
    delegate: Mutex<Option<Arc<dyn CodeModeSessionDelegate>>>,
    inner: Option<Arc<dyn CodeModeSession>>,
    reads: AtomicUsize,
    terminations: AtomicUsize,
    read_done: Arc<tokio::sync::Notify>,
    release_read: Arc<tokio::sync::Notify>,
}
impl CodeModeSessionProvider for Provider {
    fn create_session(&self) -> CodeModeSessionProviderFuture<'_> {
        Box::pin(async { Ok(self.0.clone() as Arc<dyn CodeModeSession>) })
    }

    fn terminal_ready_capability(&self) -> TerminalReadySupport {
        if self.0.case == Case::Fallback {
            TerminalReadySupport::Supported
        } else {
            TerminalReadySupport::Unsupported
        }
    }
}
impl Cell {
    fn id(&self) -> CellId {
        if let Some(id) = self
            .admitted
            .lock()
            .expect("cell fixture mutex poisoned")
            .as_ref()
        {
            return id.clone();
        }
        CellId::new(match self.case {
            Case::Oversized => "x".repeat(2000),
            Case::Escaped => "\\\"".repeat(128),
            _ => "generation:cell-1".to_owned(),
        })
    }
    fn delegate(&self) -> Arc<dyn CodeModeSessionDelegate> {
        self.delegate
            .lock()
            .expect("cell fixture mutex poisoned")
            .clone()
            .expect("cell fixture delegate is live")
    }
    fn ready(&self) {
        self.delegate().cell_terminal_ready(CellTerminalReady {
            cell_id: if self.case == Case::WrongReady {
                CellId::new("other-generation:cell-1".to_owned())
            } else {
                self.id()
            },
            status: CellTerminalStatus::Completed,
            output_kind: if self.case == Case::Media {
                CellOutputKind::Media
            } else {
                CellOutputKind::Text
            },
        });
    }
}
impl CodeModeSession for Cell {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async move {
            if let Some(inner) = &self.inner {
                let started = inner.execute(request, delegate, preempt).await?;
                *self.admitted.lock().expect("cell fixture mutex poisoned") =
                    Some(started.cell_id.clone());
                assert_eq!(
                    started.terminal_ready_support,
                    TerminalReadySupport::Supported
                );
                return Ok(started);
            }
            *self.delegate.lock().expect("cell fixture mutex poisoned") = Some(delegate);
            if !matches!(
                self.case,
                Case::Manual | Case::TerminateFirst | Case::Fallback
            ) {
                self.ready();
                self.ready();
            }
            let id = self.id();
            let case = self.case;
            if self.case == Case::ClosedEarly {
                self.delegate().cell_closed(&id);
            }
            let mut started = StartedCell::from_future(id.clone(), async move {
                if case == Case::InitialError {
                    return Err("initial provider failure".to_owned());
                }
                Ok(if case == Case::Inline {
                    RuntimeResponse::Result {
                        cell_id: id,
                        content_items: vec![],
                        error_text: None,
                        code_mode_host_duration: None,
                    }
                } else {
                    RuntimeResponse::Yielded {
                        cell_id: id,
                        content_items: vec![],
                        code_mode_host_duration: None,
                    }
                })
            });
            if !matches!(case, Case::Unsupported | Case::Fallback) {
                started.terminal_ready_support = TerminalReadySupport::Supported;
            }
            Ok(started)
        })
    }
    fn wait<'a>(
        &'a self,
        request: WaitRequest,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let result = if let Some(inner) = &self.inner {
                inner.wait(request, preempt).await?
            } else {
                if self.case == Case::Manual {
                    self.ready();
                }
                if self.case == Case::ProviderError {
                    self.read_done.notify_one();
                    return Err("provider lost".to_owned());
                }
                if self.case == Case::YieldedRead {
                    self.read_done.notify_one();
                    return Ok(WaitOutcome::LiveCell(RuntimeResponse::Yielded {
                        cell_id: request.cell_id,
                        content_items: vec![],
                        code_mode_host_duration: None,
                    }));
                }
                let id = request.cell_id;
                self.delegate().cell_closed(&id);
                if matches!(
                    self.case,
                    Case::CloseBeforeReturn | Case::Terminate | Case::Shutdown
                ) {
                    self.read_done.notify_one();
                    tokio::time::timeout(Duration::from_secs(10), self.release_read.notified())
                        .await
                        .map_err(|error| error.to_string())?;
                }
                let response = RuntimeResponse::Result {
                    cell_id: if self.case == Case::WrongResponse {
                        CellId::new("other-cell".to_owned())
                    } else {
                        id
                    },
                    content_items: vec![if self.case == Case::Media {
                        FunctionCallOutputContentItem::InputImage {
                            image_url: format!("data:image/png;base64,{PNG_1X1}"),
                            detail: None,
                        }
                    } else {
                        FunctionCallOutputContentItem::InputText {
                            text: "terminal-tail".to_owned(),
                        }
                    }],
                    error_text: None,
                    code_mode_host_duration: None,
                };
                if self.case == Case::Missing {
                    WaitOutcome::MissingCell(response)
                } else {
                    WaitOutcome::LiveCell(response)
                }
            };
            self.read_done.notify_one();
            Ok(result)
        })
    }
    fn terminate<'a>(&'a self, id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.terminations.fetch_add(1, Ordering::SeqCst);
        if let Some(inner) = &self.inner {
            return inner.terminate(id);
        }
        Box::pin(async move {
            if self.case == Case::TerminateFirst {
                self.ready();
                self.delegate().cell_closed(&id);
            }
            Ok(WaitOutcome::MissingCell(RuntimeResponse::Terminated {
                cell_id: id,
                content_items: vec![],
                code_mode_host_duration: None,
            }))
        })
    }
    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        if let Some(inner) = &self.inner {
            return inner.shutdown();
        }
        Box::pin(async {
            self.delegate
                .lock()
                .expect("cell fixture mutex poisoned")
                .take();
            Ok(())
        })
    }
}

#[test_case::test_case(Case::Hosted; "real_hosted_root")]
#[test_case::test_case(Case::ScriptError; "real_script_error")]
#[test_case::test_case(Case::NestedRead; "one_nested_native_read")]
#[test_case::test_case(Case::Early; "early_duplicate_ready")]
#[test_case::test_case(Case::Inline; "inline")]
#[test_case::test_case(Case::Unsupported; "unsupported")]
#[test_case::test_case(Case::Oversized; "oversized_id")]
#[test_case::test_case(Case::Escaped; "escaped_id")]
#[test_case::test_case(Case::Media; "media_preserved")]
#[test_case::test_case(Case::WrongReady; "wrong_generation")]
#[test_case::test_case(Case::WrongResponse; "wrong_response")]
#[test_case::test_case(Case::Manual; "manual_wins")]
#[test_case::test_case(Case::CloseBeforeReturn; "closed_before_wait_returns")]
#[test_case::test_case(Case::Terminate; "automatic_read_before_terminate")]
#[test_case::test_case(Case::Off; "feature_off")]
#[test_case::test_case(Case::InitialError; "initial_error")]
#[test_case::test_case(Case::ClosedEarly; "closed_before_initial")]
#[test_case::test_case(Case::YieldedRead; "unexpected_yield_after_ready")]
#[test_case::test_case(Case::Missing; "missing_after_ready")]
#[test_case::test_case(Case::ProviderError; "provider_error_after_ready")]
#[test_case::test_case(Case::TerminateFirst; "terminate_wins")]
#[test_case::test_case(Case::Shutdown; "shutdown_cancels_collector")]
#[test_case::test_case(Case::Rejected; "rejected_cell_rebuild_created")]
#[test_case::test_case(Case::CompletedOnly; "rejected_cell_rebuild_completed_only")]
#[test_case::test_case(Case::Fallback; "unsupported_runtime_provider_falls_back_to_core_observer")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_cell_completion(case: Case) -> Result<()> {
    let hosted = matches!(
        case,
        Case::Hosted | Case::ScriptError | Case::NestedRead | Case::Rejected | Case::CompletedOnly
    );
    let inner = if hosted {
        Some(
            ProcessOwnedCodeModeSessionProvider::with_host_program(
                codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?,
            )
            .create_session()
            .await
            .map_err(anyhow::Error::msg)?,
        )
    } else {
        None
    };
    let cell = Arc::new(Cell {
        case,
        admitted: Mutex::new(None),
        inner,
        delegate: Mutex::new(None),
        reads: AtomicUsize::new(0),
        terminations: AtomicUsize::new(0),
        read_done: Arc::new(tokio::sync::Notify::new()),
        release_read: Arc::new(tokio::sync::Notify::new()),
    });
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 1; echo native-tail"
    } else {
        "sleep 1; echo native-tail"
    };
    let code = match case {
        Case::NestedRead => format!(
            "text('initial'); yield_control(); const p = await tools.exec_command({{cmd:{command:?},yield_time_ms:250}}); text(await tools.write_stdin({{session_id:p.session_id,chars:'',yield_time_ms:3000}})); text('terminal-tail');"
        ),
        Case::ScriptError => "text('initial'); yield_control(); await new Promise(r => setTimeout(r,100)); throw new Error('script-error-tail');".to_owned(),
        _ => "text('initial'); yield_control(); await new Promise(r => setTimeout(r,100)); text('terminal-tail');".to_owned(),
    };
    let fallback = matches!(
        case,
        Case::Unsupported
            | Case::Oversized
            | Case::Escaped
            | Case::Media
            | Case::WrongReady
            | Case::Off
    );
    let manual = matches!(case, Case::Manual | Case::TerminateFirst) || fallback;
    let delivered =
        !manual && !matches!(case, Case::Inline | Case::InitialError | Case::ClosedEarly);
    let (release, gate) = tokio::sync::oneshot::channel();
    let ordinary = if manual || case == Case::Terminate {
        sse(vec![
            ev_response_created("ordinary"),
            ev_function_call(
                "observe",
                "wait",
                &json!({"cell_id":cell.id().as_str(),"terminate":matches!(case,Case::Terminate | Case::TerminateFirst)})
                    .to_string(),
            ),
            ev_completed("ordinary"),
        ])
    } else {
        sse(vec![
            ev_response_created("ordinary"),
            ev_function_call(
                "independent",
                "exec_command",
                &json!({"cmd":"echo independent"}).to_string(),
            ),
            ev_completed("ordinary"),
        ])
    };
    let retention = matches!(case, Case::Rejected | Case::CompletedOnly);
    let accepted = if retention {
        sse(vec![
            json!({"type":"response.failed","response":{"id":"rejected","error":{"code":"insufficient_quota","message":"rejected before acceptance"}}}),
        ])
    } else {
        sse(vec![
            ev_response_created("accepted"),
            ev_assistant_message("done", "done"),
            ev_completed("accepted"),
        ])
    };
    let mut streams = vec![
        vec![StreamingSseChunk {
            gate: None,
            body: sse(vec![
                ev_response_created("start"),
                ev_custom_tool_call("cell-call", "exec", &code),
                ev_completed("start"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: Some(gate),
            body: ordinary,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: accepted,
        }],
    ];
    if retention {
        for id in [
            "rebuild-before-acceptance",
            "recovered",
            "rebuild-after-acceptance",
            "after-acceptance",
        ] {
            let body = if id.starts_with("rebuild") {
                sse(vec![
                    ev_response_created(id),
                    json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"CELL_REBUILD_SUMMARY"}}),
                    ev_completed(id),
                ])
            } else if id == "recovered" && case == Case::CompletedOnly {
                sse(vec![ev_assistant_message(id, "done"), ev_completed(id)])
            } else {
                sse(vec![
                    ev_response_created(id),
                    ev_assistant_message(id, "done"),
                    ev_completed(id),
                ])
            };
            streams.push(vec![StreamingSseChunk { gate: None, body }]);
        }
    }
    let (server, _) = start_streaming_sse_server(streams).await;
    let bootstrap = start_mock_server().await;
    let base_url = format!("{}/v1", server.uri());
    let provided = cell.clone();
    let test = test_codex()
        .with_model_info_override("gpt-5.4", |model| {
            model.tool_mode = Some(ToolMode::CodeMode)
        })
        .with_thread_manager(move |manager| {
            manager.with_code_mode_session_provider(Arc::new(Provider(provided)))
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("cell completion fixture boundary");
            config
                .features
                .set_enabled(Feature::CodeModeHost, hosted)
                .expect("cell completion fixture boundary");
            config
                .features
                .set_enabled(Feature::AsyncProcessCompletion, case != Case::Off)
                .expect("cell completion fixture boundary");
            config.model_provider.base_url = Some(base_url);
            config.model_provider.stream_idle_timeout_ms = Some(600000);
        })
        .build_with_auto_env(&bootstrap)
        .await?;
    super::unified_exec::submit_unified_exec_turn(
        &test,
        "start work",
        codex_protocol::models::PermissionProfile::Disabled,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(10), server.wait_for_request_count(2)).await?;
    if delivered {
        tokio::time::timeout(Duration::from_secs(10), cell.read_done.notified())
            .await
            .context("automatic read must happen once")?;
    }
    if case == Case::Shutdown {
        test.codex.submit(Op::Shutdown).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::ShutdownComplete)
        })
        .await;
        assert_eq!(cell.reads.load(Ordering::SeqCst), 1);
        assert!(
            cell.delegate
                .lock()
                .expect("cell fixture mutex poisoned")
                .is_none()
        );
        server.shutdown().await;
        return Ok(());
    }
    let mut release = Some(release);
    if case == Case::Terminate {
        release
            .take()
            .expect("model gate available")
            .send(())
            .expect("model gate held");
        wait_for_event(&test.codex, |event| matches!(event,EventMsg::RawResponseItem(raw) if matches!(&raw.item,codex_protocol::models::ResponseItem::FunctionCall { call_id,.. } if call_id == "observe"))).await;
        tokio::task::yield_now().await;
        // The model's terminate queues behind the collector's terminal read.
        assert_eq!(cell.terminations.load(Ordering::SeqCst), 0);
        cell.release_read.notify_one();
    } else if case == Case::CloseBeforeReturn {
        cell.release_read.notify_one();
    }
    if let Some(release) = release {
        release.send(()).expect("model gate held");
    }
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = request_bodies(&server).await;
    let completions = requests.last().expect("model request captured")["input"]
        .as_array()
        .expect("cell completion fixture boundary")
        .iter()
        .filter(|item| {
            item["content"][0]["text"].as_str().is_some_and(|text| {
                text.starts_with("<async_tool_completion>") && text.contains("call=cell-call")
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(completions.len(), usize::from(delivered));
    if let Some(item) = completions.first() {
        let text = item["content"][0]["text"]
            .as_str()
            .expect("completion text");
        assert!(text.contains(&format!("kind=cell cell={}", cell.id())));
        assert!(!text.contains("process="));
        let collection_failed = matches!(
            case,
            Case::WrongResponse | Case::YieldedRead | Case::Missing | Case::ProviderError
        );
        assert!(text.contains(if collection_failed {
            "status=collectionFailed"
        } else if case == Case::ScriptError {
            "status=error"
        } else {
            "status=completed"
        }));
        if !collection_failed {
            assert!(text.contains(if case == Case::ScriptError {
                "script-error-tail"
            } else {
                "terminal-tail"
            }));
        }
        assert!(serde_json::to_vec(item)?.len() <= MAX_COMPLETION_BYTES);
        assert!(item["internal_chat_message_metadata_passthrough"]["create_time"].is_number());
    }
    if case == Case::NestedRead {
        let text = requests.last().expect("model request captured").to_string();
        assert!(text.contains("native-tail"));
    }
    assert_eq!(
        cell.reads.load(Ordering::SeqCst),
        usize::from(!matches!(
            case,
            Case::Inline | Case::InitialError | Case::ClosedEarly | Case::TerminateFirst
        ))
    );
    if retention {
        let original = (*completions[0]).clone();
        for phase in 0..2 {
            test.codex.submit(Op::Compact).await?;
            wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::TurnComplete(_))
            })
            .await;
            test.submit_turn("recover original cell result").await?;
            let request: Value = serde_json::from_slice(
                server
                    .requests()
                    .await
                    .last()
                    .expect("model request captured"),
            )?;
            let delivered = request["input"]
                .as_array()
                .expect("model input array")
                .iter()
                .find(|item| {
                    item["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("kind=cell"))
                });
            assert_eq!(delivered, if phase == 0 { Some(&original) } else { None });
        }
    }
    if case == Case::Media {
        // Media stays readable through `wait`; it is not reduced to a text tail.
        let last = requests.last().expect("model request captured");
        let observed = last["input"]
            .as_array()
            .expect("model input array")
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "observe")
            .expect("wait output recorded");
        assert!(
            observed["output"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["type"] == "input_image")),
            "{observed}"
        );
    }
    server.shutdown().await;
    Ok(())
}

async fn request_bodies(server: &StreamingSseServer) -> Vec<Value> {
    server
        .requests()
        .await
        .iter()
        .map(|body| serde_json::from_slice(body).expect("model request is JSON"))
        .collect()
}

/// Completion fragments carried by one model request.
fn completions(request: &Value) -> Vec<String> {
    request["input"]
        .as_array()
        .expect("model input array")
        .iter()
        .filter_map(|item| {
            let text = item["content"][0]["text"].as_str()?;
            text.starts_with("<async_tool_completion>")
                .then(|| text.to_owned())
        })
        .collect()
}

/// Yielded cells whose readiness and terminal result the test controls.
#[derive(Default)]
pub(super) struct ControlledCells {
    executions: AtomicUsize,
    live: Mutex<Vec<(CellId, Arc<dyn CodeModeSessionDelegate>)>>,
    reads: AtomicUsize,
    terminations: AtomicUsize,
    read_done: tokio::sync::Notify,
    terminated: tokio::sync::Notify,
}

pub(super) struct ControlledProvider(pub(super) Arc<ControlledCells>);

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

impl ControlledCells {
    fn take(&self, cell_id: &CellId) -> Option<Arc<dyn CodeModeSessionDelegate>> {
        let mut live = self.live.lock().expect("controlled cell mutex poisoned");
        let index = live.iter().position(|(id, _)| id == cell_id)?;
        Some(live.remove(index).1)
    }

    pub(super) fn executions(&self) -> usize {
        self.executions.load(Ordering::SeqCst)
    }

    /// Reports every live cell's text result ready.
    pub(super) fn finish_all(&self) {
        let live = self
            .live
            .lock()
            .expect("controlled cell mutex poisoned")
            .clone();
        for (cell_id, delegate) in live {
            delegate.cell_terminal_ready(CellTerminalReady {
                cell_id,
                status: CellTerminalStatus::Completed,
                output_kind: CellOutputKind::Text,
            });
        }
    }

    pub(super) async fn wait_for_read(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.read_done.notified())
            .await
            .expect("controlled cell terminal read");
    }
}

impl CodeModeSession for ControlledCells {
    fn execute<'a>(
        &'a self,
        _request: ExecuteRequest,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async move {
            let index = self.executions.fetch_add(1, Ordering::SeqCst);
            let cell_id = CellId::new(format!("controlled:cell-{index}"));
            self.live
                .lock()
                .expect("controlled cell mutex poisoned")
                .push((cell_id.clone(), delegate));
            let yielded = cell_id.clone();
            let mut started = StartedCell::from_future(cell_id, async move {
                Ok(RuntimeResponse::Yielded {
                    cell_id: yielded,
                    content_items: vec![FunctionCallOutputContentItem::InputText {
                        text: "controlled-initial".to_owned(),
                    }],
                    code_mode_host_duration: None,
                })
            });
            started.terminal_ready_support = TerminalReadySupport::Supported;
            Ok(started)
        })
    }

    fn wait<'a>(
        &'a self,
        request: WaitRequest,
        _preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let Some(delegate) = self.take(&request.cell_id) else {
                return Err("controlled cell is not live".to_owned());
            };
            delegate.cell_closed(&request.cell_id);
            self.read_done.notify_one();
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Result {
                cell_id: request.cell_id,
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "controlled-tail".to_owned(),
                }],
                error_text: None,
                code_mode_host_duration: None,
            }))
        })
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async move {
            self.terminations.fetch_add(1, Ordering::SeqCst);
            let Some(delegate) = self.take(&cell_id) else {
                return Ok(WaitOutcome::MissingCell(RuntimeResponse::Terminated {
                    cell_id,
                    content_items: vec![],
                    code_mode_host_duration: None,
                }));
            };
            delegate.cell_closed(&cell_id);
            self.terminated.notify_one();
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id,
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "partial-tail".to_owned(),
                }],
                code_mode_host_duration: None,
            }))
        })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async {
            self.live
                .lock()
                .expect("controlled cell mutex poisoned")
                .clear();
            Ok(())
        })
    }
}

/// Enables Code Mode backed by `cells` and automatic completion against the streaming server.
pub(super) fn configure_controlled_cells(config: &mut Config) {
    config
        .features
        .enable(Feature::CodeMode)
        .expect("enable code mode");
    // Keep the supplied provider rather than replacing it with the host program.
    config
        .features
        .disable(Feature::CodeModeHost)
        .expect("disable code mode host");
    config
        .features
        .enable(Feature::AsyncProcessCompletion)
        .expect("enable async completion");
}

async fn controlled_codex(
    cells: &Arc<ControlledCells>,
    server: &StreamingSseServer,
    configure: impl FnOnce(&mut Config) + Send + 'static,
) -> Result<TestCodex> {
    let provided = Arc::clone(cells);
    test_codex()
        .with_model_info_override("gpt-5.4", |model| {
            model.tool_mode = Some(ToolMode::CodeMode)
        })
        .with_thread_manager(move |manager| {
            manager.with_code_mode_session_provider(Arc::new(ControlledProvider(provided)))
        })
        .with_config(move |config| {
            configure_controlled_cells(config);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.request_max_retries = Some(0);
            configure(config);
        })
        .build_with_streaming_server(server)
        .await
}

fn exec_response(id: &str, call_id: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_custom_tool_call(call_id, "exec", "text('initial'); yield_control();"),
        ev_completed(id),
    ])
}

fn assistant_response(id: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_assistant_message(id, "continued"),
        ev_completed(id),
    ])
}

fn streams(bodies: Vec<String>) -> Vec<Vec<StreamingSseChunk>> {
    bodies
        .into_iter()
        .map(|body| vec![StreamingSseChunk { gate: None, body }])
        .collect()
}

async fn wait_for_turn_complete(test: &TestCodex) {
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
}

async fn assert_request_count_settles(server: &StreamingSseServer, expected: usize) {
    tokio::time::sleep(SETTLE).await;
    assert_eq!(request_bodies(server).await.len(), expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_root_wakes_once_on_cell_completion() -> Result<()> {
    let (server, _) = start_streaming_sse_server(streams(vec![
        exec_response("start", "cell-call"),
        assistant_response("idle"),
        assistant_response("automatic"),
    ]))
    .await;
    let cells = Arc::new(ControlledCells::default());
    let test = controlled_codex(&cells, &server, |_| {}).await?;
    test.submit_turn("start a cell").await?;
    assert_eq!(request_bodies(&server).await.len(), 2);
    assert!(completions(&request_bodies(&server).await[1]).is_empty());

    cells.finish_all();
    cells.wait_for_read().await;
    wait_for_turn_complete(&test).await;
    assert_request_count_settles(&server, 3).await;
    let requests = request_bodies(&server).await;
    let delivered = completions(&requests[2]);
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].contains("call=cell-call kind=cell cell=controlled:cell-0"));
    assert!(delivered[0].contains("status=completed"));
    assert!(delivered[0].contains("controlled-tail"));
    assert_eq!(cells.reads.load(Ordering::SeqCst), 1);
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_root_holds_cell_completion_for_next_message() -> Result<()> {
    let (server, _) = start_streaming_sse_server(streams(vec![
        exec_response("start", "cell-call"),
        assistant_response("idle"),
        assistant_response("resumed"),
        assistant_response("after-acceptance"),
    ]))
    .await;
    let cells = Arc::new(ControlledCells::default());
    let test = controlled_codex(&cells, &server, |_| {}).await?;
    test.submit_turn("start a cell").await?;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::AgentWakeupsUpdated(update) if update.paused),
    )
    .await;

    cells.finish_all();
    cells.wait_for_read().await;
    assert_request_count_settles(&server, 2).await;

    test.submit_turn("next message").await?;
    let requests = request_bodies(&server).await;
    assert_eq!(requests.len(), 3);
    let delivered = completions(&requests[2]);
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].contains("kind=cell cell=controlled:cell-0"));
    assert!(delivered[0].contains("controlled-tail"));

    test.submit_turn("after acceptance").await?;
    let requests = request_bodies(&server).await;
    assert_eq!(requests.len(), 4);
    assert_eq!(completions(&requests[3]), delivered);
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_publishes_cancelled_cell_completion_once() -> Result<()> {
    let (release_held, held_gate) = tokio::sync::oneshot::channel::<()>();
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: exec_response("start", "cell-call"),
        }],
        vec![StreamingSseChunk {
            gate: Some(held_gate),
            body: assistant_response("held"),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: assistant_response("resumed"),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: assistant_response("after-acceptance"),
        }],
    ])
    .await;
    let cells = Arc::new(ControlledCells::default());
    let test = controlled_codex(&cells, &server, |config| {
        config
            .features
            .enable(Feature::CodeModeInterrupt)
            .expect("enable code mode interrupt");
    })
    .await?;
    test.codex
        .start_or_steer_turn(codex_core::TurnInputRequest::user_input(vec![
            codex_protocol::user_input::UserInput::Text {
                text: "start a cell".to_owned(),
                text_elements: Vec::new(),
            },
        ]))
        .await?;
    tokio::time::timeout(Duration::from_secs(10), server.wait_for_request_count(2)).await?;

    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    drop(release_held);
    tokio::time::timeout(Duration::from_secs(10), cells.terminated.notified())
        .await
        .context("the interrupt terminates the yielded cell")?;
    assert_eq!(cells.terminations.load(Ordering::SeqCst), 1);
    assert_eq!(cells.reads.load(Ordering::SeqCst), 0);
    // A late readiness report after the interrupt publishes nothing more.
    cells.finish_all();
    assert_request_count_settles(&server, 2).await;

    test.submit_turn("next message").await?;
    let requests = request_bodies(&server).await;
    assert_eq!(requests.len(), 3);
    let delivered = completions(&requests[2]);
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].contains("kind=cell cell=controlled:cell-0"));
    assert!(delivered[0].contains("status=cancelled"));
    assert!(delivered[0].contains("partial-tail"));

    test.submit_turn("after acceptance").await?;
    let requests = request_bodies(&server).await;
    assert_eq!(requests.len(), 4);
    assert_eq!(completions(&requests[3]), delivered);
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cell_reservation_capacity_rejects_before_execution() -> Result<()> {
    // The store holds 64 reservations per owner; the 65th cell must not start.
    const CELLS: usize = 65;
    let mut start = vec![ev_response_created("start")];
    for index in 0..CELLS {
        start.push(ev_custom_tool_call(
            &format!("cell-{index}"),
            "exec",
            "text('initial'); yield_control();",
        ));
    }
    start.push(ev_completed("start"));
    let (server, _) =
        start_streaming_sse_server(streams(vec![sse(start), assistant_response("done")])).await;
    let cells = Arc::new(ControlledCells::default());
    let test = controlled_codex(&cells, &server, |_| {}).await?;
    test.submit_turn("start many cells").await?;

    assert_eq!(cells.executions(), CELLS - 1);
    let requests = request_bodies(&server).await;
    let outputs = requests[1]["input"]
        .as_array()
        .expect("model input array")
        .iter()
        .filter(|item| item["type"] == "custom_tool_call_output")
        .map(|item| {
            (
                item["call_id"].as_str().unwrap_or_default().to_owned(),
                item["output"].to_string(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), CELLS);
    let rejected = outputs
        .iter()
        .filter(|(_, output)| output.contains("async completion capacity exhausted"))
        .map(|(call_id, _)| call_id.as_str())
        .collect::<Vec<_>>();
    // Only the call past capacity is rejected, and it never reached the provider.
    assert_eq!(rejected, vec![format!("cell-{}", CELLS - 1).as_str()]);
    server.shutdown().await;
    Ok(())
}
