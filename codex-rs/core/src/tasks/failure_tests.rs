use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use test_case::test_case;
use tokio_util::sync::CancellationToken;

struct ErrorTask(ErrorEvent);
impl SessionTask for ErrorTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.error_test"
    }
    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancel: CancellationToken,
    ) -> SessionTaskResult {
        session
            .send_event(&context, EventMsg::Error(self.0.clone()))
            .await;
        Ok(None)
    }
}

#[test_case(None, true; "untyped")]
#[test_case(Some(CodexErrorInfo::Other), true; "terminal_coded")]
#[test_case(Some(CodexErrorInfo::ThreadRollbackFailed), false; "recoverable")]
#[tokio::test]
async fn emitted_error_and_completion_share_the_terminal_predicate(
    info: Option<CodexErrorInfo>,
    terminal: bool,
) {
    let (session, context, receiver) = make_session_and_context_with_rx().await;
    let error = ErrorEvent {
        message: "error during task".to_owned(),
        codex_error_info: info,
        misalignment: None,
    };
    session
        .spawn_task(Arc::clone(&context), Vec::new(), ErrorTask(error.clone()))
        .await;
    let completed = loop {
        let event = receiver.recv().await.expect("task event");
        if let EventMsg::TurnComplete(completed) = event.msg {
            break completed;
        }
    };
    let expected = terminal.then_some(error);
    assert_eq!(completed.error, expected);
    assert_eq!(*context.terminal_error.lock().await, expected);
}

/// Never reads its input, so mail handed to the turn stays accepted but unrecorded.
struct PendingTask;
impl SessionTask for PendingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.pending_test"
    }
    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancel: CancellationToken,
    ) -> SessionTaskResult {
        cancel.cancelled().await;
        Ok(None)
    }
}

/// A host failure keeps agent mail the turn accepted but had not recorded, held until an
/// explicit follow-up or user turn.
#[tokio::test]
async fn host_failure_returns_accepted_mail_and_pauses_wakeups() {
    let (session, context, _receiver) = make_session_and_context_with_rx().await;
    let mail = codex_protocol::protocol::InterAgentCommunication::new(
        codex_protocol::AgentPath::root(),
        codex_protocol::AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "accepted followup".to_string(),
        /*trigger_turn*/ true,
    );
    session
        .input_queue
        .enqueue_mailbox_communication(mail.clone(), Default::default())
        .await;
    session
        .spawn_task(Arc::clone(&context), Vec::new(), PendingTask)
        .await;

    session
        .fail_turn_if_active(
            &context.sub_id,
            ErrorEvent {
                message: "host failure".to_owned(),
                codex_error_info: None,
                misalignment: None,
            },
        )
        .await;
    let paused = session.input_queue.wakeups_paused();
    session.input_queue.resume_wakeups();

    assert_eq!(
        (
            paused,
            session.input_queue.drain_mailbox_input_items().await.0
        ),
        (true, vec![TurnInput::InterAgentCommunication(mail)])
    );
}
