//! Runtime-only context usage and reduction telemetry that parents read through `list_agents`.

use super::session::Session;
use crate::agent::types::AgentContextUsage;
use crate::agent::types::ContextReductionRecord;
use crate::agent::types::ContextTokenBasis;

impl Session {
    /// Replaces the last reduction reported by `list_agents`.
    pub(crate) async fn record_context_reduction(&self, record: ContextReductionRecord) {
        self.state.lock().await.last_context_reduction = Some(record);
    }

    /// Reads active context with the same accounting used for auto-compaction.
    pub(crate) async fn context_usage(&self) -> AgentContextUsage {
        let state = self.state.lock().await;
        let basis = if state.token_info().is_some() && !state.token_usage_estimated {
            ContextTokenBasis::Usage
        } else {
            ContextTokenBasis::Estimate
        };
        AgentContextUsage {
            active_tokens: state.get_total_token_usage(state.server_reasoning_included()),
            basis,
            last_reduction: state.last_context_reduction.clone(),
        }
    }
}
