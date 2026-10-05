use super::*;
use crate::SqliteConfig;
use crate::ThreadGoalStatus;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn continuation_guard_survives_reopen_and_rejects_stale_goal_writes() -> anyhow::Result<()> {
    let temp = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(temp.as_path()).await?;
    let sqlite = SqliteConfig::new_for_testing(temp.as_path().abs());
    let migrator = crate::migrations::runtime_goals_migrator();
    let pool = sqlite
        .open_goals_db(&migrator, /*telemetry_override*/ None)
        .await?;
    let store = GoalStore::new(Arc::new(pool));
    let thread = ThreadId::new();
    let goal = store
        .replace_thread_goal(
            thread,
            "objective",
            ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await?;
    assert_eq!(
        store
            .record_thread_goal_continuation_guard(
                thread,
                &goal.goal_id,
                "{\"streak\":2}",
                GoalContinuationDisposition::Defer
            )
            .await?,
        true
    );
    assert_eq!(
        store
            .record_thread_goal_continuation_guard(
                thread,
                &goal.goal_id,
                "{\"streak\":3}",
                GoalContinuationDisposition::Defer
            )
            .await?,
        false
    );
    assert_eq!(store.get_thread_goal(thread).await?, Some(goal.clone()));
    store.close().await;
    let pool = sqlite
        .open_goals_db(&migrator, /*telemetry_override*/ None)
        .await?;
    let store = GoalStore::new(Arc::new(pool));
    assert_eq!(
        store.has_thread_goal_continuation_deferral(thread).await?,
        true
    );
    assert_eq!(
        store.thread_goal_continuation_guard_state(thread).await?,
        Some("{\"streak\":3}".to_owned())
    );
    store.clear_thread_goal_fork_deferral(thread).await?;
    assert_eq!(
        store.has_thread_goal_continuation_deferral(thread).await?,
        true
    );
    let next = store
        .replace_thread_goal(
            thread,
            "replacement",
            ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await?;
    assert_eq!(
        store
            .record_thread_goal_continuation_guard(
                thread,
                &goal.goal_id,
                "stale",
                GoalContinuationDisposition::Defer
            )
            .await?,
        false
    );
    assert_eq!(
        store.has_thread_goal_continuation_deferral(thread).await?,
        false
    );
    assert_eq!(
        store.thread_goal_continuation_guard_state(thread).await?,
        None
    );
    assert_eq!(
        store
            .record_thread_goal_continuation_guard(
                thread,
                &next.goal_id,
                "new",
                GoalContinuationDisposition::Defer
            )
            .await?,
        true
    );
    store
        .clear_thread_goal_continuation_guard(thread, &goal.goal_id)
        .await?;
    assert_eq!(
        store.has_thread_goal_continuation_deferral(thread).await?,
        true
    );
    store
        .clear_thread_goal_continuation_guard(thread, &next.goal_id)
        .await?;
    assert_eq!(
        store.has_thread_goal_continuation_deferral(thread).await?,
        false
    );
    assert_eq!(
        store.thread_goal_continuation_guard_state(thread).await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn continuation_guard_recovery_preserves_fork_deferral() -> anyhow::Result<()> {
    let temp = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(temp.as_path()).await?;
    let sqlite = SqliteConfig::new_for_testing(temp.as_path().abs());
    let pool = sqlite
        .open_goals_db(
            &crate::migrations::runtime_goals_migrator(),
            /*telemetry_override*/ None,
        )
        .await?;
    let store = GoalStore::new(Arc::new(pool));
    let thread = ThreadId::new();
    let goal = store
        .replace_thread_goal(
            thread,
            "inherited",
            ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await?;
    store.replace_thread_goal_snapshot(&goal).await?;
    assert_eq!(
        store
            .record_thread_goal_continuation_guard(
                thread,
                &goal.goal_id,
                "state",
                GoalContinuationDisposition::Defer
            )
            .await?,
        false
    );
    store
        .clear_thread_goal_continuation_guard(thread, &goal.goal_id)
        .await?;
    assert_eq!(
        store.has_thread_goal_continuation_deferral(thread).await?,
        true
    );
    store.clear_thread_goal_fork_deferral(thread).await?;
    assert_eq!(
        store.has_thread_goal_continuation_deferral(thread).await?,
        false
    );
    Ok(())
}

#[tokio::test]
async fn continuation_guard_atomic_recovery_rolls_back_and_reopens_without_losing_receipts()
-> anyhow::Result<()> {
    let temp = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(temp.as_path()).await?;
    let sqlite = SqliteConfig::new_for_testing(temp.as_path().abs());
    let migrator = crate::migrations::runtime_goals_migrator();
    let pool = sqlite
        .open_goals_db(&migrator, /*telemetry_override*/ None)
        .await?;
    let store = GoalStore::new(Arc::new(pool));
    let thread = ThreadId::new();
    let goal = store
        .replace_thread_goal(
            thread,
            "objective",
            ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await?;
    store
        .record_thread_goal_continuation_guard(
            thread,
            &goal.goal_id,
            "old receipt identities",
            GoalContinuationDisposition::Defer,
        )
        .await?;
    // Fail the second write, after state replacement. Both writes must roll back.
    sqlx::query("CREATE TRIGGER fail_guard_recovery BEFORE DELETE ON thread_goal_continuation_deferrals BEGIN SELECT RAISE(ABORT, 'injected recovery failure'); END")
        .execute(store.pool.as_ref()).await?;
    assert!(
        store
            .recover_thread_goal_continuation_guard(thread, &goal.goal_id, "reset identities")
            .await
            .is_err()
    );
    assert_eq!(
        (
            store.thread_goal_continuation_guard_state(thread).await?,
            store.has_thread_goal_continuation_deferral(thread).await?
        ),
        (Some("old receipt identities".to_owned()), true)
    );
    sqlx::query("DROP TRIGGER fail_guard_recovery")
        .execute(store.pool.as_ref())
        .await?;
    store.close().await;
    let pool = sqlite
        .open_goals_db(&migrator, /*telemetry_override*/ None)
        .await?;
    let store = GoalStore::new(Arc::new(pool));
    assert_eq!(
        (
            store.thread_goal_continuation_guard_state(thread).await?,
            store.has_thread_goal_continuation_deferral(thread).await?
        ),
        (Some("old receipt identities".to_owned()), true)
    );
    assert!(
        store
            .recover_thread_goal_continuation_guard(thread, &goal.goal_id, "reset identities")
            .await?
    );
    store.close().await;
    let pool = sqlite
        .open_goals_db(&migrator, /*telemetry_override*/ None)
        .await?;
    let store = GoalStore::new(Arc::new(pool));
    assert_eq!(
        (
            store.thread_goal_continuation_guard_state(thread).await?,
            store.has_thread_goal_continuation_deferral(thread).await?
        ),
        (Some("reset identities".to_owned()), false)
    );
    let replacement = store
        .replace_thread_goal(
            thread,
            "replacement",
            ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await?;
    store
        .record_thread_goal_continuation_guard(
            thread,
            &replacement.goal_id,
            "replacement identities",
            GoalContinuationDisposition::Defer,
        )
        .await?;
    assert!(
        !store
            .recover_thread_goal_continuation_guard(thread, &goal.goal_id, "stale reset")
            .await?
    );
    assert_eq!(
        (
            store.thread_goal_continuation_guard_state(thread).await?,
            store.has_thread_goal_continuation_deferral(thread).await?
        ),
        (Some("replacement identities".to_owned()), true)
    );
    store.replace_thread_goal_snapshot(&replacement).await?;
    assert!(
        store
            .recover_thread_goal_continuation_guard(
                thread,
                &replacement.goal_id,
                "fork receipt identities"
            )
            .await?
    );
    assert_eq!(
        (
            store.thread_goal_continuation_guard_state(thread).await?,
            store.has_thread_goal_continuation_deferral(thread).await?
        ),
        (Some("fork receipt identities".to_owned()), true)
    );
    Ok(())
}
