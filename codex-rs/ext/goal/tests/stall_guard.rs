#![allow(dead_code)]

#[path = "../src/stall.rs"]
mod stall;
#[path = "../src/stall_guard.rs"]
mod stall_guard;
#[path = "../src/stall_live.rs"]
mod stall_live;
#[path = "../src/stall_observation.rs"]
mod stall_observation;
#[path = "../src/stall_settings.rs"]
mod stall_settings;

use codex_core::config::GoalContinuationGuardMode;
use codex_core::config::GoalsToml;
use codex_protocol::ThreadId;
use codex_state::StateRuntime;
use codex_state::ThreadGoal;
use codex_state::ThreadGoalStatus;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use stall_guard::ContinuationGuard;
use std::num::NonZeroU32;
use tempfile::TempDir;

async fn fixture() -> anyhow::Result<(TempDir, std::sync::Arc<StateRuntime>, ThreadGoal)> {
    let home = TempDir::new()?;
    let state = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test".to_owned(),
    )
    .await?;
    let thread = ThreadId::new();
    let metadata = codex_state::ThreadMetadataBuilder::new(
        thread,
        state.sqlite().home().join("rollout.jsonl"),
        chrono::Utc::now(),
        codex_protocol::protocol::SessionSource::Cli,
    );
    state.upsert_thread(&metadata.build("test")).await?;
    let goal = state
        .thread_goals()
        .replace_thread_goal(
            thread,
            "private objective sentinel",
            ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await?;
    Ok((home, state, goal))
}

fn guard(mode: GoalContinuationGuardMode) -> ContinuationGuard {
    ContinuationGuard::new(
        mode,
        NonZeroU32::MIN.saturating_add(/*rhs*/ 1),
        &GoalsToml::default(),
    )
}

async fn empty_turn(
    guard: &ContinuationGuard,
    state: &StateRuntime,
    goal: &ThreadGoal,
    id: &str,
) -> anyhow::Result<bool> {
    guard.begin(id, goal);
    guard.admitted(id);
    guard
        .finish(state.thread_goals(), goal.thread_id, id, goal)
        .await
        .map_err(anyhow::Error::msg)
}

#[tokio::test]
async fn active_hold_and_report_deduplication_survive_guard_reopen() -> anyhow::Result<()> {
    let (_home, state, goal) = fixture().await?;
    let initial = guard(GoalContinuationGuardMode::Defer);
    assert!(!empty_turn(&initial, &state, &goal, "turn1").await?);
    assert!(empty_turn(&initial, &state, &goal, "turn2").await?);
    assert_eq!(
        state
            .thread_goals()
            .get_thread_goal(goal.thread_id)
            .await?
            .map(|goal| goal.status),
        Some(ThreadGoalStatus::Active)
    );
    let encoded = state
        .thread_goals()
        .thread_goal_continuation_guard_state(goal.thread_id)
        .await?;
    assert!(encoded.as_ref().is_some_and(
        |encoded| encoded.len() <= 4096 && !encoded.contains("private objective sentinel")
    ));
    let resumed = guard(GoalContinuationGuardMode::Defer);
    assert!(!resumed.restore(&goal, encoded.as_deref()));
    assert!(!empty_turn(&resumed, &state, &goal, "turn2").await?);
    let report = stall::digest("accepted report id");
    assert!(
        resumed
            .recover(state.thread_goals(), &goal, Some(report))
            .await
            .map_err(anyhow::Error::msg)?
    );
    assert!(
        !state
            .thread_goals()
            .has_thread_goal_continuation_deferral(goal.thread_id)
            .await?
    );
    assert!(!empty_turn(&resumed, &state, &goal, "turn3").await?);
    assert!(empty_turn(&resumed, &state, &goal, "turn4").await?);
    let encoded = state
        .thread_goals()
        .thread_goal_continuation_guard_state(goal.thread_id)
        .await?;
    let reopened = guard(GoalContinuationGuardMode::Defer);
    reopened.restore(&goal, encoded.as_deref());
    assert!(
        !reopened
            .recover(state.thread_goals(), &goal, Some(report))
            .await
            .map_err(anyhow::Error::msg)?
    );
    assert!(
        state
            .thread_goals()
            .has_thread_goal_continuation_deferral(goal.thread_id)
            .await?
    );
    // Unknown encrypted text can still recover through a new accepted host identity.
    assert!(
        reopened
            .recover(
                state.thread_goals(),
                &goal,
                Some(stall::digest("new encrypted report id"))
            )
            .await
            .map_err(anyhow::Error::msg)?
    );
    assert!(
        !state
            .thread_goals()
            .has_thread_goal_continuation_deferral(goal.thread_id)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn legacy_clear_does_not_reuse_stale_history_on_new_runtime() -> anyhow::Result<()> {
    let (_home, state, goal) = fixture().await?;
    let prior = guard(GoalContinuationGuardMode::Defer);
    empty_turn(&prior, &state, &goal, "old1").await?;
    assert!(empty_turn(&prior, &state, &goal, "old2").await?);
    let encoded = state
        .thread_goals()
        .thread_goal_continuation_guard_state(goal.thread_id)
        .await?;
    // Simulate the old SQL API: it releases a hold without updating new digest
    // history. Its subsequent manual/productive work cannot record observations.
    state
        .thread_goals()
        .clear_thread_goal_continuation_deferral(goal.thread_id)
        .await?;
    assert_eq!(
        state
            .thread_goals()
            .thread_goal_continuation_guard_state(goal.thread_id)
            .await?,
        encoded
    );
    let resumed = guard(GoalContinuationGuardMode::Defer);
    assert!(!resumed.restore(&goal, encoded.as_deref()));
    assert!(!empty_turn(&resumed, &state, &goal, "new1").await?);
    assert!(
        !state
            .thread_goals()
            .has_thread_goal_continuation_deferral(goal.thread_id)
            .await?
    );
    assert!(empty_turn(&resumed, &state, &goal, "new2").await?);
    Ok(())
}
