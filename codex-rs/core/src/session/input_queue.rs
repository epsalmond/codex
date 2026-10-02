use crate::session::multi_agents::ChildReportMode;
use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_diagnostics::Gauge;
use codex_diagnostics::GaugeGuard;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

static PENDING_MAILBOX_MESSAGES: Gauge = Gauge::new("core.mailbox.pending");

/// Host capture metadata belonging to one input, including steers within another turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_order: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "codex_history::UserInputOrigin::is_user"
    )]
    pub origin: codex_history::UserInputOrigin,
}

/// Input consumed by a regular turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
        #[serde(flatten)]
        metadata: UserInputMetadata,
    },
    FunctionCallOutput(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    // Preserve the existing serialized format while carrying injection API metadata
    // through the in-memory queue.
    ResponseItem(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    InterAgentCommunication(InterAgentCommunication),
}

mod turn_input_response_item {
    use super::ResponseItem;
    use super::ResponseItemEnvelope;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serialize;
    use serde::Serializer;
    use serde::ser::Error as _;

    pub(super) fn serialize<S>(
        item: &ResponseItemEnvelope,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if item.metadata.is_some() {
            return Err(S::Error::custom(
                "annotated response items cannot cross the turn-input serialization boundary",
            ));
        }
        item.item.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<ResponseItemEnvelope, D::Error>
    where
        D: Deserializer<'de>,
    {
        ResponseItem::deserialize(deserializer).map(ResponseItemEnvelope::new)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputQueueActivity {
    Mailbox,
    Steer,
}

/// Turn-local pending input storage owned by the input queue flow.
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    items: Vec<TurnInput>,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    mailbox_pending_mails: Mutex<VecDeque<PendingMailboxCommunication>>,
    /// Holds trigger-turn mail after an interrupt until the next user message.
    wakeups_paused: AtomicBool,
}

struct PendingMailboxCommunication {
    communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    _diagnostics_guard: GaugeGuard,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            mailbox_pending_mails: Mutex::new(VecDeque::new()),
            wakeups_paused: AtomicBool::new(false),
        }
    }

    pub(crate) async fn subscribe_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let activity_rx = self.activity_tx.subscribe();
        let turn_activity = if let Some(turn_state) = turn_state {
            turn_state.lock().await.pending_input.pending_activity()
        } else {
            None
        };
        let pending_activity = if let Some(activity) = turn_activity {
            Some(activity)
        } else if self.has_pending_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        (activity_rx, pending_activity)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn deliver_mailbox_communication_to_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        communication: InterAgentCommunication,
    ) -> bool {
        let active = active_turn.lock().await;
        let Some(active_turn) = active.as_ref().filter(|turn| turn.task.is_some()) else {
            return false;
        };
        let mut turn_state = active_turn.turn_state.lock().await;
        if !turn_state.accepts_mailbox_delivery_for_current_turn() {
            return false;
        }
        turn_state
            .pending_input
            .items
            .push(TurnInput::InterAgentCommunication(communication));
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        true
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) {
        self.enqueue_mailbox_communication_if(communication, start_options, || true)
            .await;
    }

    /// Inserts mail and synchronously commits its delivery while the queue is locked.
    /// Rejected handoffs are removed before any consumer can observe them.
    pub(crate) async fn enqueue_mailbox_communication_if(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
        accept: impl FnOnce() -> bool,
    ) -> bool {
        let held = communication.trigger_turn && self.wakeups_paused();
        let mut pending = self.mailbox_pending_mails.lock().await;
        pending.push_back(PendingMailboxCommunication {
            communication,
            start_options,
            _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
        });
        if !accept() {
            pending.pop_back();
            return false;
        }
        drop(pending);
        // Held mail has nothing to deliver yet, so it must not interrupt a `sleep`.
        if !held {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
        true
    }

    /// Returns whether mail is ready for delivery. Paused wakeups hold trigger-turn mail.
    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        let paused = self.wakeups_paused();
        self.mailbox_pending_mails
            .lock()
            .await
            .iter()
            .any(|mail| !(paused && mail.communication.trigger_turn))
    }

    /// Returns whether pending mail should start a turn. Paused wakeups hold it.
    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        !self.wakeups_paused() && self.trigger_turn_mailbox_count().await > 0
    }

    pub(crate) async fn trigger_turn_mailbox_count(&self) -> usize {
        self.mailbox_pending_mails
            .lock()
            .await
            .iter()
            .filter(|mail| mail.communication.trigger_turn)
            .count()
    }

    pub(crate) fn pause_wakeups(&self) {
        self.wakeups_paused.store(true, Ordering::SeqCst);
    }

    /// Clears the pause and returns whether wakeups were paused.
    pub(crate) fn resume_wakeups(&self) -> bool {
        self.wakeups_paused.swap(false, Ordering::SeqCst)
    }

    pub(crate) fn wakeups_paused(&self) -> bool {
        self.wakeups_paused.load(Ordering::SeqCst)
    }

    /// Drains deliverable mail. Paused wakeups leave trigger-turn mail queued.
    pub(crate) async fn drain_mailbox_input_items(&self) -> (Vec<TurnInput>, TurnStartOptions) {
        let pending_mails = {
            let paused = self.wakeups_paused();
            let mut mails = self.mailbox_pending_mails.lock().await;
            let (held, deliverable): (VecDeque<_>, VecDeque<_>) = mails
                .drain(..)
                .partition(|mail| paused && mail.communication.trigger_turn);
            *mails = held;
            deliverable
        };
        // A later follow-up supersedes the earlier choice, including an omitted choice.
        let mut start_options = pending_mails
            .iter()
            .rev()
            .find(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.clone())
            .unwrap_or_default();
        start_options.parent_turn_id = pending_mails
            .iter()
            .filter(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.parent_turn_id.as_deref())
            .reduce(|expected, candidate| expected.filter(|id| candidate == Some(*id)))
            .and_then(|id| id.filter(|id| !id.trim().is_empty()).map(str::to_string));
        start_options.root_turn_id = pending_mails
            .iter()
            .find(|mail| mail.communication.trigger_turn)
            .and_then(|mail| {
                mail.start_options
                    .parent_turn_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .and(mail.start_options.root_turn_id.as_deref())
                    .filter(|id| !id.trim().is_empty())
            })
            .map(str::to_string);
        let items = pending_mails
            .into_iter()
            .map(|mail| TurnInput::InterAgentCommunication(mail.communication))
            .collect();
        (items, start_options)
    }

    pub(crate) async fn turn_state_for_sub_id(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) -> Option<Arc<Mutex<TurnState>>> {
        let active = active_turn.lock().await;
        active.as_ref().and_then(|active_turn| {
            active_turn
                .task
                .as_ref()
                .is_some_and(|task| task.turn_context.sub_id == sub_id)
                .then(|| Arc::clone(&active_turn.turn_state))
        })
    }

    /// Signal once a user message is queued for this sampling request.
    pub(crate) async fn watch_user_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
        interrupt: CancellationToken,
    ) -> Option<AbortOnDropHandle<()>> {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await?;
        // Subscribe before inspecting the queue so an arrival cannot be missed.
        let mut activity = self.activity_tx.subscribe();
        Some(AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                if turn_state.lock().await.pending_input.has_user_input() {
                    interrupt.cancel();
                    return;
                }
                if activity.changed().await.is_err() {
                    return;
                }
            }
        })))
    }

    /// Clear any pending waiters and input buffered for the current turn.
    ///
    /// In wake mode, unconsumed agent mail returns to the mailbox, where the next turn delivers it
    /// or a pause holds it. An unpaused interrupt drops it instead, so an interrupted
    /// `followup_task` does not restart a subagent. Outside wake mode it is dropped, as upstream
    /// does. Returns whether any mail was returned.
    pub(crate) async fn clear_pending(
        &self,
        active_turn: &ActiveTurn,
        reason: &TurnAbortReason,
        mode: ChildReportMode,
    ) -> bool {
        let mut items = {
            let mut turn_state = active_turn.turn_state.lock().await;
            turn_state.clear_pending_waiters();
            std::mem::take(&mut turn_state.pending_input.items)
        };
        if mode != ChildReportMode::WakeOnReport
            || (!self.wakeups_paused() && *reason == TurnAbortReason::Interrupted)
        {
            return false;
        }
        self.return_to_mailbox(&mut items).await
    }

    /// Takes the agent mail from the active turn's pending input, in order, and leaves the rest.
    pub(crate) async fn take_pending_agent_mail(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> Vec<TurnInput> {
        let turn_state = active_turn
            .lock()
            .await
            .as_ref()
            .map(|turn| Arc::clone(&turn.turn_state));
        let Some(turn_state) = turn_state else {
            return Vec::new();
        };
        let mut turn_state = turn_state.lock().await;
        let (mail, rest) = std::mem::take(&mut turn_state.pending_input.items)
            .into_iter()
            .partition(|item| matches!(item, TurnInput::InterAgentCommunication(_)));
        turn_state.pending_input.items = rest;
        mail
    }

    /// Moves the agent mail in `items` back to the front of the mailbox, in order, and leaves the
    /// other input in `items`. Returns whether any mail was returned.
    pub(crate) async fn return_to_mailbox(&self, items: &mut Vec<TurnInput>) -> bool {
        let mut returned = Vec::new();
        for item in std::mem::take(items) {
            match item {
                TurnInput::InterAgentCommunication(communication) => returned.push(communication),
                other => items.push(other),
            }
        }
        let any_returned = !returned.is_empty();
        let mut mails = self.mailbox_pending_mails.lock().await;
        for communication in returned.into_iter().rev() {
            mails.push_front(PendingMailboxCommunication {
                communication,
                start_options: TurnStartOptions::default(),
                _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
            });
        }
        any_returned
    }

    pub(crate) async fn defer_mailbox_delivery_to_next_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        let mut turn_state = turn_state.lock().await;
        // Explicit same-turn work still needs a follow-up. Queue-only child mail does not: keep
        // it pending so task completion records it for the next turn without sampling again.
        if turn_state.pending_input.items.iter().any(|input| {
            !matches!(
                input,
                TurnInput::InterAgentCommunication(communication) if !communication.trigger_turn
            )
        }) {
            return;
        }
        turn_state.set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);
    }

    pub(crate) async fn accept_mailbox_delivery_for_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        self.accept_mailbox_delivery_for_turn_state(turn_state.as_ref())
            .await;
    }

    pub(super) async fn accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) {
        turn_state
            .lock()
            .await
            .accept_mailbox_delivery_for_current_turn();
    }

    pub(super) async fn extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        {
            let mut turn_state = turn_state.lock().await;
            turn_state.pending_input.items.extend(input);
            turn_state.accept_mailbox_delivery_for_current_turn();
        }
        self.activity_tx.send_replace(InputQueueActivity::Steer);
    }

    pub(crate) async fn extend_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        turn_state.lock().await.pending_input.items.extend(input);
    }

    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.pending_input.items.split_off(0)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> (Vec<TurnInput>, TurnStartOptions) {
        let (pending_input, accepts_mailbox_delivery) = {
            let mut active = active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let mut turn_state = active_turn.turn_state.lock().await;
                    let accepts_mailbox_delivery =
                        turn_state.accepts_mailbox_delivery_for_current_turn();
                    let pending_input = if accepts_mailbox_delivery {
                        turn_state.pending_input.items.split_off(0)
                    } else {
                        Vec::new()
                    };
                    (pending_input, accepts_mailbox_delivery)
                }
                None => (Vec::new(), true),
            }
        };
        if !accepts_mailbox_delivery {
            return (pending_input, TurnStartOptions::default());
        }
        let (mailbox_items, start_options) = self.drain_mailbox_input_items().await;
        if pending_input.is_empty() {
            (mailbox_items, start_options)
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            (pending_input, start_options)
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn has_pending_input(&self, active_turn: &Mutex<Option<ActiveTurn>>) -> bool {
        let (has_turn_pending_input, accepts_mailbox_delivery) = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    (
                        !turn_state.pending_input.is_empty(),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (false, true),
            }
        };
        if !accepts_mailbox_delivery {
            return false;
        }
        if has_turn_pending_input {
            return true;
        }
        self.has_pending_mailbox_items().await
    }
}

impl TurnInputQueue {
    fn has_user_input(&self) -> bool {
        self.items
            .iter()
            .any(|input| matches!(input, TurnInput::UserInput { .. }))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn pending_activity(&self) -> Option<InputQueueActivity> {
        if self.items.iter().any(|input| {
            matches!(
                input,
                TurnInput::UserInput { .. } | TurnInput::FunctionCallOutput(_)
            )
        }) {
            Some(InputQueueActivity::Steer)
        } else if self
            .items
            .iter()
            .any(|input| matches!(input, TurnInput::InterAgentCommunication(_)))
        {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::CodexHarnessMetadata;
    use codex_protocol::AgentPath;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[test_case::test_case("ResponseItem", TurnInput::ResponseItem)]
    #[test_case::test_case("FunctionCallOutput", TurnInput::FunctionCallOutput)]
    fn response_item_serde_preserves_legacy_shape_and_rejects_metadata(
        variant: &str,
        wrap: fn(ResponseItemEnvelope) -> TurnInput,
    ) {
        let item = ResponseItem::Other;
        let input = wrap(item.clone().into());
        let value = serde_json::json!({variant: item});

        assert_eq!(serde_json::to_value(&input).unwrap(), value);
        assert_eq!(serde_json::from_value::<TurnInput>(value).unwrap(), input);

        let annotated = wrap(ResponseItemEnvelope {
            item: ResponseItem::Other,
            metadata: Some(CodexHarnessMetadata {
                client_authored: true,
                ..Default::default()
            }),
        });
        assert!(serde_json::to_value(annotated).is_err());

        let forged = serde_json::json!({
            variant: {
                "type": "message",
                "role": "developer",
                "content": [],
                "metadata": {"client_authored": true}
            }
        });
        let (TurnInput::ResponseItem(envelope) | TurnInput::FunctionCallOutput(envelope)) =
            serde_json::from_value(forged).unwrap()
        else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());

        let forged_configuration = serde_json::json!({
            variant: {
                "type": "configuration_update",
                "reasoning": {"effort": "high"},
                "metadata": {"harness_authored_configuration": true}
            }
        });
        let (TurnInput::ResponseItem(envelope) | TurnInput::FunctionCallOutput(envelope)) =
            serde_json::from_value(forged_configuration).unwrap()
        else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());
    }

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    #[tokio::test]
    async fn input_queue_notifies_mailbox_subscribers() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);

        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_one, Default::default())
            .await;
        let mail_two = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_two, Default::default())
            .await;

        activity_rx.changed().await.expect("mailbox update");
        assert_eq!(
            *activity_rx.borrow_and_update(),
            InputQueueActivity::Mailbox
        );
    }

    #[tokio::test]
    async fn mailbox_report_commit_removes_rejected_delivery_before_notification() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);
        activity_rx.borrow_and_update();
        let mut rejected_mail = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "terminal report",
            /*trigger_turn*/ true,
        );
        rejected_mail.id = Some(codex_protocol::ResponseItemId::with_suffix(
            "amsg",
            uuid::Uuid::now_v7(),
        ));

        assert!(
            !input_queue
                .enqueue_mailbox_communication_if(rejected_mail, Default::default(), || false,)
                .await
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
        assert!(
            !activity_rx
                .has_changed()
                .expect("queue sender remains open")
        );

        let mut accepted_mail = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "terminal report",
            /*trigger_turn*/ true,
        );
        accepted_mail.id = Some(codex_protocol::ResponseItemId::with_suffix(
            "amsg",
            uuid::Uuid::now_v7(),
        ));
        assert!(
            input_queue
                .enqueue_mailbox_communication_if(accepted_mail, Default::default(), || true,)
                .await
        );
        assert_eq!(input_queue.trigger_turn_mailbox_count().await, 1);
        assert!(
            activity_rx
                .has_changed()
                .expect("queue sender remains open")
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_steer_subscribers() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
        assert_eq!(pending_activity, None);

        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    metadata: Default::default(),
                    content: vec![UserInput::Text {
                        text: "steer".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        activity_rx.changed().await.expect("steer update");
        assert_eq!(*activity_rx.borrow_and_update(), InputQueueActivity::Steer);
    }

    #[tokio::test]
    async fn input_queue_reports_already_pending_steer() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let passive_output = serde_json::from_value(serde_json::json!({
            "ResponseItem": {"type": "function_call_output", "name": "notify", "output": "passive"}
        }))
        .unwrap();
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![passive_output])
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            None
        );
        let communication = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "already pending mail",
            /*trigger_turn*/ false,
        );
        input_queue
            .extend_pending_input_for_turn_state(
                &turn_state,
                vec![TurnInput::InterAgentCommunication(communication)],
            )
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            Some(InputQueueActivity::Mailbox)
        );
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    metadata: Default::default(),
                    content: vec![UserInput::Text {
                        text: "already pending".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        let (_activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;

        assert_eq!(pending_activity, Some(InputQueueActivity::Steer));
    }

    #[tokio::test]
    async fn input_queue_drains_mailbox_in_delivery_order() {
        let input_queue = InputQueue::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ true,
        );

        input_queue
            .enqueue_mailbox_communication(mail_one.clone(), Default::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone(), Default::default())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two)
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_uses_unambiguous_trigger_parent_and_first_root() {
        let (parent, peer, root, root2) = (Some("a"), Some("b"), Some("r"), Some("s"));
        for (pending_mails, expected_parent_turn_id, expected_root_turn_id) in [
            (Vec::new(), None, None),
            (vec![(false, Some("q"), root)], None, None),
            (vec![(true, Some(""), root)], None, None),
            (vec![(true, Some("   "), root)], None, None),
            (vec![(true, None, root)], None, None),
            (vec![(true, parent, None)], parent, None),
            (vec![(true, parent, Some(""))], parent, None),
            (vec![(true, parent, root), (true, peer, root)], None, root),
            (vec![(true, parent, root), (true, peer, root2)], None, root),
            (vec![(true, parent, root), (true, None, root)], None, root),
            (
                vec![(true, parent, root), (true, parent, root)],
                parent,
                root,
            ),
            (
                vec![(false, Some("q"), root2), (true, parent, root)],
                parent,
                root,
            ),
        ] {
            let input_queue = InputQueue::new();
            for (trigger_turn, parent_turn_id, root_turn_id) in pending_mails {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            parent_turn_id: parent_turn_id.map(str::to_string),
                            root_turn_id: root_turn_id.map(str::to_string),
                            ..Default::default()
                        },
                    )
                    .await;
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(
                start_options.parent_turn_id.as_deref(),
                expected_parent_turn_id
            );
            assert_eq!(start_options.root_turn_id.as_deref(), expected_root_turn_id);
        }
    }

    #[tokio::test]
    async fn input_queue_uses_latest_followup_choice_and_ignores_queue_only_mail() {
        use codex_protocol::turn_input::CyberAccessProgram;

        for latest in [Some(CyberAccessProgram::Standard), None] {
            let input_queue = InputQueue::new();
            for (trigger_turn, program) in [
                (true, Some(CyberAccessProgram::DaybreakBlue)),
                (true, latest),
                (false, Some(CyberAccessProgram::DaybreakRed)),
            ] {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            cyber_access_program: program,
                            ..Default::default()
                        },
                    )
                    .await;
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(start_options.cyber_access_program, latest);
        }
    }

    #[tokio::test]
    async fn input_queue_tracks_pending_trigger_turn_mail() {
        let input_queue = InputQueue::new();

        let queued_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "queued",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(queued_mail, Default::default())
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        let trigger_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(trigger_mail, Default::default())
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }

    /// In wake mode, unconsumed child mail survives a paused or replaced turn, in order and exactly
    /// once. An unpaused interrupt drops it, so an interrupted `followup_task` does not restart a
    /// subagent.
    #[tokio::test]
    async fn clear_pending_returns_unconsumed_mail_unless_interrupted_unpaused() {
        let worker = AgentPath::try_from("/root/worker").expect("agent path");
        let mail = |content, trigger_turn| {
            TurnInput::InterAgentCommunication(make_mail(
                worker.clone(),
                AgentPath::root(),
                content,
                trigger_turn,
            ))
        };
        let (report, progress, later) = (
            mail("report", /*trigger_turn*/ true),
            mail("progress", /*trigger_turn*/ false),
            mail("later", /*trigger_turn*/ true),
        );
        let steer = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "steer".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
            metadata: Default::default(),
        };
        let returned = vec![report.clone(), progress.clone(), later.clone()];
        let wake = ChildReportMode::WakeOnReport;
        for (paused, reason, mode, expected) in [
            (true, TurnAbortReason::Interrupted, wake, returned.clone()),
            (false, TurnAbortReason::Replaced, wake, returned.clone()),
            (
                false,
                TurnAbortReason::Interrupted,
                wake,
                vec![later.clone()],
            ),
            // Outside wake mode a replaced turn drops its unread mail, as upstream does.
            (
                false,
                TurnAbortReason::Replaced,
                ChildReportMode::WaitAgent,
                vec![later.clone()],
            ),
        ] {
            let input_queue = InputQueue::new();
            let active_turn = ActiveTurn::default();
            input_queue
                .extend_pending_input_for_turn_state(
                    active_turn.turn_state.as_ref(),
                    vec![report.clone(), steer.clone(), progress.clone()],
                )
                .await;
            let TurnInput::InterAgentCommunication(later_mail) = later.clone() else {
                unreachable!("later is agent mail");
            };
            input_queue
                .enqueue_mailbox_communication(later_mail, Default::default())
                .await;
            if paused {
                input_queue.pause_wakeups();
            }

            let returned_mail = input_queue.clear_pending(&active_turn, &reason, mode).await;
            input_queue.resume_wakeups();
            let delivered = input_queue.drain_mailbox_input_items().await.0;
            let left_in_turn = input_queue
                .take_pending_input_for_turn_state(active_turn.turn_state.as_ref())
                .await;
            let redelivered = input_queue.drain_mailbox_input_items().await.0;

            assert_eq!(
                (returned_mail, delivered, left_in_turn, redelivered),
                (expected.len() > 1, expected, Vec::new(), Vec::new()),
                "paused={paused} reason={reason:?} mode={mode:?}"
            );
        }
    }

    /// Held trigger-turn mail has nothing to deliver, so it must not interrupt a `sleep`.
    #[tokio::test]
    async fn paused_wakeups_hold_trigger_mail_without_activity() {
        let worker = AgentPath::try_from("/root/worker").expect("agent path");
        let input_queue = InputQueue::new();
        input_queue.pause_wakeups();
        let (activity_rx, _) = input_queue.subscribe_activity(/*turn_state*/ None).await;

        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    worker.clone(),
                    AgentPath::root(),
                    "report",
                    /*trigger_turn*/ true,
                ),
                Default::default(),
            )
            .await;
        let held = (
            activity_rx.has_changed().expect("activity sender"),
            input_queue.subscribe_activity(/*turn_state*/ None).await.1,
        );
        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    worker,
                    AgentPath::root(),
                    "progress",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;

        assert_eq!(
            (held, activity_rx.has_changed().expect("activity sender")),
            ((false, None), true)
        );
    }

    /// A task that never read its input returns all of its agent mail, queue-only included, so a
    /// later progress message is never delivered ahead of an earlier report. Other input stays.
    #[tokio::test]
    async fn return_to_mailbox_returns_all_agent_mail_in_order() {
        let worker = AgentPath::try_from("/root/worker").expect("agent path");
        let mail = |content, trigger_turn| {
            TurnInput::InterAgentCommunication(make_mail(
                worker.clone(),
                AgentPath::root(),
                content,
                trigger_turn,
            ))
        };
        let (progress, report, later) = (
            mail("progress", /*trigger_turn*/ false),
            mail("report", /*trigger_turn*/ true),
            mail("later", /*trigger_turn*/ true),
        );
        let steer = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "steer".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
            metadata: Default::default(),
        };
        let input_queue = InputQueue::new();
        let TurnInput::InterAgentCommunication(later_mail) = later.clone() else {
            unreachable!("later is agent mail");
        };
        input_queue
            .enqueue_mailbox_communication(later_mail, Default::default())
            .await;
        let mut items = vec![progress.clone(), steer.clone(), report.clone()];

        let returned = input_queue.return_to_mailbox(&mut items).await;
        let delivered = input_queue.drain_mailbox_input_items().await.0;

        assert_eq!(
            (returned, items, delivered),
            (true, vec![steer], vec![progress, report, later])
        );
    }
}
