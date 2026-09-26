//! Agent data and execution reservations shared by controllers and their callers.
//! Backend operations and local admission policy live in the implementations.

use crate::context::MultiAgentRoleInstructions;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SubagentContextReductionOverrides;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::turn_input::CyberAccessProgram;
use serde::Serialize;

/// Registry identity shared by loaded and unloaded agents.
/// Registered agents have an `agent_id`; a reserved spawn can still be awaiting its ID.
#[derive(Clone, Debug, Default)]
pub struct AgentMetadata {
    pub agent_id: Option<ThreadId>,
    pub agent_path: Option<AgentPath>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnAgentForkMode {
    FullHistory,
    LastNTurns(usize),
}

#[derive(Clone, Debug, Default)]
pub struct SpawnAgentOptions {
    pub fork_parent_spawn_call_id: Option<String>,
    pub fork_mode: Option<SpawnAgentForkMode>,
    pub parent_thread_id: Option<ThreadId>,
    pub parent_turn_id: Option<String>,
    /// Attribute delegated usage to the turn that initiated it.
    pub turn_trigger: Option<String>,
    pub root_turn_id: Option<String>,
    pub environments: Option<Vec<TurnEnvironmentSelection>>,
    pub multi_agent_v2_usage_hints: Option<ResolvedMultiAgentV2UsageHints>,
    pub cyber_access_program: Option<CyberAccessProgram>,
    /// Explicit context-reduction values for this child.
    pub context_reduction_policy: Option<SubagentContextReductionOverrides>,
    /// Whether explicit context-reduction values should pass to new descendants.
    pub context_policy_inherit_to_children: Option<bool>,
}

/// Identity and status observed from a loaded agent, without a handle to its runtime.
#[derive(Clone, Debug)]
pub struct LiveAgent {
    pub thread_id: ThreadId,
    pub metadata: AgentMetadata,
    pub status: AgentStatus,
    pub context_reduction: Option<Box<AgentContextReductionTelemetry>>,
}

/// Parent-visible context policy and runtime observations for one child agent.
#[derive(Clone, Debug, Serialize)]
pub struct AgentContextReductionTelemetry {
    pub active_context_tokens: Option<i64>,
    pub observed_at: Option<DateTime<Utc>>,
    pub active_context_token_basis: Option<String>,
    pub desired_policy: AgentContextReductionPolicy,
    pub desired_revision: u64,
    pub applied_revision: Option<u64>,
    pub last_reduction: Option<AgentContextReductionAttempt>,
}

#[derive(Clone, Debug)]
pub struct AgentContextReductionUsageSnapshot {
    pub active_context_tokens: Option<i64>,
    pub observed_at: Option<DateTime<Utc>>,
    pub active_context_token_basis: Option<String>,
    pub effective_threshold_tokens: Option<i64>,
    pub model_context_window_tokens: Option<i64>,
    pub auto_compact_scope: Option<String>,
    pub auto_compact_scope_limit_tokens: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentContextReductionPolicy {
    pub enabled: bool,
    pub threshold_tokens: i64,
    pub effective_threshold_tokens: Option<i64>,
    pub model_context_window_tokens: Option<i64>,
    pub auto_compact_scope: Option<String>,
    pub auto_compact_scope_limit_tokens: Option<i64>,
    pub check_after_tools: bool,
    pub shake: String,
    pub on_failure: String,
    pub provenance: AgentContextReductionPolicyProvenance,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentContextReductionPolicyProvenance {
    pub enabled: String,
    pub threshold_tokens: String,
    pub check_after_tools: String,
    pub shake: String,
    pub on_failure: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentContextReductionAttempt {
    pub at: DateTime<Utc>,
    pub reason: String,
    pub before_tokens: i64,
    pub after_tokens: i64,
    pub shake: Option<AgentContextReductionStage>,
    pub compact: Option<AgentContextReductionStage>,
    pub outcome: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentContextReductionStage {
    pub outcome: String,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ResolvedMultiAgentV2UsageHints {
    pub root: Option<MultiAgentRoleInstructions>,
    pub subagent: Option<MultiAgentRoleInstructions>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MessageDeliveryMode {
    /// Deliver to the mailbox without starting an idle agent.
    QueueOnly,
    /// Deliver to the active turn or start work if the agent is idle.
    TriggerTurn,
}

/// Keeps model-provided encrypted content distinct from text that needs a context wrapper.
pub enum AgentMessage {
    Plaintext(String),
    Encrypted(String),
}

/// Holds a backend-owned reservation until the turn ends or is cancelled.
///
/// The permit's destructor releases capacity or arranges backend cleanup. Remote backends
/// must also recover reservations after worker loss, when no Rust destructor can run.
#[must_use = "hold the execution guard for the lifetime of the admitted turn"]
pub struct AgentExecutionGuard {
    _permit: Box<dyn Send + Sync>,
}

impl AgentExecutionGuard {
    pub fn new(permit: impl Send + Sync + 'static) -> Self {
        Self {
            _permit: Box::new(permit),
        }
    }
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
