use crate::agent::LocalAgentControl;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;

fn control_with_limit(max_threads: usize) -> LocalAgentControl {
    let control = LocalAgentControl::default();
    control
        .runtime
        .agent_execution_limiter
        .initialize(max_threads);
    control
}

#[tokio::test]
async fn execution_guards_count_active_v2_subagent_turns() {
    let control = control_with_limit(/*max_threads*/ 1);
    // Child role configs cannot replace the root-derived session limit.
    control
        .runtime
        .agent_execution_limiter
        .initialize(/*max_threads*/ 2);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));

    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("first active turn should fit");
    let first = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .await
        .expect("v2 subagent execution should be counted");
    let Err(err) = control.ensure_execution_capacity(MultiAgentVersion::V2, &source) else {
        panic!("second active turn should exceed the derived non-root cap");
    };
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    drop(first);
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("capacity should be released when the running task drops");
}

#[tokio::test]
async fn execution_guards_ignore_root_and_v1_turns() {
    let control = control_with_limit(/*max_threads*/ 0);

    assert!(
        control
            .execution_guard(MultiAgentVersion::V2, &SessionSource::Cli)
            .await
            .is_none()
    );
    assert!(
        control
            .execution_guard(
                MultiAgentVersion::V1,
                &SessionSource::SubAgent(SubAgentSource::Other("worker".to_string())),
            )
            .await
            .is_none()
    );
}

#[tokio::test]
async fn concurrent_turn_admission_never_exceeds_the_configured_limit() {
    let control = control_with_limit(/*max_threads*/ 1);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    let occupied = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .await
        .expect("the first turn reserves the only slot");
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let (acquired_tx, mut acquired_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut waiters = Vec::new();
    for waiter in 0..2 {
        let control = control.clone();
        let source = source.clone();
        let ready_tx = ready_tx.clone();
        let acquired_tx = acquired_tx.clone();
        waiters.push(tokio::spawn(async move {
            ready_tx.send(waiter).expect("ready receiver remains open");
            let guard = control
                .execution_guard(MultiAgentVersion::V2, &source)
                .await
                .expect("configured limiter remains available");
            acquired_tx
                .send((waiter, guard))
                .expect("acquired receiver remains open");
        }));
    }
    drop(ready_tx);
    drop(acquired_tx);
    let _ = ready_rx.recv().await.expect("first waiter is ready");
    let _ = ready_rx.recv().await.expect("second waiter is ready");

    drop(occupied);
    let (first_waiter, first_guard) = acquired_rx
        .recv()
        .await
        .expect("one waiter acquires the released slot");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), acquired_rx.recv())
            .await
            .is_err(),
        "the second waiter must stay blocked while the first owns the permit"
    );
    drop(first_guard);
    let (second_waiter, second_guard) = acquired_rx
        .recv()
        .await
        .expect("the other waiter acquires the released slot");
    assert_ne!(first_waiter, second_waiter);
    drop(second_guard);
    for waiter in waiters {
        waiter.await.expect("admission waiter should finish");
    }
}
