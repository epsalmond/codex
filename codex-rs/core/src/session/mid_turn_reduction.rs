//! Context reduction at the mid-turn roll-over point, after a sampling request whose
//! follow-up would exceed the context limit.
//!
//! Subagents (`SessionSource::SubAgent`) try auto-shake before compaction, and a
//! compaction that cannot bring them back under the limit ends the turn instead of
//! sampling and compacting in a loop. Root sessions keep the prior behaviour: they
//! compact directly and keep sampling whatever the post-compaction size. An explicit
//! new-context-window request always compacts directly, for every session.

use std::sync::Arc;

use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use tracing::warn;

use super::context_window::context_window_token_status;
use super::session::Session;
use super::step_context::StepContext;
use super::turn::maybe_run_auto_shake;
use super::turn::run_auto_compact;
use super::turn_context::TurnContext;
use crate::agent::types::ContextReductionOutcome;
use crate::agent::types::ContextReductionRecord;
use crate::client::ModelClientSession;
use crate::compact::InitialContextInjection;
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
    /// Compaction ran but a subagent's usage is still at or over the limit.
    Insufficient,
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
        maybe_run_auto_shake(sess, turn_context).await;
        let after_shake = context_window_token_status(sess.as_ref(), turn_context.as_ref()).await;
        if !after_shake.token_limit_reached {
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
    run_auto_compact(
        sess,
        Arc::clone(step_context),
        /*fallback_step_context*/ None,
        client_session,
        InitialContextInjection::BeforeLastUserMessage {
            world_state: Arc::clone(world_state),
            step_context: Arc::clone(step_context),
        },
        CompactionReason::ContextLimit,
        CompactionPhase::MidTurn,
    )
    .await?;

    let after_compact = context_window_token_status(sess.as_ref(), turn_context.as_ref()).await;
    let tokens_after = after_compact.active_context_tokens;
    let outcome = if after_compact.token_limit_reached {
        ContextReductionOutcome::Insufficient
    } else {
        ContextReductionOutcome::Compacted
    };
    record_reduction(sess, outcome, tokens_before, tokens_after).await;
    if after_compact.token_limit_reached && subagent {
        warn!(
            turn_id = %turn_context.sub_id,
            tokens_before,
            tokens_after,
            auto_compact_scope_tokens = after_compact.auto_compact_scope_tokens,
            auto_compact_scope_limit = ?after_compact.auto_compact_scope_limit,
            full_context_window_limit = ?after_compact.full_context_window_limit,
            "mid-turn compaction left the subagent context over its limit; failing the turn"
        );
        return Ok(MidTurnReduction::Insufficient);
    }
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
