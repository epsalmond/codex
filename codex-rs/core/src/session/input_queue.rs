use crate::agent::api::AgentControl;
use crate::agent_communication::PENDING_MAILBOX_MESSAGES;
use crate::session::multi_agents::ChildReportMode;
use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_diagnostics::GaugeGuard;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

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
    InterAgentCommunication(crate::PendingAgentMessage),
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
    /// Response items queued through a notifying injection, unlike passive context.
    has_pending_injection: bool,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    controller: Option<(ThreadId, Arc<dyn AgentControl>, watch::Receiver<bool>)>,
    mailbox_pending_mails: Mutex<VecDeque<PendingMailboxCommunication>>,
    /// Holds trigger-turn mail until an explicit resume. See [`WakeupState`].
    wakeups: AtomicU8,
}

/// Why automatic wakeups are held. Both states clear on the next user turn or explicit followup.
///
/// A user pause (Esc, an aborted approval, a failed turn) holds child reports and other
/// trigger-turn mail, but queue-only mail still wakes an outstanding durable sleep. A rejected
/// automatic start also holds that mail and suppresses every automatic start, so a deterministic
/// startup failure is not repeated by later mail.
#[repr(u8)]
enum WakeupState {
    Active = 0,
    Paused = 1,
    StartFailed = 2,
}

pub(crate) struct PendingMailboxCommunication {
    pub(crate) communication: crate::PendingAgentMessage,
    start_options: TurnStartOptions,
    _diagnostics_guard: GaugeGuard,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            controller: None,
            mailbox_pending_mails: Mutex::new(VecDeque::new()),
            wakeups: AtomicU8::new(WakeupState::Active as u8),
        }
    }

    pub(crate) fn with_controller(thread_id: ThreadId, control: Arc<dyn AgentControl>) -> Self {
        let updates = control.watch_mailbox(thread_id);
        Self {
            controller: Some((thread_id, control, updates)),
            ..Self::new()
        }
    }

    pub(crate) fn mailbox_updates(&self) -> Option<watch::Receiver<bool>> {
        self.controller
            .as_ref()
            .map(|(_, _, updates)| updates.clone())
    }

    pub(crate) fn notify_mailbox(&self) {
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    fn read_mailbox(&self, pending: &mut VecDeque<PendingMailboxCommunication>) {
        if let Some((thread_id, control, _)) = &self.controller {
            pending.extend(
                control
                    .take_mailbox(*thread_id)
                    .into_iter()
                    .map(|communication| PendingMailboxCommunication {
                        communication,
                        start_options: TurnStartOptions::default(),
                        _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
                    }),
            );
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
        expected_turn_id: Option<&str>,
    ) -> bool {
        let active = active_turn.lock().await;
        let Some(active_turn) = active.as_ref().filter(|turn| {
            turn.task.as_ref().is_some_and(|task| {
                expected_turn_id.is_none_or(|id| {
                    task.turn_context.sub_id == id && !task.cancellation_token.is_cancelled()
                })
            })
        }) else {
            return false;
        };
        let mut turn_state = active_turn.turn_state.lock().await;
        if !turn_state.accepts_mailbox_delivery_for_current_turn() {
            return false;
        }
        turn_state
            .pending_input
            .items
            .push(TurnInput::InterAgentCommunication(communication.into()));
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        true
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: crate::PendingAgentMessage,
        start_options: TurnStartOptions,
    ) {
        self.enqueue_mailbox_communication_if(communication, start_options, || true)
            .await;
    }

    /// Inserts mail and synchronously commits its delivery while the queue is locked.
    /// Rejected handoffs are removed before any consumer can observe them.
    pub(crate) async fn enqueue_mailbox_communication_if(
        &self,
        communication: crate::PendingAgentMessage,
        start_options: TurnStartOptions,
        accept: impl FnOnce() -> bool,
    ) -> bool {
        let held = communication.trigger_turn && self.wakeups_paused();
        let mut pending = self.mailbox_pending_mails.lock().await;
        // Mail retained while unloaded precedes new submissions to the loaded session.
        self.read_mailbox(&mut pending);
        pending.push_back(PendingMailboxCommunication {
            communication,
            start_options,
            _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
        });
        if !accept() {
            // Extension delivery attachments may need the mailbox lock during destruction.
            let rejected = pending.pop_back();
            drop(pending);
            drop(rejected);
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
            || self
                .controller
                .as_ref()
                .is_some_and(|(_, _, updates)| *updates.borrow())
    }

    /// Returns whether ready mail can wake an outstanding durable sleep.
    ///
    /// Unless `sibling_wake` is set, only mail between agents in one lineage (an ancestor or a
    /// descendant of the recipient) wakes the sleeper. Sibling and cousin mail stays queued for
    /// the recipient's next turn.
    pub(crate) async fn has_durable_sleep_wake_mailbox_items(&self, sibling_wake: bool) -> bool {
        let paused = self.wakeups_paused();
        let mut pending = self.mailbox_pending_mails.lock().await;
        self.read_mailbox(&mut pending);
        pending.iter().any(|mail| {
            let communication = &mail.communication;
            !(paused && communication.trigger_turn)
                && (sibling_wake
                    || communication
                        .author
                        .is_ancestor_of(&communication.recipient)
                    || communication
                        .recipient
                        .is_ancestor_of(&communication.author))
        })
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

    /// Holds trigger-turn mail until an explicit resume. Keeps a stronger failed-start state.
    pub(crate) fn pause_wakeups(&self) {
        self.wakeups
            .fetch_max(WakeupState::Paused as u8, Ordering::SeqCst);
    }

    /// Holds trigger-turn mail and suppresses every automatic start until an explicit resume.
    pub(crate) fn suppress_automatic_starts(&self) {
        self.wakeups
            .store(WakeupState::StartFailed as u8, Ordering::SeqCst);
    }

    /// Clears the pause and returns whether wakeups were paused.
    pub(crate) fn resume_wakeups(&self) -> bool {
        self.wakeups
            .swap(WakeupState::Active as u8, Ordering::SeqCst)
            != WakeupState::Active as u8
    }

    pub(crate) fn wakeups_paused(&self) -> bool {
        self.wakeups.load(Ordering::SeqCst) != WakeupState::Active as u8
    }

    /// Returns whether a rejected automatic start suppresses every automatic start.
    pub(crate) fn automatic_starts_suppressed(&self) -> bool {
        self.wakeups.load(Ordering::SeqCst) == WakeupState::StartFailed as u8
    }

    /// Drains all mail, including mail held by paused wakeups.
    pub(crate) async fn drain_mailbox(&self) -> Vec<PendingMailboxCommunication> {
        let mut pending = self.mailbox_pending_mails.lock().await;
        self.read_mailbox(&mut pending);
        pending.drain(..).collect()
    }

    /// Drains deliverable mail. Paused wakeups leave trigger-turn mail queued.
    pub(crate) async fn drain_mailbox_input_items(&self) -> (Vec<TurnInput>, TurnStartOptions) {
        let pending_mails = {
            let paused = self.wakeups_paused();
            let mut mails = self.mailbox_pending_mails.lock().await;
            self.read_mailbox(&mut mails);
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

    /// Attach startup mail while the turn state is locked, without a drain-to-attach await.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "mailbox removal and its recovery copy must share one cancellation boundary"
    )]
    pub(crate) async fn drain_mailbox_into_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        reserved_start_options: &TurnStartOptions,
    ) -> TurnStartOptions {
        let mut turn_state = turn_state.lock().await;
        let (items, drained_start_options) = self.drain_mailbox_input_items().await;
        let start_options = if reserved_start_options.parent_turn_id.is_some()
            || reserved_start_options.root_turn_id.is_some()
        {
            reserved_start_options.clone()
        } else {
            drained_start_options
        };
        turn_state.pending_input.items.extend(items);
        turn_state.mail_start_options = start_options.clone();
        start_options
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
    /// Unconsumed agent mail returns to the front of the mailbox in order; other input is dropped.
    /// In wake mode the next turn delivers it or a pause holds it, and an unpaused interrupt drops
    /// it instead, so an interrupted `followup_task` does not restart a subagent. In polling mode
    /// it is always kept for the next explicit turn: unless a replacing turn takes it, returning
    /// mail pauses wakeups, so the stop cannot restart on it. Returns whether any mail was
    /// returned.
    pub(crate) async fn clear_pending(
        &self,
        active_turn: &ActiveTurn,
        reason: &TurnAbortReason,
        mode: ChildReportMode,
    ) -> bool {
        let (mail_start_options, mut items) = {
            let mut turn_state = active_turn.turn_state.lock().await;
            turn_state.clear_pending_waiters();
            (
                // Mail in pending input stopped carrying its own options when the turn drained
                // it, so return it with the options the turn was started with.
                turn_state.mail_start_options.clone(),
                std::mem::take(&mut turn_state.pending_input).items,
            )
        };
        match mode {
            ChildReportMode::WakeOnReport => {
                if !self.wakeups_paused() && *reason == TurnAbortReason::Interrupted {
                    return false;
                }
            }
            ChildReportMode::WaitAgent => {
                if *reason != TurnAbortReason::Replaced
                    && items
                        .iter()
                        .any(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
                {
                    self.pause_wakeups();
                }
            }
        }
        self.return_to_mailbox_with_start_options(&mut items, mail_start_options)
            .await
    }

    /// Takes the agent mail from the active turn's pending input, in order, and leaves the rest.
    /// One copy of every taken mail item stays in pending input (in take order): a hard abort
    /// before the copies are consumed at recording recovers the mail through `clear_pending`
    /// instead of losing it.
    pub(crate) async fn take_pending_agent_mail_keeping_unrecorded_mail(
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
        let (mail, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut turn_state.pending_input.items)
            .into_iter()
            .partition(|item| matches!(item, TurnInput::InterAgentCommunication(_)));
        turn_state.pending_input.items = rest.into_iter().chain(mail.iter().cloned()).collect();
        mail
    }

    /// Moves the agent mail in `items` back to the front of the mailbox, in order, and leaves the
    /// other input in `items`. Returns whether any mail was returned.
    pub(crate) async fn return_to_mailbox(&self, items: &mut Vec<TurnInput>) -> bool {
        self.return_to_mailbox_with_start_options(items, TurnStartOptions::default())
            .await
    }

    /// Restores accepted mail after failed startup, preserving its selected turn settings.
    pub(crate) async fn return_to_mailbox_with_start_options(
        &self,
        items: &mut Vec<TurnInput>,
        start_options: TurnStartOptions,
    ) -> bool {
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
                start_options: start_options.clone(),
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
            turn_state.pending_input.has_pending_injection |= input
                .iter()
                .any(|input| matches!(input, TurnInput::ResponseItem(_)));
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
        std::mem::take(&mut turn_state.lock().await.pending_input).items
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
                        std::mem::take(&mut turn_state.pending_input).items
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

    /// Takes the turn's pending input and drains deliverable mail for hook inspection, like
    /// `get_pending_input` does, while leaving every not-yet-recorded `InterAgentCommunication`
    /// in the turn's pending input at the same position of the drain order.
    ///
    /// Recording is the consuming step (`consume_unrecorded_mail`), so a hard abort between
    /// drain and record leaves the untouched mail reachable by `clear_pending`, which returns
    /// it to the mailbox instead of dropping it. Turn-start mail uses the same copy-held
    /// contract through `take_pending_agent_mail_keeping_unrecorded_mail`.
    ///
    /// Mail drained from the mailbox mid-turn stops carrying its own options too: the turn
    /// last accepted it, so `mail_start_options` moves to the drained batch's lineage.
    pub(crate) async fn drain_pending_input_keeping_unrecorded_mail(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> Vec<TurnInput> {
        let Some((mut batch, turn_state, batch_mail_count)) = self
            .take_pending_input_keeping_unrecorded_mail(active_turn)
            .await
        else {
            return Vec::new();
        };
        self.attach_drained_mailbox_batch(active_turn, &turn_state, batch_mail_count, &mut batch)
            .await;
        batch
    }

    /// Takes the turn's pending input for recording, leaving one copy of every not-yet-recorded
    /// mail item at the front of pending input: a hard abort between the take and its recordings
    /// must find the mail through `clear_pending`. Returns the batch, the turn state holding the
    /// copies, and how many pending mail items the batch owns, or `None` when there is no turn
    /// to drain or the turn does not accept mailbox delivery right now.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    async fn take_pending_input_keeping_unrecorded_mail(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> Option<(Vec<TurnInput>, Arc<Mutex<TurnState>>, usize)> {
        let active = active_turn.lock().await;
        let active_turn = active.as_ref()?;
        let turn_state = Arc::clone(&active_turn.turn_state);
        let mut turn_state_guard = turn_state.lock().await;
        if !turn_state_guard.accepts_mailbox_delivery_for_current_turn() {
            return None;
        }
        let batch = std::mem::take(&mut turn_state_guard.pending_input).items;
        let batch_mail_count = batch
            .iter()
            .filter(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
            .count();
        turn_state_guard.pending_input.items = batch
            .iter()
            .filter(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
            .cloned()
            .collect();
        drop(turn_state_guard);
        Some((batch, turn_state, batch_mail_count))
    }

    /// Drains deliverable mailbox mail into the drained batch, appending one unrecorded copy per
    /// item directly behind the batch's pending-input copies. The copies stay a prefix of pending
    /// input in drain order, so recording consumes them before any mail that lands in the turn
    /// state mid-drain (a same-window message-board delivery appends at the end) can shift one
    /// copy and lose another.
    ///
    /// If the active turn changed or stopped accepting mailbox delivery, mail stays in the
    /// mailbox with its own start options. Returns whether the batch grew.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    async fn attach_drained_mailbox_batch(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        turn_state: &Arc<Mutex<TurnState>>,
        batch_mail_count: usize,
        batch: &mut Vec<TurnInput>,
    ) -> bool {
        // Acquire every contended turn lock before taking mail out of the mailbox. Once the
        // mailbox drain completes, attaching its protection copies has no cancellation point.
        let active = active_turn.lock().await;
        if !active
            .as_ref()
            .is_some_and(|turn| Arc::ptr_eq(&turn.turn_state, turn_state))
        {
            return false;
        }
        let mut turn_state_guard = turn_state.lock().await;
        if !turn_state_guard.accepts_mailbox_delivery_for_current_turn() {
            return false;
        }
        let (mailbox_items, start_options) = self.drain_mailbox_input_items().await;
        if mailbox_items.is_empty() {
            return false;
        }
        // Mail delivered into the turn after the pending-input take trails the whole batch.
        let insert_at = turn_state_guard
            .pending_input
            .items
            .iter()
            .take(batch_mail_count)
            .filter(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
            .count();
        turn_state_guard
            .pending_input
            .items
            .splice(insert_at..insert_at, mailbox_items.iter().cloned());
        // Queue-only updates do not replace the requester lineage of mail already held.
        if mailbox_items.iter().any(
            |item| matches!(item, TurnInput::InterAgentCommunication(mail) if mail.trigger_turn),
        ) {
            turn_state_guard.mail_start_options = start_options;
        }
        batch.extend(mailbox_items);
        true
    }

    /// Consumes the next unrecorded mail item the turn's pending input holds, matching the
    /// drain order of `drain_pending_input_keeping_unrecorded_mail`. Call this at the point of
    /// recording so a mid-record abort cannot leave the same mail both in history and in the
    /// queue.
    pub(crate) async fn consume_unrecorded_mail(&self, active_turn: &Mutex<Option<ActiveTurn>>) {
        let turn_state = active_turn
            .lock()
            .await
            .as_ref()
            .map(|turn| Arc::clone(&turn.turn_state));
        let Some(turn_state) = turn_state else {
            return;
        };
        let mut turn_state = turn_state.lock().await;
        turn_state.pending_input.consume_unrecorded_mail();
    }

    /// Mail start options the turn would return pending mail with, defaults when no turn.
    pub(crate) async fn turn_mail_start_options(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> TurnStartOptions {
        let Some(turn_state) = active_turn
            .lock()
            .await
            .as_ref()
            .map(|turn| Arc::clone(&turn.turn_state))
        else {
            return TurnStartOptions::default();
        };
        turn_state.lock().await.mail_start_options.clone()
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
    pub(super) fn consume_unrecorded_mail(&mut self) {
        if let Some(index) = self
            .items
            .iter()
            .position(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
        {
            self.items.remove(index);
        }
    }

    fn has_user_input(&self) -> bool {
        self.items
            .iter()
            .any(|input| matches!(input, TurnInput::UserInput { .. }))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn pending_activity(&self) -> Option<InputQueueActivity> {
        if self.has_pending_injection
            || self.items.iter().any(|input| {
                matches!(
                    input,
                    TurnInput::UserInput { .. } | TurnInput::FunctionCallOutput(_)
                )
            })
        {
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
            .enqueue_mailbox_communication(mail_one.into(), Default::default())
            .await;
        let mail_two = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_two.into(), Default::default())
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
                .enqueue_mailbox_communication_if(rejected_mail.into(), Default::default(), || {
                    false
                })
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
                .enqueue_mailbox_communication_if(accepted_mail.into(), Default::default(), || true)
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
        let passive_output: TurnInput = serde_json::from_value(serde_json::json!({
            "ResponseItem": {"type": "function_call_output", "name": "notify", "output": "passive"}
        }))
        .unwrap();
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![passive_output.clone()])
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            None
        );
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![passive_output.clone()],
            )
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            Some(InputQueueActivity::Steer)
        );
        // Consuming the injection must not make later passive context wake a wait.
        input_queue
            .take_pending_input_for_turn_state(&turn_state)
            .await;
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
                vec![TurnInput::InterAgentCommunication(communication.into())],
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
            .enqueue_mailbox_communication(mail_one.clone().into(), Default::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone().into(), Default::default())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![
                TurnInput::InterAgentCommunication(mail_one.into()),
                TurnInput::InterAgentCommunication(mail_two.into())
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
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn)
                            .into(),
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
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn)
                            .into(),
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
            .enqueue_mailbox_communication(queued_mail.into(), Default::default())
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        let trigger_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(trigger_mail.into(), Default::default())
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }

    /// Durable sleep wakes on mail from the recipient's lineage only, unless `sibling_wake` is set.
    #[tokio::test]
    async fn durable_sleep_wakes_only_on_lineage_mail() {
        let path = |value: &str| AgentPath::try_from(value).expect("agent path");
        let lead = path("/root/lead");
        for (author, sibling_wake, expected) in [
            (path("/root/peer"), false, false),
            (path("/root/peer/helper"), false, false),
            (path("/root/peer"), true, true),
            (AgentPath::root(), false, true),
            (path("/root/lead/worker"), false, true),
        ] {
            let input_queue = InputQueue::new();
            input_queue
                .enqueue_mailbox_communication(
                    make_mail(
                        author.clone(),
                        lead.clone(),
                        "note",
                        /*trigger_turn*/ false,
                    )
                    .into(),
                    Default::default(),
                )
                .await;
            assert_eq!(
                input_queue
                    .has_durable_sleep_wake_mailbox_items(sibling_wake)
                    .await,
                expected,
                "author={author} sibling_wake={sibling_wake}"
            );
            assert!(input_queue.has_pending_mailbox_items().await);
        }
    }

    /// In wake mode, unconsumed child mail survives a paused or replaced turn, in order and exactly
    /// once. An unpaused interrupt drops it, so an interrupted `followup_task` does not restart a
    /// subagent. Polling mode always keeps it.
    #[tokio::test]
    async fn clear_pending_returns_unconsumed_mail_unless_interrupted_unpaused() {
        let worker = AgentPath::try_from("/root/worker").expect("agent path");
        let mail = |content, trigger_turn| {
            TurnInput::InterAgentCommunication(
                make_mail(worker.clone(), AgentPath::root(), content, trigger_turn).into(),
            )
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
            // Polling mode keeps unread mail for the next explicit turn.
            (
                false,
                TurnAbortReason::Replaced,
                ChildReportMode::WaitAgent,
                returned.clone(),
            ),
            (
                false,
                TurnAbortReason::Interrupted,
                ChildReportMode::WaitAgent,
                returned.clone(),
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

    /// A user pause holds trigger-turn mail but leaves automatic starts possible; a rejected start
    /// also suppresses them. A later pause keeps the suppression, and one explicit resume clears
    /// either state.
    #[test]
    fn user_pause_and_failed_start_are_distinct_wakeup_states() {
        let input_queue = InputQueue::new();
        let state =
            |queue: &InputQueue| (queue.wakeups_paused(), queue.automatic_starts_suppressed());
        let mut observed = vec![state(&input_queue)];
        input_queue.pause_wakeups();
        observed.push(state(&input_queue));
        input_queue.suppress_automatic_starts();
        observed.push(state(&input_queue));
        input_queue.pause_wakeups();
        observed.push(state(&input_queue));
        let resumed = input_queue.resume_wakeups();
        observed.push(state(&input_queue));
        let resumed_again = input_queue.resume_wakeups();

        assert_eq!(
            (observed, resumed, resumed_again),
            (
                vec![
                    (false, false),
                    (true, false),
                    (true, true),
                    (true, true),
                    (false, false),
                ],
                true,
                false,
            )
        );
    }

    /// Polling mode keeps accepted mail across an interrupt and pauses wakeups, so an interrupted
    /// `followup_task` waits for an explicit turn; queue-only mail stays deliverable. A replacing
    /// turn takes the mail itself without a pause. Steers are dropped.
    #[tokio::test]
    async fn polling_clear_pending_holds_returned_trigger_mail_until_explicit_turn() {
        let parent = AgentPath::try_from("/root/worker").expect("agent path");
        let worker = AgentPath::try_from("/root/worker/helper").expect("agent path");
        let mail = |content, trigger_turn| {
            TurnInput::InterAgentCommunication(
                make_mail(parent.clone(), worker.clone(), content, trigger_turn).into(),
            )
        };
        let followup = mail("followup", /*trigger_turn*/ true);
        let progress = mail("progress", /*trigger_turn*/ false);
        let steer = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "steer".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
            metadata: Default::default(),
        };
        let mut outcomes = Vec::new();
        for (reason, accepted) in [
            (
                TurnAbortReason::Interrupted,
                vec![followup.clone(), steer.clone(), progress.clone()],
            ),
            (
                TurnAbortReason::Interrupted,
                vec![progress.clone(), steer.clone()],
            ),
            (
                TurnAbortReason::Replaced,
                vec![followup.clone(), steer.clone()],
            ),
        ] {
            let input_queue = InputQueue::new();
            let active_turn = ActiveTurn::default();
            input_queue
                .extend_pending_input_for_turn_state(active_turn.turn_state.as_ref(), accepted)
                .await;

            let returned = input_queue
                .clear_pending(&active_turn, &reason, ChildReportMode::WaitAgent)
                .await;
            let paused = input_queue.wakeups_paused();
            let while_paused = input_queue.drain_mailbox_input_items().await.0;
            input_queue.resume_wakeups();
            let after_resume = input_queue.drain_mailbox_input_items().await.0;
            outcomes.push((returned, paused, while_paused, after_resume));
        }

        assert_eq!(
            outcomes,
            vec![
                (true, true, vec![progress.clone()], vec![followup.clone()]),
                (true, true, vec![progress], Vec::new()),
                (true, false, vec![followup], Vec::new()),
            ]
        );
    }

    /// Mail returned by `clear_pending` keeps the turn's start options, so an interrupted
    /// `followup_task` that starts a new turn still reports the requester's turn lineage. Mail
    /// in the mailbox that the turn never drained is returned untouched.
    #[test_case::test_case(TurnAbortReason::Interrupted; "interrupted")]
    #[test_case::test_case(TurnAbortReason::Replaced; "replaced")]
    #[tokio::test]
    async fn clear_pending_returns_mail_with_the_turns_start_options(reason: TurnAbortReason) {
        let parent = AgentPath::try_from("/root/worker").expect("agent path");
        let report = TurnInput::InterAgentCommunication(
            make_mail(
                parent,
                AgentPath::root(),
                "report",
                /*trigger_turn*/ true,
            )
            .into(),
        );
        let steer = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "steer".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
            metadata: Default::default(),
        };
        let start_options = TurnStartOptions {
            parent_turn_id: Some("requester-turn".to_string()),
            root_turn_id: Some("root-turn".to_string()),
            turn_trigger: Some("composer".to_string()),
            ..Default::default()
        };

        let input_queue = InputQueue::new();
        let active_turn = ActiveTurn::default();
        input_queue
            .extend_pending_input_for_turn_state(
                active_turn.turn_state.as_ref(),
                vec![report.clone(), steer.clone()],
            )
            .await;
        active_turn.turn_state.lock().await.mail_start_options = start_options.clone();

        assert!(
            input_queue
                .clear_pending(&active_turn, &reason, ChildReportMode::WaitAgent)
                .await
        );
        input_queue.resume_wakeups();
        let (delivered, delivered_options) = input_queue.drain_mailbox_input_items().await;
        assert_eq!(delivered, vec![report]);
        assert_eq!(
            delivered_options.parent_turn_id.as_deref(),
            Some("requester-turn")
        );
        assert_eq!(delivered_options.root_turn_id.as_deref(), Some("root-turn"));
        assert_eq!(delivered_options.turn_trigger.as_deref(), Some("composer"));
        assert!(
            !input_queue
                .clear_pending(&active_turn, &reason, ChildReportMode::WaitAgent)
                .await
        );
    }

    fn steer_input(text: &str) -> TurnInput {
        TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
            metadata: Default::default(),
        }
    }

    /// A mid-turn drain leaves unrecorded mail in pending input at the drain order, so hooks can
    /// inspect a stable snapshot for a hard abort to recover; recording consumes it item by item.
    #[tokio::test]
    async fn drain_keeping_unrecorded_mail_leaves_mail_until_consumed() {
        let parent = AgentPath::try_from("/root/worker").expect("agent path");
        let steer = steer_input("steer");
        let queued_mail = TurnInput::InterAgentCommunication(
            make_mail(
                parent.clone(),
                AgentPath::root(),
                "queued report",
                /*trigger_turn*/ false,
            )
            .into(),
        );
        let mailbox_mail = TurnInput::InterAgentCommunication(
            make_mail(
                parent.clone(),
                AgentPath::root(),
                "mailbox report",
                /*trigger_turn*/ true,
            )
            .into(),
        );
        let input_queue = InputQueue::new();
        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        input_queue
            .extend_pending_input_for_turn_state(
                active_turn
                    .lock()
                    .await
                    .as_ref()
                    .expect("active turn")
                    .turn_state
                    .as_ref(),
                vec![steer.clone(), queued_mail.clone()],
            )
            .await;
        input_queue
            .enqueue_mailbox_communication(
                match &mailbox_mail {
                    TurnInput::InterAgentCommunication(communication) => communication.clone(),
                    _ => unreachable!("mailbox mail is a communication"),
                },
                Default::default(),
            )
            .await;

        let drained = input_queue
            .drain_pending_input_keeping_unrecorded_mail(&active_turn)
            .await;
        assert_eq!(
            drained,
            vec![steer.clone(), queued_mail.clone(), mailbox_mail.clone()]
        );

        // The steering input left the queue; unrecorded mail stays pending until recording
        // consumes it, in drain order.
        input_queue.consume_unrecorded_mail(&active_turn).await;
        {
            let mut active = active_turn.lock().await;
            let turn_state = active.as_mut().expect("active turn").turn_state.as_ref();
            let left = turn_state.lock().await.pending_input.items.clone();
            assert_eq!(left, vec![mailbox_mail.clone()]);
        }

        // Consuming the recorded copy fully empties the held copies; steering drains without a
        // protection copy.
        input_queue.consume_unrecorded_mail(&active_turn).await;
        input_queue
            .extend_pending_input_for_turn_state(
                active_turn
                    .lock()
                    .await
                    .as_ref()
                    .expect("active turn")
                    .turn_state
                    .as_ref(),
                vec![steer.clone()],
            )
            .await;
        let steered_batch = input_queue
            .drain_pending_input_keeping_unrecorded_mail(&active_turn)
            .await;
        assert_eq!(steered_batch, vec![steer]);
        {
            let mut active = active_turn.lock().await;
            let turn_state = active.as_mut().expect("active turn").turn_state.as_ref();
            assert!(turn_state.lock().await.pending_input.items.is_empty());
        }
    }

    /// Mail appended into the turn state during the drain window (a same-window message-board
    /// delivery, between the pending-input take and the mailbox attach) must stay behind the
    /// drained batch's copies: recording consumes exactly the batch copies, in batch order, and
    /// the late delivery is neither consumed on the batch's behalf nor left doubled.
    #[tokio::test]
    async fn mail_delivered_mid_drain_window_waits_behind_batch_copies() {
        let parent = AgentPath::try_from("/root/worker").expect("agent path");
        let steer = steer_input("steer");
        let queued_mail = TurnInput::InterAgentCommunication(
            make_mail(
                parent.clone(),
                AgentPath::root(),
                "queued report",
                /*trigger_turn*/ false,
            )
            .into(),
        );
        let mailbox_mail = TurnInput::InterAgentCommunication(
            make_mail(
                parent.clone(),
                AgentPath::root(),
                "mailbox report",
                /*trigger_turn*/ true,
            )
            .into(),
        );
        let late_mail = TurnInput::InterAgentCommunication(
            make_mail(
                parent.clone(),
                AgentPath::root(),
                "board update",
                /*trigger_turn*/ false,
            )
            .into(),
        );
        let input_queue = InputQueue::new();
        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        // Pre-bind the turn state so the guard is not held across the extend's await.
        let turn_state_ref = active_turn
            .lock()
            .await
            .as_ref()
            .expect("active turn")
            .turn_state
            .clone();
        input_queue
            .extend_pending_input_for_turn_state(
                turn_state_ref.as_ref(),
                vec![steer.clone(), queued_mail.clone()],
            )
            .await;
        input_queue
            .enqueue_mailbox_communication(
                match &mailbox_mail {
                    TurnInput::InterAgentCommunication(communication) => communication.clone(),
                    _ => unreachable!("mailbox mail is a communication"),
                },
                Default::default(),
            )
            .await;

        // Block 1 of the drain: pending input becomes the batch; the queued mail keeps one copy.
        let Some((mut batch, turn_state, batch_mail_count)) = input_queue
            .take_pending_input_keeping_unrecorded_mail(&active_turn)
            .await
        else {
            panic!("the turn accepts mailbox delivery");
        };
        assert_eq!(batch, vec![steer.clone(), queued_mail.clone()]);
        assert_eq!(batch_mail_count, 1);

        // The same window: a producer appends mail into the turn state, like the message-board
        // delivery does before the mailbox attach runs.
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![late_mail.clone()])
            .await;
        {
            let turn_state = turn_state.lock().await;
            assert_eq!(
                turn_state.pending_input.items,
                vec![queued_mail.clone(), late_mail.clone()]
            );
        }

        // Block 2: mailbox copies attach directly behind the batch's existing copies, so the
        // late mail trails the whole batch. The window's append runs before and after both work.
        let attached = input_queue
            .attach_drained_mailbox_batch(&active_turn, &turn_state, batch_mail_count, &mut batch)
            .await;
        assert!(attached);
        assert_eq!(
            batch,
            vec![steer.clone(), queued_mail.clone(), mailbox_mail.clone()]
        );
        {
            let turn_state = turn_state.lock().await;
            assert_eq!(
                turn_state.pending_input.items,
                vec![queued_mail.clone(), mailbox_mail.clone(), late_mail.clone()]
            );
        }

        // Recording the batch consumes exactly its two copies, in batch order. The late mail
        // keeps its own entry for its own delivery instead of being consumed in their place.
        input_queue.consume_unrecorded_mail(&active_turn).await;
        input_queue.consume_unrecorded_mail(&active_turn).await;
        {
            let turn_state = turn_state.lock().await;
            assert_eq!(turn_state.pending_input.items, vec![late_mail.clone()]);
        }

        // The late mail is delivered exactly once by its own later drain.
        let late_batch = input_queue
            .drain_pending_input_keeping_unrecorded_mail(&active_turn)
            .await;
        assert_eq!(late_batch, vec![late_mail]);
        input_queue.consume_unrecorded_mail(&active_turn).await;
        {
            let turn_state = turn_state.lock().await;
            assert!(turn_state.pending_input.items.is_empty());
        }
    }

    #[test_case::test_case(true; "active_turn_lock")]
    #[test_case::test_case(false; "turn_state_lock")]
    #[tokio::test]
    async fn cancelled_mailbox_attach_keeps_mail_in_order(contend_active_turn: bool) {
        let input_queue = InputQueue::new();
        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        let turn_state = active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .turn_state
            .clone();
        let start_options = TurnStartOptions {
            parent_turn_id: Some("requester-turn".to_string()),
            root_turn_id: Some("requester-root".to_string()),
            ..Default::default()
        };
        let first = TurnInput::InterAgentCommunication(
            make_mail(AgentPath::root(), AgentPath::root(), "first", true).into(),
        );
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![first.clone()])
            .await;
        turn_state.lock().await.mail_start_options = start_options.clone();
        let second = make_mail(AgentPath::root(), AgentPath::root(), "second", true);
        input_queue
            .enqueue_mailbox_communication(second.clone().into(), start_options)
            .await;
        let (mut batch, _, mail_count) = input_queue
            .take_pending_input_keeping_unrecorded_mail(&active_turn)
            .await
            .unwrap();

        // Poll once while a lock is held, then cancel the future at that precise await.
        // The former drain-before-lock implementation loses the second message here.
        let active_guard = if contend_active_turn {
            Some(active_turn.lock().await)
        } else {
            None
        };
        let state_guard = if contend_active_turn {
            None
        } else {
            Some(turn_state.lock().await)
        };
        {
            let attach = input_queue.attach_drained_mailbox_batch(
                &active_turn,
                &turn_state,
                mail_count,
                &mut batch,
            );
            tokio::pin!(attach);
            assert!(futures::poll!(attach.as_mut()).is_pending());
        }
        drop(state_guard);
        drop(active_guard);
        assert_eq!(input_queue.trigger_turn_mailbox_count().await, 1);
        assert!(
            input_queue
                .clear_pending(
                    active_turn.lock().await.as_ref().unwrap(),
                    &TurnAbortReason::Replaced,
                    ChildReportMode::WaitAgent,
                )
                .await
        );
        let (recovered, options) = input_queue.drain_mailbox_input_items().await;
        assert_eq!(
            recovered,
            vec![first, TurnInput::InterAgentCommunication(second.into())]
        );
        assert_eq!(options.parent_turn_id.as_deref(), Some("requester-turn"));
        assert_eq!(options.root_turn_id.as_deref(), Some("requester-root"));
        assert!(input_queue.drain_mailbox_input_items().await.0.is_empty());
    }

    /// Recording consumes the turn's held mail copies in drain order; consuming past the batch is
    /// a no-op instead of losing later mail.
    #[tokio::test]
    async fn consume_unrecorded_mail_consumes_in_drain_order() {
        let parent = AgentPath::try_from("/root/worker").expect("agent path");
        let first = TurnInput::InterAgentCommunication(
            make_mail(
                parent.clone(),
                AgentPath::root(),
                "first",
                /*trigger_turn*/ true,
            )
            .into(),
        );
        let second = TurnInput::InterAgentCommunication(
            make_mail(
                parent,
                AgentPath::root(),
                "second",
                /*trigger_turn*/ false,
            )
            .into(),
        );
        let input_queue = InputQueue::new();
        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        {
            let mut active = active_turn.lock().await;
            let turn_state = active.as_mut().expect("active turn").turn_state.as_ref();
            turn_state
                .lock()
                .await
                .pending_input
                .items
                .extend([first.clone(), second.clone()]);
        }

        input_queue.consume_unrecorded_mail(&active_turn).await;
        {
            let mut active = active_turn.lock().await;
            let turn_state = active.as_mut().expect("active turn").turn_state.as_ref();
            let left = turn_state.lock().await.pending_input.items.clone();
            assert_eq!(left, vec![second.clone()]);
        }
        input_queue.consume_unrecorded_mail(&active_turn).await;
        input_queue.consume_unrecorded_mail(&active_turn).await;
        {
            let mut active = active_turn.lock().await;
            let turn_state = active.as_mut().expect("active turn").turn_state.as_ref();
            assert!(turn_state.lock().await.pending_input.items.is_empty());
        }
    }

    /// A hard abort while pre-sampling hooks still hold a drained batch returns every
    /// not-yet-recorded mail item to the mailbox (in order, with the mail's own lineage) and
    /// drops the steered input.
    #[test_case::test_case(true; "trigger_mail")]
    #[test_case::test_case(false; "queue_only_update")]
    #[tokio::test]
    async fn clear_pending_recovers_unrecorded_drained_mail_after_hard_abort(
        mailbox_trigger_turn: bool,
    ) {
        let parent = AgentPath::try_from("/root/worker").expect("agent path");
        let steer = steer_input("steer");
        let queued_mail = TurnInput::InterAgentCommunication(
            make_mail(
                parent.clone(),
                AgentPath::root(),
                "queued report",
                /*trigger_turn*/ true,
            )
            .into(),
        );
        let mailbox_mail = TurnInput::InterAgentCommunication(
            make_mail(
                parent,
                AgentPath::root(),
                "mailbox report",
                mailbox_trigger_turn,
            )
            .into(),
        );
        let input_queue = InputQueue::new();
        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        input_queue
            .extend_pending_input_for_turn_state(
                active_turn
                    .lock()
                    .await
                    .as_ref()
                    .expect("active turn")
                    .turn_state
                    .as_ref(),
                vec![steer.clone(), queued_mail.clone()],
            )
            .await;
        active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .turn_state
            .lock()
            .await
            .mail_start_options = TurnStartOptions {
            parent_turn_id: Some("requester-turn".to_string()),
            root_turn_id: Some("requester-root".to_string()),
            ..Default::default()
        };
        input_queue
            .enqueue_mailbox_communication(
                match &mailbox_mail {
                    TurnInput::InterAgentCommunication(communication) => communication.clone(),
                    _ => unreachable!("mailbox mail is a communication"),
                },
                TurnStartOptions {
                    parent_turn_id: Some("requester-turn".to_string()),
                    root_turn_id: Some("requester-root".to_string()),
                    ..Default::default()
                },
            )
            .await;

        let drained = input_queue
            .drain_pending_input_keeping_unrecorded_mail(&active_turn)
            .await;
        assert_eq!(
            drained,
            vec![steer.clone(), queued_mail.clone(), mailbox_mail.clone()]
        );

        assert!(
            input_queue
                .clear_pending(
                    active_turn.lock().await.as_ref().expect("active turn"),
                    &TurnAbortReason::Interrupted,
                    ChildReportMode::WaitAgent,
                )
                .await
        );
        input_queue.resume_wakeups();
        let (delivered, delivered_options) = input_queue.drain_mailbox_input_items().await;
        assert_eq!(delivered, vec![queued_mail, mailbox_mail]);
        assert_eq!(
            delivered_options.parent_turn_id.as_deref(),
            Some("requester-turn")
        );
        assert_eq!(
            delivered_options.root_turn_id.as_deref(),
            Some("requester-root")
        );
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
                )
                .into(),
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
                )
                .into(),
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
            TurnInput::InterAgentCommunication(
                make_mail(worker.clone(), AgentPath::root(), content, trigger_turn).into(),
            )
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
