//! Request-aware observations, carried by existing token and terminal event paths.

use super::context_window::ContextWindowTokenStatus;
use super::context_window::context_window_token_status_for_model;
use super::session::Session;
use super::step_context::StepContext;
use crate::agent::types::AgentContextUsage;
use crate::agent::types::ContextReductionRecord;
use crate::agent::types::ContextTokenBasis;
use crate::client_common::Prompt;
use crate::responses_metadata::CodexResponsesMetadata;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenCountEvent;

pub(super) struct PreparedContext {
    pub(super) status: ContextWindowTokenStatus,
    pub(super) active: i64,
    pub(super) additions: i64,
}

impl Session {
    /// Measures the exact prepared prompt used by admission without repeating preparation.
    pub(super) async fn observe_prepared_context(
        &self,
        step: &StepContext,
        prompt: &Prompt,
        metadata: &CodexResponsesMetadata,
    ) -> CodexResult<PreparedContext> {
        let turn = &step.turn;
        let history = self.clone_history().await;
        let status = context_window_token_status_for_model(
            self,
            &turn.config,
            turn,
            &step.settings.model_info,
        )
        .await;
        let request = self.services.model_client.build_responses_request(
            prompt,
            &step.settings.model_info,
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            metadata,
            /*include_internal*/ true,
        )?;
        let request_tokens =
            i64::try_from(crate::guardian::estimate_request_tokens(&request)).unwrap_or(i64::MAX);
        let baseline = super::turn::build_prompt(
            history
                .clone()
                .for_prompt(&step.settings.model_info.input_modalities),
            step,
            prompt.base_instructions.clone(),
        );
        let baseline = self.services.model_client.build_responses_request(
            &baseline,
            &step.settings.model_info,
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            metadata,
            /*include_internal*/ true,
        )?;
        let history_tokens =
            i64::try_from(crate::guardian::estimate_request_tokens(&baseline)).unwrap_or(i64::MAX);
        let overhead = i64::try_from(crate::guardian::estimate_request_overhead_tokens(
            &request,
            prompt,
            &step.settings.model_info,
        ))
        .unwrap_or(i64::MAX);
        let changed_overhead = {
            let state = self.state.lock().await;
            if state.token_info().is_some() && !state.token_usage_estimated {
                overhead
                    .saturating_sub(
                        state
                            .last_provider_request_overhead_tokens
                            .unwrap_or(/*default*/ 0),
                    )
                    .max(0)
            } else {
                0
            }
        };
        // The current-history baseline cancels current fixed overhead, including Lite
        // prefixes. Compare that overhead with the actual provider measurement's request,
        // then add input-only attachments once. Unknown legacy overhead is conservative.
        let mut additions = request_tokens
            .saturating_sub(history_tokens)
            .max(0)
            .saturating_add(changed_overhead);
        let mut active = request_tokens.max(status.active_context_tokens.saturating_add(additions));
        // A response without usage must not discard already estimated newer context.
        // Partial output retained by an early-close stream also grows this floor on retry.
        // Actual measurements and history rewrites clear this derived floor.
        {
            let state = self.state.lock().await;
            if state.token_info().is_some()
                && !state.token_usage_estimated
                && let Some(previous) = state.prepared_context_tokens
            {
                let growth = request_tokens
                    .saturating_sub(state.prepared_request_tokens.unwrap_or(request_tokens))
                    .max(0);
                active = active.max(previous.saturating_add(growth));
                additions =
                    additions.max(active.saturating_sub(status.active_context_tokens).max(0));
            }
        }
        let child = matches!(turn.session_source, SessionSource::SubAgent(_));
        let enabled = child.then_some(turn.config.subagent_context_reduction.enabled);
        let mut state = self.state.lock().await;
        state.prepared_context_tokens = Some(active);
        state.prepared_request_tokens = Some(request_tokens);
        state.last_context_snapshot = Some(AgentContextUsage {
            active_tokens: active,
            basis: if state.token_info().is_some() && !state.token_usage_estimated {
                ContextTokenBasis::Usage
            } else {
                ContextTokenBasis::Estimate
            },
            last_reduction: state.last_context_reduction.clone(),
            selected_model: (step.settings.model_info.slug.len() <= 128)
                .then(|| step.settings.model_info.slug.clone()),
            child_policy_enabled: enabled,
            child_active_cap_tokens: enabled.filter(|enabled| *enabled).map(|_| {
                i64::try_from(turn.config.subagent_context_reduction.threshold_tokens)
                    .unwrap_or(i64::MAX)
            }),
            model_window_tokens: status.full_context_window_limit,
            observed_at: Some(chrono::Utc::now().timestamp()),
            provider_usage_at: state.provider_usage_at,
            shake_watermark: state
                .history
                .shake_history_state_is_valid()
                .then(|| state.history.shake_history_state().watermark),
        });
        Ok(PreparedContext {
            status,
            active,
            additions,
        })
    }

    /// Records a reduction before the terminal event, even without legacy usage counters.
    pub(crate) async fn record_context_reduction(&self, record: ContextReductionRecord) {
        {
            let mut state = self.state.lock().await;
            state.last_context_reduction = Some(record.clone());
            state.refresh_context_snapshot();
            if let Some(after) = record.after_tokens
                && let Some(snapshot) = &mut state.last_context_snapshot
            {
                snapshot.active_tokens = after;
            }
        }
        self.publish_context_snapshot().await;
    }

    /// Replays captured data without resetting either observation or provider time.
    pub(crate) async fn context_usage_snapshot(&self) -> Option<AgentContextUsage> {
        self.state.lock().await.last_context_snapshot.clone()
    }

    pub(crate) async fn context_usage(&self) -> AgentContextUsage {
        let mut state = self.state.lock().await;
        if state.last_context_snapshot.is_none() {
            state.refresh_context_snapshot();
        }
        state.last_context_snapshot.clone().unwrap_or_default()
    }

    /// Metadata-only token events update observers, not the parent's model or legacy counters.
    pub(crate) async fn publish_context_snapshot(&self) {
        let event = {
            let state = self.state.lock().await;
            state
                .last_started_turn_id
                .as_ref()
                .zip(state.last_context_snapshot.as_ref())
                .map(|(id, snapshot)| Event {
                    id: id.clone(),
                    msg: EventMsg::TokenCount(TokenCountEvent {
                        info: None,
                        rate_limits: None,
                        context_usage: Some(snapshot.clone()),
                    }),
                })
        };
        if let Some(event) = event {
            self.send_event_raw(event).await;
        }
    }
}

impl crate::state::SessionState {
    pub(crate) fn refresh_context_snapshot(&mut self) {
        let active = self
            .get_total_token_usage(self.server_reasoning_included())
            .max(self.prepared_context_tokens.unwrap_or(/*default*/ 0))
            .max(self.prepared_request_tokens.unwrap_or(/*default*/ 0));
        let basis = if self.token_info().is_some() && !self.token_usage_estimated {
            ContextTokenBasis::Usage
        } else {
            ContextTokenBasis::Estimate
        };
        let watermark = self
            .history
            .shake_history_state_is_valid()
            .then(|| self.history.shake_history_state().watermark);
        let snapshot = self.last_context_snapshot.get_or_insert_default();
        snapshot.active_tokens = active;
        snapshot.basis = basis;
        snapshot.last_reduction = self.last_context_reduction.clone();
        snapshot.observed_at = Some(chrono::Utc::now().timestamp());
        snapshot.provider_usage_at = self.provider_usage_at;
        snapshot.shake_watermark = watermark;
    }

    pub(crate) fn restore_context_snapshot(&mut self, items: &[codex_rollout::RolloutItem]) {
        let snapshot = items.iter().rev().find_map(|item| match item {
            codex_rollout::RolloutItem::EventMsg(EventMsg::TokenCount(event)) => {
                event.context_usage.clone()
            }
            _ => None,
        });
        if let Some(mut snapshot) = snapshot {
            if snapshot
                .selected_model
                .as_ref()
                .is_some_and(|model| model.len() > 128)
            {
                snapshot.selected_model = None;
            }
            if snapshot.shake_watermark.is_some_and(|watermark| {
                !self.history.shake_history_state_is_valid()
                    || self.history.shake_history_state().watermark != watermark
            }) {
                snapshot.shake_watermark = None;
            }
            self.provider_usage_at = snapshot.provider_usage_at;
            self.last_context_reduction = snapshot.last_reduction.clone();
            self.token_usage_estimated = snapshot.basis == ContextTokenBasis::Estimate;
            // Unmeasured output already included in a saved usage-based observation
            // must remain charged when this history is resumed.
            self.prepared_context_tokens = Some(snapshot.active_tokens);
            self.prepared_request_tokens = None;
            self.last_context_snapshot = Some(snapshot);
        }
    }
}
