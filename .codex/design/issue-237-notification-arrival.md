# Issue #237: apply subagent visibility at notification arrival

Status: implementation integrated; focused validation, formatting, and independent
code review passed. Ready for root's publication/merge gate.
Source baseline: `84ec4a4d2e2b64a177477b2ad42d603d7348b71d` (`origin/main`).
Owner/worktree: `agent/issue-237-visibility`,
`/home/eric/worktrees/review/codex-237-visibility`.

## Problem and current source trace

`app/app_server_events.rs::handle_app_server_event` routes notifications through
`handle_server_notification_event`. Its existing thread-target/ownership checks
accept a hidden parent when its channel or navigation entry belongs to this TUI;
they reject unrelated and hidden structured threads. Accepted notifications reach
`app/thread_routing.rs::enqueue_thread_notification` regardless of which thread is
displayed. This is the intended arrival boundary, after the abandoned-side guard.

Enqueue already applies picker details/status (`cache_agent_picker_notification`),
observed turn completion, authoritative turn start, unload/close, and row refresh.
It stores every notification in `ThreadEventStore`. Active channels also send it
to their receiver; inactive channels retain only the snapshot copy. A full active
channel schedules its existing asynchronous sender; it must not delay bookkeeping.

The remaining receiver effects instead run in
`thread_routing.rs::handle_thread_event_now` via
`cache_collab_receiver_threads_for_notification`. Active receiver selection and
`drain_active_thread_events_until` determine when this happens. Hidden parent
notifications therefore cannot create a pending grandchild or restore a dismissed
child at arrival. `replay_thread_snapshot` calls `handle_thread_event_replay`,
which forwards notifications directly to `ChatWidget` and never runs the cache.
Changing the selected transcript should not control these effects.

Startup is distinct: `enqueue_primary_thread_notification` retains events until
the primary session is known. `enqueue_primary_thread_session_with_presentation`
then routes those events through normal enqueue once. Historical turns installed
by `ThreadEventStore::set_session`/`set_turns` are snapshots, not fresh arrivals;
their replay must not restore dismissed rows. Backend discovery stays separate.

## Existing semantics to preserve

`multi_agents.rs::sub_agent_activity_display` maps V2 `Started` to a spawn running
hint and `Completed`/`Interrupted` to a stopped hint. `Interacted` has no running
hint; `sub_agent_interaction_thread_id` restores local visibility only.
`agent_navigation.rs::record_sub_agent_activity` keeps `awaiting_first_turn`,
`turn_observed`, and `stopped_threads` so a delayed spawn hint cannot undo an
observed stop or mark an already-run child pending. Closed entries remain closed.
The already-landed #227 guards remain necessary: parent and child streams can
still arrive in a different order even after display latency is removed.

Legacy `CollabAgentToolCall` receiver caching validates IDs, skips `NotFound`,
creates missing metadata, and uses `mark_subagent`/`mark_spawned` for spawn calls.
It does not claim the child's turn has started. V2 activity carries agent paths,
which establish descendant scope even without legacy spawn membership.

`agent_navigation/session_subagents.rs` owns counts, dismissal, and idle-close
targets. Pending-first-turn children and busy descendants are protected from
dismissal/idle-close. Local dismissal leaves metadata, counts, backend state, and
thread attachment intact. Backend archive exclusions remain authoritative;
restoring a local dismissal cannot unarchive a row. Child turn/status arrival
owns actual liveness and reopens an idle unloaded child for a subsequent turn.

## Implementation and file ownership

One implementation worker owns these files; issue #93 owns lifecycle/runtime and
protocol work independently. Do not revert other agents' changes.

1. `codex-rs/tui/src/app/thread_routing.rs`: invoke the existing receiver cache in
   `enqueue_thread_notification`, immediately after the abandoned-side guard and
   before storing/sending the notification. Keep the existing picker/status
   processing and final `update_agent_picker_rows_if_present` so an open picker
   and counts update during the same arrival. Remove the cache call from
   `handle_thread_event_now`; display and replay become consumers of state already
   applied at arrival. Clarify the cache's comment and the misplaced doc comment
   currently attached to `show_wake_mode_hint_once`: receiver effects are local,
   synchronous, and require no server read. No new queue, dedup ledger, protocol
   field, or replay-side state mutation is needed.
2. `codex-rs/tui/src/app/tests/agent_picker_dismissal_tests.rs`: replace the
   `parent_event` helper's direct display dispatch with async enqueue and update
   its existing tests. Add the arrival/order regressions below; use the real
   top-level router for representative hidden-parent cases, not just helpers.
3. `codex-rs/tui/src/app/tests/status_line_subagents_tests.rs`: migrate V2 activity
   arrivals currently dispatched through `handle_thread_event_now` (baseline
   lines 60–68, 92, 105, 117, 165) to async enqueue. Retain actual queue/display
   consumption where streaming assertions need it, so the streaming regression
   continues proving that arrival and presentation preserve an active stream.
   `codex-rs/tui/src/app/tests.rs`: migrate the legacy receiver tests at baseline
   lines 2137 and 2174 to enqueue as well. State tests may still call pure
   navigation APIs to isolate their own invariants.
4. `README.md`, Monitoring subagents: add concise user behavior for local `q`
   dismissal, pending spawn protection, and immediate restoration on parent
   contact even while that parent's transcript is hidden. Issue #93 coordinates
   any other README change; keep this edit within that subsection.
5. This design records final validation and independent review dispositions.

Production changes to `agent_navigation.rs`, `multi_agents.rs`, ownership routing,
backend discovery, and `ThreadEventStore` are not expected. If a test proves an
additional defect, bring its concrete trace back to the owner before expanding.

## Invariants and ordering

- Each accepted notification applies receiver bookkeeping once during normal
  enqueue, independent of displayed thread, snapshot eviction, or active-channel
  backpressure. This is single ownership of local effects, not a new guarantee
  that remote transports deduplicate repeated notifications.
- Draining an older active event, replaying an inactive snapshot, and switching
  transcripts never reapply receiver effects or undo a later local dismissal.
- Parent contact restores local visibility without changing running, closed,
  pending, or stopped state. A dismissed closed review row may become visible
  while retaining `is_closed` and exclusion from open-subagent counts. An archived
  row stays excluded from navigation. Later authoritative child activity may
  change liveness.
- Spawn order stays first-seen; metadata updates never duplicate/reorder entries.
  Legacy spawn remains pending until a turn was observed or it is closed.
- Preserve archived exclusions, child unload/reopen semantics, `NotFound` skips,
  invalid-ID rejection, side-thread filtering, and unrelated-root ownership.
- Do not scan historical turns for live effects on resume/reconnect. Existing
  backend refresh seeds metadata/lifecycle; transcript replay is presentation.

## Meaningful regression and acceptance matrix

Use `make_test_app_with_channels`, a known root and hidden parent channel, and an
embedded picker app server where `handle_app_server_event` needs a client. Keep
root displayed and the parent inactive; send boxed `AppServerEvent::ServerNotification`.

1. **Actual hidden-parent arrival:** V2 `Started` from `/root/parent` creates
   `/root/parent/grandchild` before any parent display/drain/replay. Open the picker
   before arrival and assert the live row/path and counts update. Child `Idle`
   before its first turn clears running but does not permit `q` or idle-close to
   remove it; after a real child start/completion it becomes dismissible.
2. **Hidden-parent contact:** finish and dismiss a known child, then route parent
   `Interacted` while root remains displayed. Row immediately returns in its old
   order; running/closed flags and counts remain unchanged. It can be dismissed
   again immediately because contact does not start a turn.
3. **Active queue stale drain:** activate the parent's channel and enqueue contact
   (and a spawn case if useful), but do not drain. Dismiss the inactive child
   after arrival, then consume the actual queued event through display handling
   (prefer `drain_active_thread_events_until` with existing test TUI support).
   The child must stay dismissed. This catches leaving the old cache call behind.
4. **Hidden snapshot stale replay:** after case 2, dismiss again, snapshot the
   parent's real store, then use `replay_thread_snapshot`/the normal switch path.
   Repeated replay does not restore the child or alter pending/liveness/order.
5. **Late hints in both producer forms:** preserve the existing failed-first-turn
   test, but route parent activity through enqueue. Child start/completion/idle
   arriving before V2 `Started` and legacy spawn receiver hints must leave the
   child stopped and dismissible. Also check a closed review row retains
   `is_closed` and count exclusion after contact (local visibility may restore),
   while an archived row stays hidden after either hint or contact.
6. **Legacy routing:** hidden-parent legacy spawn creates a pathless confirmed
   descendant immediately and protects its pending first turn; malformed receiver
   IDs and completed `NotFound` receivers create no rows. Existing side-thread
   exclusion/count tests must continue passing.
7. **Router rejection:** through `handle_app_server_event`, send receiver-bearing
   V2 activity and legacy collab notifications for an unrelated parent, a temporary
   structured parent, and an abandoned side parent. Assert navigation entries,
   order, counts and dismissed visibility are unchanged, and no new receiver row
   appears. Existing general routing tests do not establish this effect boundary.
8. Preserve existing restart-after-idle-eviction, pending-first-turn, counts,
   archive, picker refresh/liveness, unrelated-thread routing, and channel-full
   regressions. A focused full-channel receiver-effect assertion is useful if
   existing fixtures make it small; do not create a new queue simulator.

Run formatting/diff checks and focused `codex-tui` tests with this worktree's
`codex-worktree env` target directory. Prefer the repository `just test`/nextest
runner, selecting dismissal, agent navigation/picker, `status_line_subagents`,
routing ownership, and thread-event-buffer tests; run the new arrival tests
explicitly. Use one
event-driven supervised command for long compilation. CI owns the full workspace.
Record exact commands, pass counts, and any environment prerequisite separately
from runtime behavior proof. No screenshots/video or shared-service restart is
needed for this bounded state/routing change.

## Delivery gates

Independent plan review precedes implementation. Then focused tests plus an
independent code review (one meaningful delta review at most), with exact reviewed
head and dispositions. Present the uncommitted diff and Zed worktree URL before
publication. Root owns PR/merge and performs one combined build/install only
after both #237 and #93 merge. This lane's acceptance proves actual TUI routing
and state behavior in focused tests; installed binary and fresh-session checks
belong to the combined install gate.

## Independent plan-review dispositions

Reviewer: `/root/visibility_plan_review`, source baseline `84ec4a4d`.
The production move was accepted as sound. Before implementation, three material
clarifications were incorporated: explicit status-line/legacy test migration and
focused status-line coverage; receiver-effect assertions for rejected top-level
router arrivals; and closed review-row restoration distinguished from backend
archive exclusion. Final reviewer verdict: GO at `84ec4a4d`, with no remaining
blocking plan findings. Preserve the #227 stopped/turn-observed guards and keep
contact effects limited to local visibility.

## Implementation and validation evidence

Implementation commit: `4974ee460098230067f0a8f41c99612bd80e24e3` in the
isolated helper worktree `/home/eric/worktrees/review/codex-237-implementation`.
Integrated as `fc809cf881e0435bdaecbe0a771b723adc987502`; the four Rust file
blobs are identical. Production changes are confined to `thread_routing.rs`:
receiver bookkeeping moved to enqueue and its display-side invocation removed.
No liveness/pending guards or protocol/runtime files changed.

The focused suite covers actual top-level-router hidden-parent V2 and legacy
arrivals with an open picker, pending first turns, contact without liveness,
stable order, closed review rows and archive exclusions, rejected unrelated and
temporary/abandoned parent arrivals, full-channel backpressure, stale active
queue draining, repeated snapshot replay, and preserved streaming/status counts.

Recovered initial terminal result: **105 passed, 5,785 skipped**, zero failures,
from `just test -p codex-tui --lib -E 'test(agent_picker) | test(agent_navigation)
| test(status_line_subagents) | test(thread_event) | test(thread_routing) |
test(thread_ownership) | test(collab_receiver)'` with the helper's unique Cargo
target and `CARGO_BUILD_JOBS=4`. Supervisor receipt:
`/home/eric/.local/state/agent-jobs/job-6i5gu60p/receipt.json`; detailed test log:
`/home/eric/.local/state/wait-for-build/build-fe1kh9qi/build.log`.
The native completion handoff was interrupted; the owner recovered that terminal
receipt and launched one repeat from committed source to confirm provenance.
Committed-source repeat: **105 passed, 5,785 skipped**, zero failures, with the
same command and target. Terminal receipt
`/home/eric/.local/state/agent-jobs/job-8ltg126m/receipt.json` records exit zero;
detailed log `/home/eric/.local/state/wait-for-build/build-rxevkj8b/build.log`
records the 12.897-second test run (21.528-second supervised command).
`git diff --check` passes.

Independent full code review: `/root/visibility_code_review`, **PASS, no confirmed
findings**, reviewed owner head `16ddfaff6c38948edc1b1bcbaf58ec4f5dc72785`.
The reviewer verified all four Rust blobs match tested `4974ee4600`, read the
terminal test receipt/log and reviewed README/design integration. No delta review
is required for this documentation-only disposition update.

Formatting: `cargo fmt --all -- --check` completed with exit zero in native
session `82822`; stable rustfmt reported the repository's existing nightly-only
`imports_granularity = Item` warnings. No files changed. Merge and combined
build/install belong to root.
