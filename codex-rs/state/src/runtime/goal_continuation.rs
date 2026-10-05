//! Compact host-owned detector state and goal-conditional continuation holds.

use super::goals::GoalStore;
use codex_protocol::ThreadId;

/// Whether the current detector observation permits another automatic turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalContinuationDisposition {
    Continue,
    Defer,
}

impl GoalStore {
    /// Reads only the current goal's compact detector state.
    pub async fn thread_goal_continuation_guard_state(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT s.state FROM thread_goal_continuation_guard_state s \
             JOIN thread_goals g ON g.thread_id = s.thread_id AND g.goal_id = s.goal_id \
             WHERE s.thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?)
    }

    /// Records bounded, payload-free state and optionally holds the same active goal atomically.
    /// Returns true only when this call creates a new stall hold. Fork holds are preserved.
    pub async fn record_thread_goal_continuation_guard(
        &self,
        thread_id: ThreadId,
        expected_goal_id: &str,
        state: &str,
        disposition: GoalContinuationDisposition,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            state.len() <= 4096,
            "goal guard state exceeds its byte limit"
        );
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO thread_goal_continuation_guard_state (thread_id, goal_id, state) \
             SELECT thread_id, goal_id, ? FROM thread_goals \
             WHERE thread_id = ? AND goal_id = ? AND status = 'active' \
             ON CONFLICT(thread_id) DO UPDATE SET goal_id = excluded.goal_id, state = excluded.state",
        )
        .bind(state)
        .bind(thread_id.to_string())
        .bind(expected_goal_id)
        .execute(&mut *tx)
        .await?;
        let newly_held = if disposition == GoalContinuationDisposition::Defer {
            sqlx::query(
                "INSERT INTO thread_goal_continuation_deferrals (thread_id, goal_id, kind) \
                 SELECT thread_id, goal_id, 'suspected_stall' FROM thread_goals \
                 WHERE thread_id = ? AND goal_id = ? AND status = 'active' \
                 ON CONFLICT(thread_id) DO UPDATE SET goal_id = excluded.goal_id, kind = excluded.kind \
                 WHERE thread_goal_continuation_deferrals.kind != 'fork' \
                 AND thread_goal_continuation_deferrals.goal_id != excluded.goal_id",
            )
            .bind(thread_id.to_string())
            .bind(expected_goal_id)
            .execute(&mut *tx)
            .await?
            .rows_affected() > 0
        } else {
            false
        };
        tx.commit().await?;
        Ok(newly_held)
    }

    /// Recovers only the expected active goal, preserving reset state and fork holds atomically.
    /// Returns false if the goal was replaced or ceased to be active.
    pub async fn recover_thread_goal_continuation_guard(
        &self,
        thread_id: ThreadId,
        expected_goal_id: &str,
        state: &str,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            state.len() <= 4096,
            "goal guard state exceeds its byte limit"
        );
        let mut tx = self.pool.begin().await?;
        let applied = sqlx::query(
            "INSERT INTO thread_goal_continuation_guard_state (thread_id, goal_id, state) \
             SELECT thread_id, goal_id, ? FROM thread_goals \
             WHERE thread_id = ? AND goal_id = ? AND status = 'active' \
             ON CONFLICT(thread_id) DO UPDATE SET goal_id = excluded.goal_id, state = excluded.state",
        ).bind(state).bind(thread_id.to_string()).bind(expected_goal_id)
            .execute(&mut *tx).await?.rows_affected() > 0;
        if applied {
            sqlx::query("DELETE FROM thread_goal_continuation_deferrals WHERE thread_id = ? AND goal_id = ? AND kind = 'suspected_stall'")
                .bind(thread_id.to_string()).bind(expected_goal_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(applied)
    }

    /// Clears a detector hold and history for this goal without releasing a fork hold.
    pub async fn clear_thread_goal_continuation_guard(
        &self,
        thread_id: ThreadId,
        expected_goal_id: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM thread_goal_continuation_deferrals WHERE thread_id = ? AND goal_id = ? AND kind = 'suspected_stall'")
            .bind(thread_id.to_string()).bind(expected_goal_id).execute(&mut *tx).await?;
        sqlx::query(
            "DELETE FROM thread_goal_continuation_guard_state WHERE thread_id = ? AND goal_id = ?",
        )
        .bind(thread_id.to_string())
        .bind(expected_goal_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Preserves the legacy fork contract: its next turn owns goal continuation.
    pub async fn clear_thread_goal_fork_deferral(&self, thread_id: ThreadId) -> anyhow::Result<()> {
        sqlx::query(
            "DELETE FROM thread_goal_continuation_deferrals WHERE thread_id = ? AND kind = 'fork'",
        )
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "goal_continuation_tests.rs"]
mod tests;
