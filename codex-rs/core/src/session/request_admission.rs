//! Child admission after final prompt preparation. Rebuilds retain the sampling worker.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;

use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_async_utils::OrCancelExt;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use tokio_util::sync::CancellationToken;

use super::context_reduction_telemetry::PreparedContext;
use super::mid_turn_reduction::record_reduction;
use super::session::Session;
use super::step_context::StepContext;
use super::turn::maybe_run_auto_shake;
use super::turn::run_auto_compact;
use crate::agent::types::ContextReductionOutcome;
use crate::client::ModelClientSession;

/// A bounded marker based on post-reduction content, rather than turn IDs or generations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InsufficientContext {
    history: u64,
    limits: (Option<i64>, Option<i64>, Option<i64>),
    scope: codex_protocol::config_types::AutoCompactTokenLimitScope,
}

pub(super) enum Admission {
    Proceed,
    Rebuild,
    Insufficient { limit_tokens: Option<i64> },
}

#[derive(Default)]
pub(super) struct ChildRequestAdmission {
    attempt: Option<ReductionAttempt>,
    default_shake_checked: bool,
}

pub(crate) struct ReductionAttempt {
    pub(crate) before_tokens: i64,
    pub(crate) outcome: ContextReductionOutcome,
}

impl ChildRequestAdmission {
    // Returning to run_turn would drop the Code Mode dispatcher and attachment retention map.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn check(
        &mut self,
        sess: &Arc<Session>,
        step: &Arc<StepContext>,
        client: &mut ModelClientSession,
        context: &PreparedContext,
        cancellation: &CancellationToken,
    ) -> CodexResult<Admission> {
        let turn = &step.turn;
        if !matches!(turn.session_source, SessionSource::SubAgent(_)) {
            return Ok(Admission::Proceed);
        }
        if self.attempt.is_none() {
            self.attempt = sess.state.lock().await.pending_child_reduction.take();
        }
        if cancellation.is_cancelled() {
            let err = codex_protocol::error::CodexErr::TurnAborted;
            if let Some(attempt) = self.attempt.take() {
                super::mid_turn_reduction::record_reduction_error(
                    sess,
                    attempt.before_tokens,
                    &err,
                )
                .await;
            }
            return Err(err);
        }
        // Consumed by the first admission after the compaction, whether or not it applies.
        let budget_compacted = crate::guardian::is_basic_session_source(&turn.session_source)
            && sess
                .services
                .thread_extension_data
                .remove::<crate::guardian::ReviewBudgetCompacted>()
                .is_some();
        let history = sess.clone_history().await;
        let status = &context.status;
        let additions = context.additions;
        let active = context.active;
        let child_limit = turn.config.subagent_context_reduction.enabled.then(|| {
            i64::try_from(turn.config.subagent_context_reduction.threshold_tokens)
                .unwrap_or(i64::MAX)
        });
        let limit_tokens = [child_limit, status.full_context_window_limit]
            .into_iter()
            .flatten()
            .min();
        let token_budget = super::token_budget::resolve_token_budget(
            turn.configured_token_budget.as_ref(),
            turn.use_model_token_budget_defaults,
            &step.settings.model_info,
        );
        let scope_limit = status.auto_compact_scope_limit.map(|limit| {
            limit.saturating_add(
                token_budget
                    .as_ref()
                    .map_or(0, crate::config::TokenBudgetConfig::fallback_buffer_tokens),
            )
        });
        let scope_tokens = match turn.config.model_auto_compact_token_limit_scope {
            codex_protocol::config_types::AutoCompactTokenLimitScope::Total => active,
            codex_protocol::config_types::AutoCompactTokenLimitScope::BodyAfterPrefix => {
                status.auto_compact_scope_tokens.saturating_add(additions)
            }
        };
        let scope_exceeded = scope_limit.is_some_and(|limit| scope_tokens >= limit);
        let exceeds = limit_tokens.is_some_and(|limit| active >= limit) || scope_exceeded;
        let limit_tokens = if scope_exceeded {
            [limit_tokens, scope_limit].into_iter().flatten().min()
        } else {
            limit_tokens
        };
        if !exceeds {
            if self.attempt.is_none() && !self.default_shake_checked {
                self.default_shake_checked = true;
                if maybe_run_auto_shake(sess, turn).await {
                    self.attempt = Some(ReductionAttempt {
                        before_tokens: active,
                        outcome: ContextReductionOutcome::Shaken,
                    });
                    return Ok(Admission::Rebuild);
                }
                // A no-op can still seal a prefix. Rebuild its request guard without
                // claiming a reduction or repeating the same default Shake check.
                if sess.clone_history().await.shake_history_state() != history.shake_history_state()
                {
                    return Ok(Admission::Rebuild);
                }
            }
            if let Some(attempt) = self.attempt.take() {
                record_reduction(sess, attempt.outcome, attempt.before_tokens, active).await;
            }
            sess.state.lock().await.insufficient_child_context = None;
            return Ok(Admission::Proceed);
        }
        // Pre-turn budget compaction already fitted this Guardian review; estimates alone
        // must not compact it again while it still fits the complete context window.
        if budget_compacted
            && !status
                .full_context_window_limit
                .is_some_and(|limit| active >= limit)
        {
            return Ok(Admission::Proceed);
        }
        // Guardian reviewers recover from the parent checkpoint instead of compacting,
        // exactly as the pre-turn path does.
        if crate::guardian::is_basic_session_source(&turn.session_source)
            && !crate::guardian::should_compact_guardian_input(sess)?
        {
            return Ok(Admission::Proceed);
        }
        let mut content = DefaultHasher::new();
        step.settings.model_info.slug.hash(&mut content);
        for envelope in history.annotated_items() {
            // Automatically refreshed environment/override fragments are not progress.
            if matches!(&envelope.item, ResponseItem::Message { role, content, .. }
                if role == "developer" || role == "system"
                || crate::event_mapping::is_contextual_user_message_content(content))
            {
                continue;
            }
            serde_json::to_vec(&envelope.item)
                .unwrap_or_default()
                .hash(&mut content);
        }
        let marker = InsufficientContext {
            history: content.finish(),
            limits: (child_limit, status.full_context_window_limit, scope_limit),
            scope: turn.config.model_auto_compact_token_limit_scope,
        };
        if let Some(attempt) = self
            .attempt
            .take_if(|attempt| attempt.outcome == ContextReductionOutcome::Compacted)
        {
            record_reduction(
                sess,
                ContextReductionOutcome::Insufficient,
                attempt.before_tokens,
                active,
            )
            .await;
            sess.state.lock().await.insufficient_child_context = Some(marker);
            return Ok(Admission::Insufficient { limit_tokens });
        }
        if sess.state.lock().await.insufficient_child_context.as_ref() == Some(&marker) {
            return Ok(Admission::Insufficient { limit_tokens });
        }
        // A changed context/policy starts a fresh attempt. Failure or cancellation
        // must not reactivate an older completed-insufficiency marker.
        sess.state.lock().await.insufficient_child_context = None;
        if self.attempt.is_none() {
            self.attempt = Some(ReductionAttempt {
                before_tokens: active,
                outcome: ContextReductionOutcome::Shaken,
            });
            if maybe_run_auto_shake(sess, turn).await {
                return Ok(Admission::Rebuild);
            }
        }
        let before = self
            .attempt
            .get_or_insert(ReductionAttempt {
                before_tokens: active,
                outcome: ContextReductionOutcome::Shaken,
            })
            .before_tokens;
        if let Err(err) = run_auto_compact(
            sess,
            Arc::clone(step),
            Arc::clone(step),
            client,
            CompactionReason::ContextLimit,
            CompactionPhase::MidTurn,
        )
        .or_cancel(cancellation)
        .await
        .unwrap_or(Err(codex_protocol::error::CodexErr::TurnAborted))
        {
            super::mid_turn_reduction::record_reduction_error(sess, before, &err).await;
            return Err(err);
        }
        self.attempt = Some(ReductionAttempt {
            before_tokens: before,
            outcome: ContextReductionOutcome::Compacted,
        });
        Ok(Admission::Rebuild)
    }
}
