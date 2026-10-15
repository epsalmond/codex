//! Real hosted callbacks held until the original owner finishes its turn.
//!
//! Each script yields, then makes one callback. The fixture holds that callback
//! until the turn is complete and releases it while no turn is active.
use super::*;
use codex_code_mode::CellId;
use codex_code_mode::CellTerminalReady;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeSessionProviderFuture;
use codex_code_mode::CodeModeSessionResultFuture;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::StartedCell;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use pretty_assertions::assert_eq;
use test_case::test_case;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const HOLD_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_REFUSAL: &str = "Rerun this call inside a turn.";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Callback {
    /// A nested tool call completes while the owner is idle.
    Native,
    /// A notification is kept in history for the owner's next turn.
    Notify,
    /// A nested call that needs a user prompt is refused while idle.
    UserApproval,
    /// A nested call that needs a Guardian decision is refused while idle.
    GuardianApproval,
    /// Shutdown stops the worker while an idle nested call is in flight.
    Shutdown,
}

struct Gates {
    reached: Notify,
    release: Notify,
    forwarded: Notify,
    settled: Notify,
}

struct Provider(Arc<HeldSession>);

impl CodeModeSessionProvider for Provider {
    fn create_session(&self) -> CodeModeSessionProviderFuture<'_> {
        Box::pin(async { Ok(Arc::clone(&self.0) as Arc<dyn CodeModeSession>) })
    }
}

/// Wraps a real hosted session so the first callback after `exec` can be held.
struct HeldSession {
    inner: Arc<dyn CodeModeSession>,
    gates: Arc<Gates>,
}

impl CodeModeSession for HeldSession {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        let delegate = Arc::new(HeldDelegate {
            inner: delegate,
            gates: Arc::clone(&self.gates),
        });
        self.inner.execute(request, delegate, preempt)
    }

    fn wait<'a>(
        &'a self,
        request: WaitRequest,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.inner.wait(request, preempt)
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.inner.terminate(cell_id)
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        self.inner.shutdown()
    }
}

struct HeldDelegate {
    inner: Arc<dyn CodeModeSessionDelegate>,
    gates: Arc<Gates>,
}

impl HeldDelegate {
    async fn hold(&self) -> Result<(), String> {
        self.gates.reached.notify_one();
        tokio::time::timeout(HOLD_TIMEOUT, self.gates.release.notified())
            .await
            .map_err(|error| error.to_string())?;
        self.gates.forwarded.notify_one();
        Ok(())
    }
}

impl CodeModeSessionDelegate for HeldDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            self.hold().await?;
            let result = self.inner.invoke_tool(invocation, cancellation_token).await;
            self.gates.settled.notify_one();
            result
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            self.hold().await?;
            let result = self
                .inner
                .notify(call_id, cell_id, text, cancellation_token)
                .await;
            self.gates.settled.notify_one();
            result
        })
    }

    fn cell_terminal_ready(&self, ready: CellTerminalReady) {
        self.inner.cell_terminal_ready(ready);
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.inner.cell_closed(cell_id);
    }
}

fn callback_source(callback: Callback) -> &'static str {
    match callback {
        Callback::Native => {
            r#"const native = await tools.exec_command({ cmd: "printf native-tail" }); text(native.output);"#
        }
        Callback::Notify => r#"notify("original-idle-notification");"#,
        Callback::UserApproval | Callback::GuardianApproval => {
            r#"try {
  const escalated = await tools.exec_command({ cmd: "touch escalated-ran", sandbox_permissions: "require_escalated", justification: "needs approval" });
  text("escalated: " + JSON.stringify(escalated));
} catch (error) {
  text("escalated error: " + String(error));
}"#
        }
        Callback::Shutdown => {
            r#"await tools.exec_command({ cmd: "sleep 60", yield_time_ms: 30000 });"#
        }
    }
}

#[test_case(Callback::Native; "native_callback_after_idle")]
#[test_case(Callback::Notify; "notify_after_idle")]
#[test_case(Callback::UserApproval; "user_approval_refused_while_idle")]
#[test_case(Callback::GuardianApproval; "guardian_approval_refused_while_idle")]
#[test_case(Callback::Shutdown; "shutdown_stops_idle_callback")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_callback_settles_after_original_owner_becomes_idle(
    callback: Callback,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let inner = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?,
    )
    .create_session()
    .await
    .map_err(anyhow::Error::msg)?;
    let gates = Arc::new(Gates {
        reached: Notify::new(),
        release: Notify::new(),
        forwarded: Notify::new(),
        settled: Notify::new(),
    });
    let session = Arc::new(HeldSession {
        inner,
        gates: Arc::clone(&gates),
    });

    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_thread_manager(move |manager| {
            manager.with_code_mode_session_provider(Arc::new(Provider(session)))
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("enable Code Mode");
            // Keep the supplied provider rather than replacing it with an executable lookup.
            config
                .features
                .disable(Feature::CodeModeHost)
                .expect("disable host lookup");
            if matches!(
                callback,
                Callback::UserApproval | Callback::GuardianApproval
            ) {
                config.permissions.approval_policy =
                    Constrained::allow_any(AskForApproval::OnRequest);
            }
            if callback == Callback::GuardianApproval {
                config.approvals_reviewer = ApprovalsReviewer::AutoReview;
            }
        })
        .build(&server)
        .await?;

    let code = format!(
        "text('initial');\nyield_control();\n{}\ntext('cell-tail');\n",
        callback_source(callback)
    );
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call("original-cell-call", "exec", &code),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let first_completion = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "owner response"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_text_turn("launch hosted work").await?;
    tokio::time::timeout(HOLD_TIMEOUT, gates.reached.notified()).await?;
    let first_items =
        custom_tool_output_items(&first_completion.single_request(), "original-cell-call");
    let cell_id = extract_running_cell_id(text_item(&first_items, /*index*/ 0));

    // The original turn is over; release the held callback while the owner is idle.
    gates.release.notify_one();
    if callback == Callback::Shutdown {
        tokio::time::timeout(HOLD_TIMEOUT, gates.forwarded.notified()).await?;
        tokio::time::timeout(HOLD_TIMEOUT, test.codex.shutdown_and_wait()).await??;
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            2
        );
        return Ok(());
    }
    tokio::time::timeout(HOLD_TIMEOUT, gates.settled.notified())
        .await
        .context("an idle cell callback must settle without a new turn")?;
    // Settling while idle starts no model request (and no Guardian review).
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        2
    );

    let wait_request = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-3"),
            responses::ev_function_call(
                "wait-call",
                "wait",
                &serde_json::to_string(&serde_json::json!({ "cell_id": cell_id }))?,
            ),
            ev_completed("resp-3"),
        ]),
    )
    .await;
    let wait_completion = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-2", "done"),
            ev_completed("resp-4"),
        ]),
    )
    .await;
    test.submit_text_turn("read the cell").await?;

    let wait_items = function_tool_output_items(&wait_completion.single_request(), "wait-call");
    let wait_output = wait_items
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(wait_output.starts_with("Script completed"), "{wait_output}");
    assert!(wait_output.contains("cell-tail"), "{wait_output}");
    match callback {
        Callback::Native => {
            assert!(wait_output.contains("native-tail"), "{wait_output}");
        }
        Callback::Notify => {
            let notifications = wait_request
                .single_request()
                .input()
                .into_iter()
                .filter(|item| {
                    item["type"] == "custom_tool_call_output"
                        && item.to_string().contains("original-idle-notification")
                })
                .collect::<Vec<_>>();
            assert_eq!(notifications.len(), 1, "{notifications:?}");
            assert_eq!(notifications[0]["call_id"], "original-cell-call");
        }
        Callback::UserApproval | Callback::GuardianApproval => {
            assert!(wait_output.contains(IDLE_REFUSAL), "{wait_output}");
            assert!(!test.workspace_path("escalated-ran").exists());
        }
        Callback::Shutdown => unreachable!("shutdown returns before the second turn"),
    }

    test.codex.shutdown_and_wait().await?;
    Ok(())
}
