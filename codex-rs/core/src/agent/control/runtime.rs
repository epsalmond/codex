//! Shared state and startup bindings for one local agent tree.
//! Registry identity is allocation identity; cloning this handle preserves ownership checks.

use super::LocalAgentControl;
use super::coordinator::AgentAssignmentId;
use super::coordinator::AgentWakeCoordinator;
use super::coordinator::AssignmentPhase;
use super::coordinator::ClaimedTerminalReport;
use super::coordinator::TerminalReportDeliveryGuard;
use super::coordinator::TurnEndDisposition;
use super::dispatcher::WakeDispatcherLifetime;
use super::execution::AgentExecutionLimiter;
use super::residency::V2Residency;
use crate::agent::api::AgentControl;
use crate::agent::registry::AgentRegistry;
use crate::config::RolloutBudgetConfig;
use crate::rollout_budget::RolloutBudget;
use crate::thread_manager::ThreadIdGenerator;
use crate::thread_manager::ThreadManagerState;
use arc_swap::ArcSwapOption;
use codex_extension_api::ThreadInstructionsProvider;
use codex_protocol::ResponseItemId;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Local tree state, kept separate from the shared agent operation interface.
#[derive(Clone)]
pub(crate) struct LocalAgentRuntime {
    /// Weak handle back to the global thread registry/state.
    /// This is `Weak` to avoid reference cycles and shadow persistence of the form
    /// `ThreadManagerState -> CodexThread -> Session -> SessionServices -> ThreadManagerState`.
    pub(super) manager: Weak<ThreadManagerState>,
    /// Captured at construction so delegates retain their manager's allocation policy.
    pub(super) thread_id_generator: ThreadIdGenerator,
    pub(super) agent_execution_limiter: Arc<AgentExecutionLimiter>,
    /// Session-scoped state shared by the root thread and every cloned sub-agent control handle.
    pub(super) rollout_budget: Arc<RolloutBudget>,
    /// The user-selected root routing tier, shared by the entire agent tree.
    pub(super) root_service_tier: Arc<ArcSwapOption<String>>,
    /// Retains the root's opt-in instruction provider even when the root is unloaded.
    pub(super) shared_thread_instructions_provider:
        Arc<OnceLock<Arc<dyn ThreadInstructionsProvider>>>,
    pub(super) registry: Arc<AgentRegistry>,
    pub(super) residency: Arc<V2Residency>,
    /// In-memory assignment obligations and terminal reports for the agent tree.
    pub(super) wake_coordinator: Arc<AgentWakeCoordinator>,
    pub(super) wake_dispatcher: Option<Arc<WakeDispatcherLifetime>>,
    wake_mode_enabled: Arc<AtomicBool>,
    root_waits_for_children: Arc<AtomicBool>,
}

impl LocalAgentRuntime {
    pub(crate) fn claim_next_report_for_delivery(
        &self,
        parent: &AgentAssignmentId,
    ) -> Option<ClaimedTerminalReport> {
        self.wake_coordinator.claim_next_mailbox_report(parent)
    }

    pub(super) fn new(
        manager: Weak<ThreadManagerState>,
        thread_id_generator: ThreadIdGenerator,
        rollout_budget: Option<RolloutBudgetConfig>,
    ) -> Self {
        let wake_coordinator = Arc::new(AgentWakeCoordinator::default());
        let runtime = Self {
            manager,
            thread_id_generator,
            registry: Arc::default(),
            residency: Arc::new(V2Residency::new(Arc::clone(&wake_coordinator))),
            wake_coordinator,
            wake_dispatcher: Some(Arc::new(WakeDispatcherLifetime::default())),
            wake_mode_enabled: Arc::new(AtomicBool::new(false)),
            root_waits_for_children: Arc::new(AtomicBool::new(false)),
            agent_execution_limiter: Arc::default(),
            rollout_budget: Arc::default(),
            root_service_tier: Arc::new(ArcSwapOption::from(None)),
            shared_thread_instructions_provider: Arc::default(),
        };
        if let Some(rollout_budget) = rollout_budget {
            runtime.rollout_budget.configure(rollout_budget);
        }
        runtime
    }

    /// Bind local startup to the same tree state with this session's identity.
    pub(crate) fn control(&self, session_id: SessionId) -> LocalAgentControl {
        LocalAgentControl {
            session_id,
            runtime: self.clone(),
        }
    }
}

/// Local construction binds identity after reading history. Hosts and internal children
/// provide an already-bound controller without selecting a backend again.
#[derive(Clone)]
pub(crate) enum AgentControlInit {
    Local(LocalAgentControl),
    Provided {
        control: Arc<dyn AgentControl>,
        runtime: LocalAgentRuntime,
    },
}

impl From<LocalAgentControl> for AgentControlInit {
    fn from(control: LocalAgentControl) -> Self {
        Self::Local(control)
    }
}

impl AgentControlInit {
    pub(crate) fn runtime(&self) -> &LocalAgentRuntime {
        match self {
            Self::Local(control) => &control.runtime,
            Self::Provided { runtime, .. } => runtime,
        }
    }

    pub(crate) fn control(&self) -> &dyn AgentControl {
        match self {
            Self::Local(control) => control,
            Self::Provided { control, .. } => control.as_ref(),
        }
    }
}

impl LocalAgentRuntime {
    pub(crate) fn generate_thread_id(&self) -> ThreadId {
        (self.thread_id_generator)()
    }

    pub(crate) fn notify_active_turn_cleared(&self) {
        self.wake_coordinator.notify_active_turn_cleared();
    }

    pub(crate) fn wake_mode_enabled(&self) -> bool {
        self.wake_mode_enabled.load(Ordering::Acquire)
    }

    pub(crate) fn enable_wake_mode(&self) {
        self.wake_mode_enabled.store(true, Ordering::Release);
    }

    pub(crate) fn set_root_waits_for_children(&self, wait: bool) {
        self.root_waits_for_children.store(wait, Ordering::Release);
    }

    pub(crate) fn root_waits_for_children(&self) -> bool {
        self.root_waits_for_children.load(Ordering::Acquire)
    }

    pub(crate) fn terminal_report_is_current(
        &self,
        report_id: &ResponseItemId,
        thread_id: ThreadId,
    ) -> bool {
        self.wake_coordinator
            .report_is_current_for_thread(report_id, thread_id)
    }

    #[cfg(test)]
    pub(crate) fn acknowledge_terminal_report_mailbox_delivery(
        &self,
        report_id: &ResponseItemId,
        thread_id: ThreadId,
    ) -> bool {
        self.wake_coordinator
            .acknowledge_report_mailbox_delivery(report_id, thread_id)
    }

    pub(crate) fn release_terminal_report_claim(&self, report_id: &ResponseItemId) -> bool {
        self.wake_coordinator.release_mailbox_claim(report_id)
    }

    pub(crate) fn terminal_report_delivery_guard(
        &self,
        report_id: ResponseItemId,
    ) -> TerminalReportDeliveryGuard {
        self.wake_coordinator.delivery_guard(report_id)
    }

    pub(crate) fn discard_stale_terminal_report(&self, report_id: &ResponseItemId) -> bool {
        self.wake_coordinator.discard_report_if_stale(report_id)
    }

    pub(crate) fn classify_wake_turn_end(
        &self,
        assignment: &AgentAssignmentId,
        turn_id: &str,
        disposition: TurnEndDisposition,
    ) -> Result<AssignmentPhase, &'static str> {
        self.wake_coordinator
            .classify_turn_end(assignment, turn_id, disposition)
    }

    pub(crate) fn wake_assignment_status(&self, thread_id: ThreadId) -> Option<AgentStatus> {
        self.wake_coordinator.assignment_status(thread_id)
    }

    pub(crate) fn wake_report_ids_in_prompt(
        &self,
        assignment: &AgentAssignmentId,
        prompt_input: &[ResponseItem],
    ) -> Vec<ResponseItemId> {
        self.wake_coordinator
            .report_ids_in_prompt(assignment, prompt_input)
    }

    pub(crate) fn has_recorded_wake_reports(&self, assignment: &AgentAssignmentId) -> bool {
        self.wake_coordinator.has_recorded_reports(assignment)
    }

    pub(crate) async fn wait_for_queue_only_wake_reports_enqueued_for_thread(
        &self,
        thread_id: ThreadId,
    ) {
        if !self.wake_mode_enabled() {
            return;
        }
        if let Some(assignment) = self.wake_coordinator.current_assignment(thread_id) {
            self.wake_coordinator
                .wait_for_queue_only_reports_enqueued(&assignment)
                .await;
        }
    }

    pub(crate) fn is_current_waiting_assignment(&self, assignment: &AgentAssignmentId) -> bool {
        self.wake_coordinator
            .is_current_waiting_assignment(assignment)
    }

    pub(crate) fn accept_wake_reports(
        &self,
        assignment: &AgentAssignmentId,
        report_ids: &[ResponseItemId],
    ) -> usize {
        self.wake_coordinator.accept_reports(assignment, report_ids)
    }

    pub(crate) fn mark_wake_report_recorded(&self, report_id: &ResponseItemId) -> bool {
        self.wake_coordinator.mark_report_recorded(report_id)
    }

    pub(crate) fn terminal_wake_assignment_for_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Option<AgentAssignmentId> {
        self.wake_coordinator
            .terminal_assignment_for_turn(thread_id, turn_id)
    }

    pub(crate) fn interrupt_idle_wake_assignment(
        &self,
        thread_id: ThreadId,
        terminal_turn_id: &str,
    ) -> Option<AgentAssignmentId> {
        let assignment = self.wake_coordinator.current_assignment(thread_id)?;
        self.wake_coordinator
            .interrupt_idle_assignment(&assignment, terminal_turn_id)
            .ok()
            .filter(|interrupted| *interrupted)
            .map(|_| assignment)
    }

    pub(crate) fn begin_wake_assignment_for_turn(
        &self,
        thread_id: ThreadId,
        source: &SessionSource,
        turn_id: &str,
        allow_new_generation: bool,
    ) -> Result<Option<AgentAssignmentId>, &'static str> {
        if !self.wake_mode_enabled() {
            return Ok(None);
        }

        let current = self.wake_coordinator.current_assignment(thread_id);
        let existing_parent = current
            .as_ref()
            .and_then(|id| self.wake_coordinator.parent_assignment(id));
        let parent_thread_id = source.parent_thread_id();
        let current_is_open = current
            .as_ref()
            .is_some_and(|id| self.wake_coordinator.is_current_open_assignment(id));
        let parent = if current_is_open {
            existing_parent
        } else if let Some(parent_thread_id) = parent_thread_id {
            // Follow-up lineage may name a sibling caller's turn. Assignment ownership always
            // follows the spawn parent, including when the previous child report was consumed.
            let parent = if allow_new_generation {
                self.wake_coordinator
                    .current_assignment(parent_thread_id)
                    .filter(|id| self.wake_coordinator.is_current_open_assignment(id))
            } else {
                existing_parent.filter(|id| self.wake_coordinator.is_current_open_assignment(id))
            };
            if parent.is_none() {
                return Err("parent assignment is not active for this turn");
            }
            parent
        } else {
            None
        };

        let assignment = self
            .wake_coordinator
            .begin_or_continue_assignment(thread_id, parent, turn_id, allow_new_generation)
            .map(Some)?;
        if let Some(assignment) = assignment.as_ref() {
            self.wake_coordinator
                .request_wake_for_pending_mailbox_reports(assignment);
        }
        Ok(assignment)
    }

    pub(super) fn without_wake_dispatcher(&self) -> Self {
        let mut runtime = self.clone();
        runtime.wake_dispatcher = None;
        runtime
    }

    pub(crate) fn root_thread_instructions_provider(
        &self,
        root_thread_id: ThreadId,
        provider: Option<Arc<dyn ThreadInstructionsProvider>>,
    ) -> Option<Arc<dyn ThreadInstructionsProvider>> {
        let provider = match self.manager.upgrade() {
            Some(manager) => manager.shared_thread_instructions_provider(root_thread_id, provider),
            None => provider,
        };
        if let Some(provider) = provider
            .as_ref()
            .filter(|provider| provider.share_with_subagents())
        {
            let _ = self
                .shared_thread_instructions_provider
                .set(Arc::clone(provider));
        }
        provider
    }
}
