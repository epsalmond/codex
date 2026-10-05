use codex_extension_api::ExtensionWarning;

use super::GoalRuntimeHandle;
use crate::stall::Digest;
use crate::stall_guard::ContinuationGuard;

impl GoalRuntimeHandle {
    pub(crate) fn guard(&self) -> &ContinuationGuard {
        &self.inner.guard
    }

    pub(crate) fn goal_store(&self) -> &codex_state::GoalStore {
        self.inner.state_dbs.thread_goals()
    }

    pub(crate) async fn initialize_guard(&self) -> Result<(), String> {
        let _permit = self.goal_state_permit().await?;
        let Some(goal) = self
            .goal_store()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(());
        };
        let encoded = self
            .goal_store()
            .thread_goal_continuation_guard_state(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        if self.guard().restore(&goal, encoded.as_deref()) {
            self.goal_store()
                .clear_thread_goal_continuation_guard(self.thread_id(), &goal.goal_id)
                .await
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    pub(crate) async fn recover_guard(&self, report: Option<Digest>) -> Result<(), String> {
        let _permit = self.goal_state_permit().await?;
        if let Some(goal) = self
            .goal_store()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        {
            self.guard()
                .recover(self.goal_store(), &goal, report)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn finish_guard(&self, turn_id: &str) -> Result<(), String> {
        // Admission holds this permit until its successful automatic mark. Goal
        // accounting runs before this callback, so budget/status remain authoritative.
        let _permit = self.goal_state_permit().await?;
        let Some(goal) = self
            .goal_store()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(());
        };
        if goal.status == codex_state::ThreadGoalStatus::Active
            && self
                .guard()
                .finish(self.goal_store(), self.thread_id(), turn_id, &goal)
                .await?
        {
            let message =
                if self.guard().mode == codex_core::config::GoalContinuationGuardMode::Defer {
                    include_str!("../templates/goals/stall_held.md")
                } else {
                    include_str!("../templates/goals/stall_observed.md")
                };
            self.inner
                .event_emitter
                .sink
                .emit_warning(ExtensionWarning {
                    thread_id: self.thread_id().to_string(),
                    turn_id: Some(turn_id.to_owned()),
                    message: message.trim().to_owned(),
                });
        }
        Ok(())
    }
}
