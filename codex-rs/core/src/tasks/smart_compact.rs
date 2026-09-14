use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskResult;
use crate::session::ShakeTrigger;
use crate::session::TurnInput;
use crate::session::apply_shake;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::protocol::TurnStartedEvent;
use tokio_util::sync::CancellationToken;

/// Runs the explicit smart-compact handoff as a cancellable session task.
///
/// The mechanical history rewrite happens before the optional Luna request and
/// is owned by this task, so an interrupt can cancel the request without
/// racing a separate task against a history snapshot.
pub(crate) struct SmartCompactTask {
    pub(crate) expected_fingerprint: Option<String>,
}

impl SessionTask for SmartCompactTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Compact
    }

    fn span_name(&self) -> &'static str {
        "session_task.smart_compact"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        session
            .send_event(
                &ctx,
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: ctx.sub_id.clone(),
                    trace_id: ctx.trace_id.clone(),
                    started_at: ctx.turn_timing_state.started_at_unix_secs().await,
                    model_context_window: ctx.model_context_window(),
                    collaboration_mode_kind: ctx.mode(),
                }),
            )
            .await;
        let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
        session.emit_turn_item_started(&ctx, &compaction_item).await;
        apply_shake(
            &session,
            &ctx,
            ShakeMode::SmartCompact,
            self.expected_fingerprint.clone(),
            ShakeTrigger::Manual,
            &cancellation_token,
        )
        .await;
        session
            .emit_turn_item_completed(&ctx, compaction_item)
            .await;
        Ok(None)
    }
}
