# Queue wake suppression while delegated work is outstanding

Source base: 351b23269ac1d2d8ae9c2e868d48fe2c1f9e0ce6.

A parent in wake mode completes its current model turn while a descendant is still working. Core publishes `AgentStatus::Waiting`, clears the active-turn slot, and keeps the same coordinator assignment open. This state can accept explicit queued user input. The queue extension instead excluded `Waiting` from both immediate enqueue wake and external SQLite-change dispatch, suppressing admission indefinitely until another source (such as a child report) started a turn.

The narrow correction removes `Waiting` from those two exclusions. Dispatch continues through the normal idle lifecycle and `start_turn_if_idle`: active-turn reservation remains atomic, interrupted threads remain paused, drain refusals retain queued rows, and a row is removed only after accepted admission. A waiting assignment resumes its existing generation with descendant/report relationships intact. Explicit user input starts a new causal turn; it does not copy a previous turn's root metadata.

The external-store observer retains its existing ten-second interval. This correction removes indefinite suppression; it does not make that observer instantaneous.

Regression evidence must use a real spawned worker, gated before its terminal response, and the real thread-idle lifecycle after the parent completes. Verify queue wake before releasing the worker, then release its real final report and verify one parent report turn. Run both direct service enqueue and a separate SQLite connection observed by the external watcher. Keep active-turn, actual interruption, failed-admission and FIFO coverage separate.

Source-level proof does not establish the status of a specific live incident. No live messages, service restart, package installation, or daemon activation is part of these tests.

## Focused evidence (2026-10-22)

On the unchanged production source, both real waiting-parent regressions failed at the queued-wake deadline. Failure assertions confirmed the row remained stored, the parent remained `Waiting`, and the original child remained `Running`.

With the two exclusions removed, all nine focused queue-service checks passed: direct and external waiting-parent wakes, retained real child report delivery and causal lineage, admission refusal while waiting, actual active/interrupted boundaries, FIFO, ordinary admission refusal, selected-item active deferral, and independent external-store dispatch/retry. The waiting-parent fixture gates the admitted user turn separately so explicit-wake lifecycle notifications cannot substitute for its later natural active-turn cleanup.
