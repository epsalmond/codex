//! Agent data and execution reservations shared by controllers and their callers.
//! Backend operations and local admission policy live in the implementations.

use crate::context::MultiAgentRoleInstructions;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
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
}

/// Identity and status observed from a loaded agent, without a handle to its runtime.
#[derive(Clone, Debug)]
pub struct LiveAgent {
    pub thread_id: ThreadId,
    pub metadata: AgentMetadata,
    pub status: AgentStatus,
}

/// How an automatic context reduction ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextReductionOutcome {
    /// Shaking alone brought active context below the threshold.
    Shaken,
    /// Compaction ran after shaking was skipped or insufficient, or directly for an explicit
    /// new-context-window request.
    Compacted,
    /// Reduction ran or was attempted but active context stayed above the threshold.
    Insufficient,
}

/// The most recent automatic context reduction of an agent. Runtime-only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextReductionRecord {
    /// Unix seconds.
    pub at: i64,
    pub before_tokens: i64,
    /// `None` when the post-reduction size is unknown.
    pub after_tokens: Option<i64>,
    pub outcome: ContextReductionOutcome,
}

/// Where an agent's active context token count comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextTokenBasis {
    /// Last model-reported usage plus a local estimate of items recorded since.
    Usage,
    /// No model usage reported since the thread started or history was last rewritten;
    /// a local estimate of recorded history.
    Estimate,
}

/// Active-context snapshot reported to a parent by `list_agents`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentContextUsage {
    pub active_tokens: i64,
    pub basis: ContextTokenBasis,
    pub last_reduction: Option<ContextReductionRecord>,
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
