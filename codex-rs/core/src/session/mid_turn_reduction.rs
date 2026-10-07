//! Context reduction at the mid-turn roll-over point, after a sampling request whose
//! follow-up would exceed the context limit.
//!
//! Ordinary child limits are enforced by request_admission after prompt preparation.
//! Explicit new-context-window requests compact directly here and defer child outcomes
//! to final admission. Root sessions preserve their scoped post-sampling roll-over.

use std::sync::Arc;

use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;

use super::context_window::context_window_token_status;
use super::session::Session;
use super::step_context::StepContext;
use super::turn::maybe_run_auto_shake;
use super::turn::run_inline_compact;
use super::turn_context::TurnContext;
use crate::agent::types::ContextReductionOutcome;
use crate::agent::types::ContextReductionRecord;
use crate::client::ModelClientSession;
use crate::compact::InitialContextInjection;
use crate::compact_invocation::CompactionInvocation;
use crate::context::world_state::WorldState;

/// Why the sampling loop is rolling over to a reduced context.
pub(super) enum RollOverTrigger {
    /// The model explicitly asked for a new context window; always compact.
    NewContextWindowRequested,
    /// Post-sampling usage reached the auto-compact or full-window limit.
    TokenLimitReached,
}

/// How the mid-turn roll-over reduced the context.
pub(super) enum MidTurnReduction {
    /// Auto-shake brought usage under the limit, so compaction was skipped.
    Shaken,
    /// Compaction ran and sampling continues (a root session may still be over the limit).
    Compacted,
}

/// Error text for a subagent turn that ended over its context limit. The parent reads it
/// as the child's status, so it states what happened and the parent's next step.
/// A subagent inherits its parent's `multi_agent_version`, which names the parent's messaging tool.
pub(super) fn subagent_context_limit_message(
    limit_tokens: Option<i64>,
    multi_agent_version: MultiAgentVersion,
) -> String {
    let limit = limit_tokens.map_or_else(
        || "context limit".to_string(),
        |limit| format!("{limit}-token context limit"),
    );
    let ask = match multi_agent_version {
        MultiAgentVersion::V2 => "Use `followup_task` to ask it",
        MultiAgentVersion::V1 => "Use `send_input` to ask it",
        MultiAgentVersion::Disabled => "Ask it",
    };
    format!(
        "This subagent's turn ended because its context was still over its {limit} after compaction. {ask} for a brief report of its partial results, then delegate the remaining work as smaller tasks."
    )
}

pub(super) async fn reduce_mid_turn(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    step_context: &Arc<StepContext>,
    world_state: &Arc<WorldState>,
    client_session: &mut ModelClientSession,
    trigger: RollOverTrigger,
    tokens_before: i64,
) -> CodexResult<MidTurnReduction> {
    let subagent = matches!(turn_context.session_source, SessionSource::SubAgent(_));
    let shake_first = match trigger {
        RollOverTrigger::NewContextWindowRequested => false,
        RollOverTrigger::TokenLimitReached => subagent,
    };
    if shake_first {
        let shook = maybe_run_auto_shake(sess, turn_context).await;
        let after_shake = context_window_token_status(sess.as_ref(), turn_context.as_ref()).await;
        if shook && !after_shake.token_limit_reached {
            record_reduction(
                sess,
                ContextReductionOutcome::Shaken,
                tokens_before,
                after_shake.active_context_tokens,
            )
            .await;
            return Ok(MidTurnReduction::Shaken);
        }
    }

    // The request's step context holds settings, tools, and MCP bindings but no history
    // snapshot, so it remains valid for compaction after a shake rewrote history.
    let compact = run_inline_compact(
        sess,
        Arc::clone(step_context),
        /*fallback_step_context*/ None,
        client_session,
        InitialContextInjection::BeforeLastUserMessage {
            world_state: Arc::clone(world_state),
            step_context: Arc::clone(step_context),
        },
        CompactionInvocation::automatic(CompactionReason::ContextLimit, CompactionPhase::MidTurn),
    )
    .await;
    if let Err(err) = compact {
        record_reduction_error(sess, tokens_before, &err).await;
        return Err(err);
    }
    if subagent {
        sess.state.lock().await.pending_child_reduction =
            Some(super::request_admission::ReductionAttempt {
                before_tokens: tokens_before,
                outcome: ContextReductionOutcome::Compacted,
            });
        return Ok(MidTurnReduction::Compacted);
    }

    let after_compact = context_window_token_status(sess.as_ref(), turn_context.as_ref()).await;
    let tokens_after = after_compact.active_context_tokens;
    let outcome = if after_compact.token_limit_reached {
        ContextReductionOutcome::Insufficient
    } else {
        ContextReductionOutcome::Compacted
    };
    record_reduction(sess, outcome, tokens_before, tokens_after).await;
    Ok(MidTurnReduction::Compacted)
}

/// Records the outcome reported as `last_reduction` by `list_agents`.
pub(super) async fn record_reduction(
    sess: &Session,
    outcome: ContextReductionOutcome,
    before_tokens: i64,
    after_tokens: i64,
) {
    sess.record_context_reduction(ContextReductionRecord {
        at: chrono::Utc::now().timestamp(),
        before_tokens,
        after_tokens: Some(after_tokens),
        outcome,
    })
    .await;
}

/// Failed maintenance has no trustworthy post-reduction measurement.
pub(super) async fn record_reduction_error(
    sess: &Session,
    before_tokens: i64,
    err: &codex_protocol::error::CodexErr,
) {
    let outcome = match err.details() {
        codex_protocol::error::CodexErrorDetails::TurnAborted
        | codex_protocol::error::CodexErrorDetails::Interrupted => {
            ContextReductionOutcome::Cancelled
        }
        _ => ContextReductionOutcome::Failed,
    };
    sess.record_context_reduction(ContextReductionRecord {
        at: chrono::Utc::now().timestamp(),
        before_tokens,
        after_tokens: None,
        outcome,
    })
    .await;
}

#[cfg(test)]
#[path = "mid_turn_reduction_tests.rs"]
mod tests;
