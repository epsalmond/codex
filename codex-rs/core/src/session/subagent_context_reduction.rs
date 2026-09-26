//! Safe sampling-boundary context management for spawned subagents.

use std::sync::Arc;

use crate::agent::context_policy::ResolvedSubagentContextPolicy;
use crate::agent::types::AgentContextReductionAttempt;
use crate::agent::types::AgentContextReductionStage;
use crate::client::ModelClientSession;
use crate::compact::InitialContextInjection;
use crate::context::world_state::WorldState;
use crate::session::context_window::context_window_token_status;
use crate::session::handlers::ShakeTrigger;
use crate::session::session::Session;
use crate::session::turn::AutoShakeOutcome;
use crate::session::turn::AutoShakePassOptions;
use crate::session::turn::run_auto_compact;
use crate::session::turn::run_auto_shake_pass;
use crate::session::turn_context::TurnContext;
use crate::state::SuppressedContextReduction;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_config::config_toml::SubagentContextReductionOnFailure;
use codex_config::config_toml::SubagentContextReductionShake;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use tokio_util::sync::CancellationToken;

const CONTEXT_GROWTH_RETRY_TOKENS: i64 = 4_000;

#[derive(Debug)]
pub(crate) enum BoundaryOutcome {
    NoAction,
    Compacted,
    Stop(String),
}

pub(super) struct ReductionBoundary {
    pub(super) phase: CompactionPhase,
    pub(super) explicit_context_reset: bool,
    pub(super) forced_model_rollover: bool,
}

/// Applies the child policy before a fresh step context is captured.
pub(crate) async fn reduce_before_sampling(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    client_session: &mut ModelClientSession,
    cancellation_token: &CancellationToken,
    world_state: Option<&Arc<WorldState>>,
    boundary: ReductionBoundary,
) -> CodexResult<BoundaryOutcome> {
    let ReductionBoundary {
        phase,
        explicit_context_reset,
        forced_model_rollover,
    } = boundary;
    let Some((policy, desired_revision, _, _)) = session.context_reduction_policy_status().await
    else {
        return Ok(BoundaryOutcome::NoAction);
    };
    session.mark_context_policy_applied(desired_revision).await;

    let before_status = context_window_token_status(session, turn_context).await;
    record_usage(session, turn_context, &policy, &before_status).await;
    if policy.enabled && before_status.active_context_tokens < policy.threshold_tokens {
        session.clear_suppressed_context_reduction().await;
        session.clear_context_reduction_failure_episode().await;
    }
    let reason = if explicit_context_reset {
        "explicit_context_reset"
    } else if forced_model_rollover || before_status.token_limit_reached {
        "model_context_limit"
    } else {
        "child_context_threshold"
    };
    let threshold_reached =
        policy.enabled && before_status.active_context_tokens >= policy.threshold_tokens;
    let forced_reduction =
        explicit_context_reset || forced_model_rollover || before_status.token_limit_reached;
    let normal_auto_shake = !policy.enabled
        || (policy.shake == SubagentContextReductionShake::Inherit
            && !threshold_reached
            && !forced_reduction);
    if !threshold_reached && !forced_reduction && !normal_auto_shake {
        return Ok(BoundaryOutcome::NoAction);
    }

    let (_, suppressed) = session.context_reduction_observations().await;
    let suppression_active = clear_or_check_suppression(
        session,
        suppressed,
        desired_revision,
        before_status.active_context_tokens,
        policy.threshold_tokens,
    )
    .await;
    if suppression_active && threshold_reached && !forced_reduction {
        return Ok(BoundaryOutcome::NoAction);
    }

    let shake = if normal_auto_shake {
        { super::turn::maybe_run_pre_sampling_auto_shake(session, turn_context).await }
    } else {
        match policy.shake {
            SubagentContextReductionShake::Off => AutoShakeOutcome {
                skipped_reason: Some("disabled_by_policy".to_string()),
                ..Default::default()
            },
            SubagentContextReductionShake::On if threshold_reached || forced_reduction => {
                forced_shake(
                    session,
                    turn_context,
                    before_status.active_context_tokens,
                    &policy,
                )
                .await
            }
            SubagentContextReductionShake::Inherit if threshold_reached || forced_reduction => {
                let settings = turn_context
                    .config
                    .auto_shake
                    .settings_for_model(turn_context.model_info().slug.as_str());
                if settings.enabled {
                    forced_shake(
                        session,
                        turn_context,
                        before_status.active_context_tokens,
                        &policy,
                    )
                    .await
                } else {
                    AutoShakeOutcome {
                        skipped_reason: Some("auto_shake_disabled".to_string()),
                        ..Default::default()
                    }
                }
            }
            SubagentContextReductionShake::On => AutoShakeOutcome {
                skipped_reason: Some("below_child_threshold".to_string()),
                ..Default::default()
            },
            SubagentContextReductionShake::Inherit => {
                unreachable!("inherit below threshold uses normal auto-shake")
            }
        }
    };

    let after_shake_status = context_window_token_status(session, turn_context).await;
    record_usage(session, turn_context, &policy, &after_shake_status).await;
    if policy.enabled && after_shake_status.active_context_tokens < policy.threshold_tokens {
        session.clear_suppressed_context_reduction().await;
        session.clear_context_reduction_failure_episode().await;
    }
    let reduction_still_needed = should_compact_after_shake(
        explicit_context_reset,
        after_shake_status.token_limit_reached,
        after_shake_status.active_context_tokens,
        &policy,
    );
    if !reduction_still_needed {
        if shake.attempted {
            record_attempt(
                session,
                AgentContextReductionAttempt {
                    at: chrono::Utc::now(),
                    reason: if !threshold_reached && !forced_reduction {
                        "configured_auto_shake"
                    } else {
                        reason
                    }
                    .to_string(),
                    before_tokens: before_status.active_context_tokens,
                    after_tokens: after_shake_status.active_context_tokens,
                    shake: Some(shake_stage(
                        &shake,
                        before_status.active_context_tokens,
                        after_shake_status.active_context_tokens,
                    )),
                    compact: None,
                    outcome: "shaken".to_string(),
                },
                None,
            )
            .await;
        }
        return Ok(BoundaryOutcome::NoAction);
    }

    let step_context = session
        .capture_step_context(Arc::clone(turn_context), cancellation_token)
        .await?;
    let initial_context_injection = match phase {
        CompactionPhase::PreTurn => InitialContextInjection::DoNotInject,
        CompactionPhase::MidTurn => {
            let Some(world_state) = world_state else {
                return Err(CodexErr::TurnAborted);
            };
            InitialContextInjection::BeforeLastUserMessage {
                world_state: Arc::clone(world_state),
                step_context: Arc::clone(&step_context),
            }
        }
        CompactionPhase::PostTurn => InitialContextInjection::DoNotInject,
        _ => InitialContextInjection::DoNotInject,
    };
    let compact_result = run_auto_compact(
        session,
        step_context,
        /*fallback_step_context*/ None,
        client_session,
        initial_context_injection,
        CompactionReason::ContextLimit,
        phase,
    )
    .await;
    let after_compact_status = context_window_token_status(session, turn_context).await;
    record_usage(session, turn_context, &policy, &after_compact_status).await;
    if let Err(error) = compact_result {
        let compact_stage = AgentContextReductionStage {
            outcome: "failed".to_string(),
            reason: Some(error.to_string()),
        };
        record_attempt(
            session,
            AgentContextReductionAttempt {
                at: chrono::Utc::now(),
                reason: reason.to_string(),
                before_tokens: before_status.active_context_tokens,
                after_tokens: after_compact_status.active_context_tokens,
                shake: Some(shake_stage(
                    &shake,
                    before_status.active_context_tokens,
                    after_shake_status.active_context_tokens,
                )),
                compact: Some(compact_stage),
                outcome: "failed".to_string(),
            },
            continue_suppression(
                &policy,
                desired_revision,
                after_compact_status.active_context_tokens,
            ),
        )
        .await;
        if matches!(
            error.details(),
            codex_protocol::error::CodexErrorDetails::TurnAborted
                | codex_protocol::error::CodexErrorDetails::Interrupted
        ) {
            return Err(error);
        }
        if after_shake_status.token_limit_reached || explicit_context_reset {
            return Err(error);
        }
        return failure_outcome(
            session,
            desired_revision,
            &policy,
            error.to_string(),
            after_compact_status.active_context_tokens,
        )
        .await;
    }

    if after_compact_status.token_limit_reached {
        record_attempt(
            session,
            AgentContextReductionAttempt {
                at: chrono::Utc::now(),
                reason: reason.to_string(),
                before_tokens: before_status.active_context_tokens,
                after_tokens: after_compact_status.active_context_tokens,
                shake: Some(shake_stage(
                    &shake,
                    before_status.active_context_tokens,
                    after_shake_status.active_context_tokens,
                )),
                compact: Some(AgentContextReductionStage {
                    outcome: "applied".to_string(),
                    reason: None,
                }),
                outcome: "model_limit_still_reached".to_string(),
            },
            None,
        )
        .await;
        return Err(CodexErr::ContextWindowExceeded);
    }

    if policy.enabled && after_compact_status.active_context_tokens >= policy.threshold_tokens {
        let failure = format!(
            "context reduction completed but the child remains above its {} token threshold",
            policy.threshold_tokens
        );
        record_attempt(
            session,
            AgentContextReductionAttempt {
                at: chrono::Utc::now(),
                reason: reason.to_string(),
                before_tokens: before_status.active_context_tokens,
                after_tokens: after_compact_status.active_context_tokens,
                shake: Some(shake_stage(
                    &shake,
                    before_status.active_context_tokens,
                    after_shake_status.active_context_tokens,
                )),
                compact: Some(AgentContextReductionStage {
                    outcome: "applied_but_insufficient".to_string(),
                    reason: None,
                }),
                outcome: "threshold_still_reached".to_string(),
            },
            continue_suppression(
                &policy,
                desired_revision,
                after_compact_status.active_context_tokens,
            ),
        )
        .await;
        return failure_outcome(
            session,
            desired_revision,
            &policy,
            failure,
            after_compact_status.active_context_tokens,
        )
        .await;
    }

    record_attempt(
        session,
        AgentContextReductionAttempt {
            at: chrono::Utc::now(),
            reason: reason.to_string(),
            before_tokens: before_status.active_context_tokens,
            after_tokens: after_compact_status.active_context_tokens,
            shake: Some(shake_stage(
                &shake,
                before_status.active_context_tokens,
                after_shake_status.active_context_tokens,
            )),
            compact: Some(AgentContextReductionStage {
                outcome: "applied".to_string(),
                reason: None,
            }),
            outcome: "compacted".to_string(),
        },
        None,
    )
    .await;
    Ok(BoundaryOutcome::Compacted)
}

async fn forced_shake(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    active_context_tokens: i64,
    policy: &ResolvedSubagentContextPolicy,
) -> AutoShakeOutcome {
    if turn_context.config.ephemeral {
        return AutoShakeOutcome {
            skipped_reason: Some("ephemeral_thread_requires_compaction".to_string()),
            ..Default::default()
        };
    }
    let settings = turn_context
        .config
        .auto_shake
        .settings_for_model(turn_context.model_info().slug.as_str());
    let mut outcome = run_auto_shake_pass(
        session,
        turn_context,
        AutoShakePassOptions {
            model_slug: turn_context.model_info().slug.as_str(),
            active_context_tokens,
            protect_tokens: crate::shake::AUTO_PROTECT_TOKENS,
            min_elidable_percent: settings.min_elidable_percent,
            min_savings_tokens: settings.min_savings_tokens,
            trigger: ShakeTrigger::Automatic,
        },
    )
    .await;
    let after_first = context_window_token_status(session, turn_context).await;
    if after_first.token_limit_reached
        || after_first.active_context_tokens >= policy.threshold_tokens
    {
        outcome.merge(
            run_auto_shake_pass(
                session,
                turn_context,
                AutoShakePassOptions {
                    model_slug: turn_context.model_info().slug.as_str(),
                    active_context_tokens: after_first.active_context_tokens,
                    protect_tokens: crate::shake::MANUAL_PROTECT_TOKENS,
                    min_elidable_percent: settings.min_elidable_percent,
                    min_savings_tokens: settings.min_savings_tokens / 2,
                    trigger: ShakeTrigger::AutomaticEscalated,
                },
            )
            .await,
        );
    }
    outcome
}

fn shake_stage(
    shake: &AutoShakeOutcome,
    before_tokens: i64,
    after_tokens: i64,
) -> AgentContextReductionStage {
    let outcome = if shake.attempted && after_tokens < before_tokens {
        "applied"
    } else if shake.artifact_save_failures > 0 {
        "artifact_save_failed"
    } else if shake.attempted {
        "insufficient"
    } else {
        "skipped"
    };
    AgentContextReductionStage {
        outcome: outcome.to_string(),
        reason: shake.skipped_reason.clone(),
    }
}

fn continue_suppression(
    policy: &ResolvedSubagentContextPolicy,
    revision: u64,
    after_tokens: i64,
) -> Option<SuppressedContextReduction> {
    (policy.on_failure == SubagentContextReductionOnFailure::Continue).then_some({
        SuppressedContextReduction {
            after_tokens,
            policy_revision: revision,
        }
    })
}

async fn clear_or_check_suppression(
    session: &Session,
    suppression: Option<SuppressedContextReduction>,
    revision: u64,
    active_tokens: i64,
    threshold_tokens: i64,
) -> bool {
    let Some(suppression) = suppression else {
        return false;
    };
    match suppression_decision(&suppression, revision, active_tokens, threshold_tokens) {
        SuppressionDecision::Suppress => true,
        SuppressionDecision::Recovered | SuppressionDecision::PolicyChanged => {
            session.clear_suppressed_context_reduction().await;
            session.clear_context_reduction_failure_episode().await;
            false
        }
        SuppressionDecision::Growth => {
            session.clear_suppressed_context_reduction().await;
            false
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SuppressionDecision {
    Suppress,
    Growth,
    Recovered,
    PolicyChanged,
}

fn suppression_decision(
    suppression: &SuppressedContextReduction,
    revision: u64,
    active_tokens: i64,
    threshold_tokens: i64,
) -> SuppressionDecision {
    if active_tokens < threshold_tokens {
        SuppressionDecision::Recovered
    } else if suppression.policy_revision != revision {
        SuppressionDecision::PolicyChanged
    } else if active_tokens.saturating_sub(suppression.after_tokens) >= CONTEXT_GROWTH_RETRY_TOKENS
    {
        SuppressionDecision::Growth
    } else {
        SuppressionDecision::Suppress
    }
}

fn should_compact_after_shake(
    explicit_context_reset: bool,
    model_token_limit_reached: bool,
    active_context_tokens: i64,
    policy: &ResolvedSubagentContextPolicy,
) -> bool {
    explicit_context_reset
        || model_token_limit_reached
        || (policy.enabled && active_context_tokens >= policy.threshold_tokens)
}

async fn failure_outcome(
    session: &Session,
    revision: u64,
    policy: &ResolvedSubagentContextPolicy,
    message: String,
    active_tokens: i64,
) -> CodexResult<BoundaryOutcome> {
    match policy.on_failure {
        SubagentContextReductionOnFailure::Stop => Ok(BoundaryOutcome::Stop(message)),
        SubagentContextReductionOnFailure::Continue => {
            session
                .notify_parent_of_context_reduction_failure_once(
                    revision,
                    format!(
                        "Subagent context reduction did not reach its threshold at {active_tokens} active tokens; continuing. It will retry after {CONTEXT_GROWTH_RETRY_TOKENS} more active tokens or a policy change."
                    ),
                )
                .await;
            Ok(BoundaryOutcome::NoAction)
        }
    }
}

async fn record_attempt(
    session: &Session,
    attempt: AgentContextReductionAttempt,
    suppression: Option<SuppressedContextReduction>,
) {
    session
        .record_context_reduction_attempt(attempt, suppression)
        .await;
}

async fn record_usage(
    session: &Session,
    turn_context: &TurnContext,
    policy: &ResolvedSubagentContextPolicy,
    status: &super::context_window::ContextWindowTokenStatus,
) {
    let auto_compact_scope = turn_context
        .config
        .model_auto_compact_token_limit_scope
        .to_string();
    session
        .record_context_policy_usage(
            status.active_context_tokens,
            status.active_context_token_basis,
            policy.threshold_tokens,
            status.full_context_window_limit,
            &auto_compact_scope,
            status.auto_compact_scope_limit,
        )
        .await;
}

#[cfg(test)]
#[path = "subagent_context_reduction_tests.rs"]
mod tests;
