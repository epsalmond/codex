# Issue 93: Task, Cell and Service lifecycle and exec drain

Status: independent source plan review GO; implementation in progress. Audited base:
`84ec4a4d2e` (`origin/main`); worktree `codex-93-task-service`, branch
`agent/issue-93-task-service`. Audit date: 2026-10-24. Implementation progress and
validation are recorded below. Issue text: `/tmp/codex-parallel-237-93/issue-93.md`.
The latest owner comments are authoritative for current scope:
[2026-10-15 ownership](https://github.com/epsalmond/codex/issues/93#issuecomment-6182298928)
assigns Cell production/polling refusal to #79; the
[2026-10-20 rescope](https://github.com/epsalmond/codex/issues/93#issuecomment-6303799343)
says the remainder is command **or cell** Task/Service declaration and Service
drain exclusion. The matrix below audits inherited behavior; its shipped rows
are regression gates, not a mandate to rebuild that behavior.

Independent review resolved finite child assignment retention, all-registration
residency fencing and confirmed remote shutdown, then the precise nonautomatic
finite hint gate/acknowledgement contract. Shared lifecycle API production commit
`580bb08` is integrated as `e976fba09d`; focused test delta and native/Cell
producers follow. Actual API uses `reserve_owned_work`,
`pin_owned_work_owner`, `reserve_with_lifecycle` and
`reserve_cell_with_lifecycle`; Service cells mark their retained-cell authority
explicitly with `OwnedWorkRegistration::mark_cell`. Focused runtime evidence and
remaining gates are recorded in the settlement/validation sections below.

## Outcome and scope

Finite native commands default to Task; Code Mode executions are finite Cell;
either a native command or a cell can explicitly declare Service. A finite producer admitted before exec
finishes keeps the observed execution scope open through terminal collection and
automatic result acceptance. Service stays owned by the session and has the same
terminal reporting path, but never contributes to drain quiescence. Closing the
scope stops its services. This closes the complete remaining issue, including
classification in both native and Cell paths, shutdown/admission races, docs and
behavioral acceptance. The source audit finds a root-native/cell drain accounting
gap adjacent to this declaration, so the finite hold must also be made real;
this does not require a new producer or result transport.

Keep the one existing `AsyncCompletions` store, `CompletionReservation` /
`CompletionPublisher`, `CellCompletion` collector, ordinary model acceptance
boundary, and idle wake scheduler. Lifecycle bookkeeping contains no output or
second delivery queue. Do not change default activation twice, add model waits,
replace the agent assignment system, or change #237's TUI arrival bookkeeping.

## Requirement status at the audited base

| Issue criterion | Current implementation/evidence | Remaining work |
| --- | --- | --- |
| Work observation/default exec drain | `agent/control/work_lifecycle.rs`, `exec/src/work_lifecycle.rs`, `exec/src/lib.rs:1180`; #95 landed | Extend existing coordinator; retain subscribe/revision-guarded close and opt-outs. |
| Default automatic completion | `config/defaults.toml:21` enables it (#161); raw `FeatureSpec` remains false intentionally | No feature-default rewrite. Test packaged defaults only if touched. |
| Task success/failure/cancel result | #145/#146; `unified_exec/process_manager.rs:exec_command`, `async_watcher.rs:spawn_exit_watcher`; one bounded store | Existing process result path stays; explicit Task kind and lifecycle registration missing. |
| Owner identity/assignment generation | Store records owner, originating turn/call, native/cell ID, assignment; `CompletionDrain`; #149 root recovery | Preserve exact assignment gating; no service result reassignment to a new generation. |
| Task drain | `TreeWorkSummary` counts only open assignments/reports/wakes; `WorkCoordinator::snapshot` has no producer counts | Register all finite native producers, including root-owned work, before launch. Root without an assignment currently has a close-before-completion window. |
| Child producer retention | `coordinator/owned_work.rs`, assignment lifecycle checks `owns_assignment`; #151 | Retain finite work; running services must not implicitly hold assignments open. |
| Cell producer and transfer | #79 source is present: `reserve_cell`, `CellCompletion`, native terminal readiness callback, observation lock, initial-yield/interrupt logic | Register Cell into the existing work observation/drain. Do not recreate collector/readiness/queue. |
| Cell compatibility | Provider without readiness retains `wait`; fallback watcher exists for hidden-wait cases; media policy is explicit | Lifecycle must cover live cells even when completion reservation is released for visible `wait`, and release at native close. |
| Persistent Service declaration | No WorkKind or service field in native exec args/request/process entry or parsed Code Mode pragma | Add explicit declaration in both surfaces, preserve owned shutdown and terminal diagnostics, exclude from drain. |
| Polling rejection | `WriteStdinHandler` rejects only top-level empty polls of live auto-observed non-TTY processes when `automatic_completion_active`; hidden Code Mode `wait` refuses; nonempty input/TTY/nested/compatibility work | Keep capability gates; test Service uses same capability, and no rejected operation performs input/observation side effects. |
| Active/idle/final races | #152 and subsequent integration: active request appends, finalization rechecks ready work, revision-coalesced idle wake | Regress with lifecycle enabled; no duplicate wake/result or premature quiescence. |
| Esc / failed auto-start | `session/wakeups.rs`, input queue pause, `owned_completion_wake_tests`, idle integration cases | Pending finite delivery keeps drain nonquiescent; no retry loop. Explicit user recovery drains accepted identity once. |
| Capacity/output limits | 64 completion reservations, 3072 encoded bytes each, 8 new fragments/request; immutable history metadata; acceptance at `response.created` or completed fallback | Reuse limits. Historical proposal's 4096/768-token/256KiB/6144-token values are superseded: hard token bound is separately tracked by #192. |
| Closed owner / shutdown | `shutdown_session_runtime` retires store before terminating processes/cells; publishers and owner pointer are weak | Producer admission must share close fence; callbacks after close cannot resurrect scope. Services need proof of owned termination. |
| Eviction / leases | `residency::is_unloadable` currently rejects completion-store records and durable sleep, NOT all live cells/terminals; Code Mode delegate intentionally retains originating StepContext; queued callbacks are weak | Add a live-registration gate for Task/Cell/Service, TTY and compatibility producers. No transparent migration: pin until resources settle, then allow existing idle eviction. |
| README / RELEASE_NOTES | Already document #79 completion and #95 child drain | Add explicit classification, finite producer drain, service lifetime/shutdown, compatibility behavior and bounded diagnostics. |

Historical issue notes that the Cell producer or polling rejection is absent are
stale. Source, rather than issue checkboxes or historical PR tests, is authority.

## API and invariants

### Declaration and observable behavior

1. Add internal `WorkKind::{Task, Cell, Service}` in the lifecycle module. Add
   model-facing optional `work_kind: "task" | "service"` to resumable
   `exec_command`; omission is Task. Code Mode uses its existing first-line
   `// @exec: {"work_kind":"service"}` pragma; omitted or `"task"` is finite
   Cell. Extend `ParsedExecSource` and pragma validation in
   `code-mode-protocol/src/description.rs`; this is host-side classification and
   does not need a new grpc ExecuteRequest field or remote runtime behavior.
   `Cell` is host-assigned, never a declaration value. Invalid values fail before
   process allocation/provider execution. Keep declaration enum separate from
   producer identity so a Service cell still correlates as `kind=cell`.
2. Carry native kind in `ExecCommandRequest` and `ProcessEntry`, return it in
   yielded command headers and Code Mode's structured command result. Use an
   optional classification field on the shared output type so unrelated legacy
   shell outputs do not need altered headers. Command label remains bounded.
3. A one-shot command cannot be Service: reject declaration before side effects
   rather than quietly accepting a declaration the host cannot honor. Its
   bounded synchronous Task is covered by the admitted turn/startup guard.
4. TTY allocation does not mean Service. Task TTY commands remain finite for
   drain, retain existing interactive stdin and visible polling, and have no new
   automatic text result. This is the existing capability exception, made explicit
   in docs. Native terminal/end event must complete before their ledger hold ends.
5. Native and Cell Services use the existing automatic terminal message when available, marked
   `work_kind=service`; process/cell correlation text otherwise stays compatible
   (`kind=process`, `kind=cell`). Non-TTY Service polls are refused only when the
   existing automatic-completion capability covers that exact live process.
   A Service cell keeps the existing `exec_terminate`/compatibility wait contract.
   Service classification applies to that producer only; a Service cell's
   separately launched native command defaults to Task unless it too explicitly
   declares Service. Do not implicitly detach or reclassify nested producers.
6. A service's running record never causes `owns_assignment` to hold a completed
   assignment in Waiting. Its terminal diagnostic remains retained under the
   originating generation and follows existing exact-owner delivery/root explicit
   recovery rules. If its original assignment has retired, it cannot inject into
   a newer assignment or wake an already-closed task agent. Service terminal
   records, as well as running services, are excluded from drain. They remain
   bounded/recoverable until normal acceptance or owned shutdown. This is a
   deliberate semantic choice for review: terminal reporting is retained, but
   does not promise a new automatic turn after that owner assignment is terminal.
   An owner still running/waiting in that assignment receives its diagnostic by
   the ordinary path. Do not silently choose a fresh assignment for diagnostics.

### Registration and one terminal path

Extend `WorkCoordinator` with bounded producer registrations, not a completion
mailbox. A registration stores opaque ID, owner ThreadId, initiating turn/call,
assignment generation, WorkKind and running/terminal phase; optional native ID
binds after start for Cell. It stores no output. The handle is an Arc-backed
`OwnedWorkRegistration`; its last holder's Drop removes the registration. Reject
before side effects at 64 live/retained registrations per owning session (matching
the existing completion cap), including at most 16 Services. No unbounded
per-session map survives last registration removal. Identifier checks retain the
existing 128-byte metadata contract. These are bounded registration limits;
completion reservations keep their existing independent 64-record admission.

* Admission occurs under the existing WorkCoordinator close lock. A successful
  registration makes the current revision stale before launch. A guarded close
  either loses to admission and observes nonquiescence, or closes first and
  rejects launch. Service admission also rejects after close despite its zero
  finite weight. Acquire the existing runtime start membership during startup,
  and recheck session/tree shutdown before publishing a newly spawned handle.
  A process racing shutdown is terminated before the startup membership releases;
  `terminate_all_processes` must not miss a handle published after its snapshot.
* Never hold store mutex while acquiring lifecycle/wake coordinator locks. Existing
  wake-coordinator -> completion-store order remains. Required global order is
  lifecycle coordinator -> wake coordinator -> completion store. Registration
  removal/publication updates happen after releasing store locks; native callbacks
  and `changed()` cannot recursively acquire the lifecycle lock. Implement via
  small methods/RAII, no lock held across `.await` or process termination.
* Each process/cell native terminal path marks its registration terminal exactly
  once. Initial inline return or failed start releases it after the original tool
  response handles the result. A yielded automatic producer's completion Record
  holds a registration clone until ordinary model acceptance removes that record;
  history append/claim alone does not release drain. A collector/watcher keeps its
  clone until terminal output/event processing finishes, so acceptance cannot
  make drain quiescent before remaining terminal event work is done.
* Nonautomatic producers also register: a TTY exit watcher owns its handle;
  a compatibility Code Mode delegate owns its handle until `cell_closed`. Dropping
  a completion reservation because visible `wait` owns the result must not drop
  the native lifecycle registration prematurely. A Code Mode delegate and its
  automatic record share the same registration, not independent ledger entries.
* **Assignment retention is separate from drain counts.** Add a derived
  `finite_owned_producers` count to each `Assignment`; increment/decrement it
  while registering/releasing the authoritative lifecycle entry, under
  lifecycle -> wake locks. `prepare_turn_end` currently acquires only the wake
  lock and consults `store.owns_assignment`; extend its succeeded predicate to
  `direct_children || finite_owned_producers != 0 || finite_store_records`.
  It reads the scalar already under that lock and NEVER enters WorkCoordinator.
  This is a derived index of the same registrations, not another producer map.
  Services contribute zero to this predicate, including retained diagnostics.
  Failed/interrupted/cancelled assignments retain their existing terminal policy
  and teardown; registration does not convert those dispositions to Waiting.
* A nonautomatic finite registration carries the exact captured generation even
  when launched from an originating cell after its turn yielded. Admission takes
  lifecycle -> wake locks and verifies that the original generation remains
  current and open AND that the retained originating Cell registration authorizes
  that old turn's nested calls. Do not reassign it to the new active turn, and do
  not require its old turn to still be Running. Reject a retired/replaced
  generation before launch. Extend the existing retained-cell dispatch context
  with its registration identity; no model-supplied parent or generation field.
* Last finite release signals the EXISTING wake coordinator and existing
  pending-work scheduler. If a waiting assignment has no automatic payload, keep
  a single derived `finite_terminal_revision`/acknowledged revision on the
  assignment so the existing `PendingWorkCause::OwnedCompletion` eligibility and
  `continue_owned_completion` can admit one empty-input continuation. This is
  a wake hint derived from lifecycle transition, not a result queue; TTY terminal
  output remains in its original process buffers, and visible wait/stdin reads
  keep their existing result/consumption semantics. A new revision during an
  active/final turn uses the same finalization recheck, pause/rejection policy
  and coordinator wake scheduling; acknowledge only when that continuation binds,
  and restore authority if startup aborts. Do not leave a Waiting assignment
  permanently open after its last finite TTY/native hold ends. Compatibility
  Cell providers lacking readiness cannot provide an exit hint before native
  close: their existing explicit `wait`/terminate remains responsible for reading
  and closing that cell; its registration prevents premature final/report/close
  meanwhile. No new hidden watcher may steal that compatibility result.
  The hint is generated ONLY for a successfully started, yielded/nonautomatic
  finite producer whose exact generation is Waiting, or whose terminal transition
  races the final boundary that would otherwise retain it. Admission failures,
  failed startup, inline-only returns, Service releases and automatic-record
  acceptance/drop MUST NOT generate one. Track the producer's successful start
  and yielded/nonautomatic eligibility on that same registration; a generic
  last-Arc-drop hook cannot decide eligibility correctly.
  Widen every current readiness/bind gate to `ready payload OR exact-generation
  unacknowledged finite hint`: `coordinator/owned_work.rs` in
  `continue_owned_completion` and `has_ready_owned_completions`, the
  `tasks/owned_completion.rs::ready_owned_completion` selector, and the final
  `tasks/mod.rs` OwnedCompletion pre-bind readiness recheck. The dispatcher must
  use that same combined predicate; merely requesting a wake is insufficient.
  Bind under the wake lock acknowledges the observed hint revision for that
  assignment as it moves Waiting -> Running; a newer revision remains pending.
  Acknowledgement consumes wake authority only, never a payload: automatic result
  records retain their independent ordinary-request acceptance rule. If another
  active turn already incorporated the terminal transition before finalization,
  acknowledge that observed hint at that boundary rather than creating an extra
  continuation. An aborted/unstarted bind restores only its captured unconsumed
  hint revision under the same generation/turn ownership checks used by
  `release_unstarted_turn`; paused/rejected startup holds it without retries.
  Replaced/terminal generations cannot donate hints to a new assignment.
* `WorkCoordinator::snapshot` sums non-Service registrations into existing
  outstanding/running/pending-notification counts. Running registration contributes
  running finite work; terminal registration awaiting transfer contributes pending
  work. Assignment and producer counts are separate units, so an assignment with
  two tasks can contribute three running units. Document the additive meaning;
  no new app-server wire fields are needed. Quiescence requires all existing
  root/output/agent checks AND zero finite registrations.
* Update/subscribe/revision-guarded close continue to use one coordinator lock.
  Producer changes publish snapshots immediately; the existing wake-forwarder
  handles agent changes. No new polling loop or notification transport is added.
* Completion publication is still `CompletionPublisher::publish`; active
  preparation is still `record_ready`; acceptance is still `accept` at ordinary
  model request acceptance. Cell collection remains elected once under its
  observation lock. No duplicated failure/cancel collector or function-call
  output synthesis. Admission failures/startup failures use the original tool
  response and release guards, rather than a second asynchronous result.

### Shutdown, pause and lifetime

Service exclusion applies solely to drain accounting. `ProcessEntry` and native
shutdown ownership remain. Current teardown is insufficient for remote Services:
`terminate_all_processes` drains entries and calls `terminate`, which spawns an
untracked remote request and discards errors. Native-lane work must replace this
owned shutdown path with awaited `terminate_confirmed` and existing tree failure
reporting; graceful drain close is not complete before that settles.
No new detach operation, process restart persistence, or external service control.
Retiring the completion store prevents late terminal messages after explicit close.

Add a manager-local process-publication fence: close it under the process-store
lock before taking the shutdown snapshot. Track admitted starts with the existing
runtime membership plus a manager startup guard held from before native spawn
through stored publication or confirmed cleanup. A start publishing after the
fence must hand its exact native process to owned async cleanup and await
`terminate_confirmed`; it cannot escape because shutdown previously drained the
map. Shutdown waits for pre-fence starts to finish publication/cleanup, then
confirms every drained process. Use cancellation-safe guarded cleanup (transfer
to one owned spawned task with a teardown membership if the initiating future
drops), not async work in Drop that loses its receipt. All memberships persist
until confirmation and failure recording, and all per-process failures are
recorded before scope shutdown's detailed report can resolve. Reuse
`AgentTreeTeardownGuard`/`record_shutdown_failure`; no bare ignored remote
termination errors or fire-and-forget cleanup. Never await with process-store,
lifecycle or wake locks held. Confirmation failure releases bookkeeping but
returns/reports a failed shutdown, never a successful terminated claim.

Eviction checks the authoritative per-owner `has_live_registrations` under a
brief lifecycle lock, before acquiring wake/store locks and with no lock held
across async session checks. Every registration counts here, including Service
with zero drain weight, TTY, compatibility Cell and retained automatic result.
Registration admission must use the existing residency gate for originating
retained-cell starts so an eviction cannot pass its final recheck and then race
new producer publication; hold the native startup residency pin until publication
or cleanup. In `try_evict_v2_thread`, after taking the exclusive residency gate,
perform the live-registration predicate with the existing final unloadability
check. Existing normal active-turn dispatch already prevents eviction; don't
infer all producers are protected by the completion-store predicate alone.

Esc holds accepted automatic records without starting new turns. A held finite
record remains nonquiescent; existing exec interruption/error path may deliberately
end the drain and then shuts down owned work. Explicit followup recovers once.
Failed wake startup retains records and does not self-retry. Busy/final-boundary
publication relies on existing finalization recheck. Native completion observers
should retain weak Session references rather than indefinitely pin a Session for
Service lifetime; upgrade only to emit an event or make a bounded wake attempt.
Retain immutable turn/model/call metadata needed by events. Code Mode's existing
explicit StepContext callback lease stays until cell close/termination, with
queued dispatch retaining Weak<StepContext>; verify release on every terminal,
failed-start and cancellation path. Do not shorten that lease and break nested
calls after their originating turn.

## Bounded implementation slices and file ownership

Independent plan PASS is mandatory before these production changes. Write one
brief per implementation lane after review; share this reviewed API contract.
Workers are not alone and must preserve other edits. Root owns PRs/merges and
the single final combined build/install. The #237 owner owns TUI state/router;
there is no planned TUI overlap. Root coordinates README/RELEASE_NOTES overlap.

1. Lifecycle/native lane (Sol/high): `agent/control/work_lifecycle{,_tests}.rs`,
   narrow runtime accessors; `agent/control/coordinator.rs`,
   `coordinator/{assignments/lifecycle,owned_work}.rs`,
   `agent/control/{residency,dispatcher}.rs` and focused tests;
   `tasks/owned_completion.rs` and narrow existing startup/finalization hooks;
   `session/async_completion{,_tests}.rs`, `session/handlers.rs` shutdown failure
   accounting; `unified_exec/{mod,process,process_manager,async_watcher}.rs`, native relevant tests;
   `tools/handlers/{unified_exec,unified_exec/exec_command,shell_spec}.rs`,
   `tools/context.rs`. Own WorkKind/registration API, native declaration/output,
   automatic-record hold, finite assignment predicate, no-payload finite terminal
   wake hint, eviction fence and confirmed native shutdown. Keep changes modular;
   request owner integration for shared coordinator assignment methods.
2. Cell/integration lane (Sol/high, dependent on agreed API landing):
   `code-mode-protocol/src/description.rs` and parser tests;
   `tools/code_mode/{execute_handler,mod,delegate,cell_completion}.rs` and tests;
   focused lifecycle admissions, app-server work-state tests, docs. Own Cell handle
   declaration/registration before execute, provider ID binding/native-close release, startup
   and interruption regressions, real host behavior. Do not edit native lane files
   concurrently. Integration owner resolves shared store/lifecycle changes.

If one coherent change exceeds review size limits, split at internal API plus
native producer and then Cell integration; publish/merge only with explicit issue
acceptance accounting. Do not mark #93 complete with only the enum/native slice.

## Required checks and acceptance evidence

Use deterministic barriers/oneshots and native completion events, no timing-only
status polling. Extend existing suites instead of cloning historical race tests.
Test names below are implementation targets; existing names are identified as such.

* `finite_task_registration_prevents_quiescent_close_until_acceptance`: subscribe
  to actual WorkObservation, end root turn/output, hold a native task, prove
  guarded close rejects; finish it, hold request acceptance, prove still blocked;
  accept once and observe quiescence. Parameterize success/nonzero/cancel.
* `cell_registration_prevents_quiescent_close_until_acceptance`: same via actual
  CodeModeService/delegate/readiness collector, include running nested native
  Task and terminal collection failure. One cell/native registration each; no
  phantom double terminal and no lost final-boundary result.
* `service_is_excluded_but_owned_shutdown_terminates_it`: actual long-lived
  process, explicit Service echoed in response, root finite scope drains while
  service lives, guarded close and existing shutdown terminate exact PID/native
  handle. Report exit/cancel diagnostic once while its owner still accepts work.
* `service_does_not_hold_assignment_waiting`: child final with only a service
  reaches final/owned close according to current task/persistent agent policy;
  stale generation receives no new automatic diagnostic turn. Root recovery
  retrieves retained service diagnostics where the root remains open.
  Parameterize native and Cell Service. A `// @exec` Service cell reaches its
  native shutdown/terminate callback during scope close; no substitute observer
  or alternate cell completion transport is permitted.
* `producer_admission_serializes_with_guarded_close`: both race orderings,
  including Service, root without assignment, native process and Cell; rejected
  call has no filesystem side effect. Failed start/cancel before publication
  releases count/capacity; publication-after-shutdown cannot escape teardown.
* `service_capacity_and_failed_start_release_registration`: registration caps,
  automatic reservation cap failure, invalid work_kind and one-shot Service
  rejection all precede side effects and leave no phantom drain work.
  Parser tests cover omitted/task/service/invalid Cell declarations and preserve
  existing yield/output pragma behavior; native request tests cover the same.
* `tty_and_compatibility_cells_keep_native_lifecycle_holds`: TTY Task output/end
  event forwarding and cell with unsupported readiness / visible wait remain
  counted until native close; input and termination remain usable.
  Exercise actual task child finalization/report routing: child with live TTY
  Task stays Waiting and is not auto-closed, TTY exit admits one existing-path
  continuation and then one parent final; child with a compatibility Cell stays
  Waiting until explicit wait/terminate observes/closes it. Run both service
  classifications to prove they do not hold finalization open. A retained Cell
  launching a native Task after its initial turn ends keeps the ORIGINAL
  generation; replacing/closing that generation rejects launch before side effects.
* `remote_service_shutdown_waits_for_confirmation_and_reports_failure`: fake
  remote `ExecProcess::terminate` blocks on an explicit barrier. Scope shutdown
  must stay pending until acknowledgement; success completes afterward. Error
  and cancellation cases settle tracked cleanup and produce a detailed tree
  shutdown failure. Gate a second start between spawn/publication, close scope,
  and prove that handle is confirmed stopped before startup membership/overall
  shutdown completes. Do not substitute local PID termination for this proof.
* Existing `native_completion_is_appended_in_the_active_turn_without_stdin`,
  `idle_owner_receives_native_completion` cases (idle/final/paused/rejected/closed),
  `history_append_is_retained_until_model_acceptance`,
  `cell_completion_publishes_text_tail_once_on_readiness`,
  `cell_completion_interrupt_publishes_cancelled_tail_once`, model-read release,
  fallback watcher cancellation and bound tests: run with new hold assertions.
  Add exact no-extra-wake assertions after automatic payload acceptance, failed
  admission/startup, inline-only completion and Service release. Exercise a
  nonautomatic native last-release/final-boundary race through ALL scheduler
  selector/readiness/bind gates; paused and rejected/unstarted binds preserve one
  hint, successful bind consumes once, and a new generation rejects stale hint.
  Coalesce concurrent terminal hints into the already-bound/running continuation
  when it observed those transitions; never consume automatic payloads via hint
  acknowledgement.
* Existing polling suite: top-level empty rejection, input/TTY/nested exceptions;
  add Service and compatibility gates. Code Mode retired wait handler still
  rejects at dispatch; termination remains available.
* `live_producer_and_retained_result_prevent_eviction_then_release`: check native
  task/service/cell, automatic record retained after history append and released
  after acceptance, then existing idle residency eviction/reload. Intentional
  close retires results and late readiness cannot start a turn.
  Test actual exclusive residency-gate eviction against live Service (zero drain),
  TTY and compatibility Cell separately, including retained-cell startup racing
  eviction; after terminal close/acceptance, the same owner becomes evictable.
* Representative combined host acceptance: an in-process app-server execution
  observing real work-state notifications with a native finite process AND a
  Code Mode cell whose terminal native result is gated past the initial final
  answer; observe exactly one followup delivery each and quiescent shutdown. A
  separate live Service proves drain exclusion plus teardown. This must use the
  actual observer/registration/provider routes, not fabricated snapshot counts.

Focused cargo gates (isolated `codex-worktree env` target): first build helper
binaries `codex-code-mode-host` and `test_stdio_server`, then selected codex-core
unit/integration tests for changed modules, codex-exec work_lifecycle tests,
app-server work-state test(s). Use one supervised wait per long command. Run
`cargo fmt`/repository formatting and `git diff --check`. Update tool schema
snapshots affected by `work_kind`/output classification; run relevant schema
snapshot test(s). No config schema generation is necessary unless config types
change; no public app-server field/schema change is planned. CI owns full suite;
user authorized merge without CI wait. Record exact reviewed/tested head and
actual runtime receipt separately. Final build/install belongs to root only,
after both #93 and #237 merge.

Installed-source preservation is coordinated by root. Merged main `f4de69bc0c`
now contains queue fixes `4cc6293763` and `98181e1127` directly, plus #237; this
branch merged that base and requires no queue overlay. User steering allowed
#237 to ship independently. Future #93 build/install work is NAS-only; no new
MBP build, rebuild or retry is allowed for this session. Root owns publication,
merge and installation; this workstream does not mutate installed state.

## Review questions to resolve before production implementation

1. Confirm Service diagnostic policy for a terminal/replaced originating
   assignment: retained exact-generation record and explicit-root recovery,
   never resurrection or injection into a new assignment. This follows current
   ownership and makes Service exclusion real; widening delivery authority would
   be a separate explicit contract change.
2. Confirm lifecycle guard sharing gives terminal-output and model-acceptance
   holds without duplicate results, including unsupported-readiness cells and
   startup-vs-shutdown. Require concrete lock/ownership review, not enum-only PASS.
3. Confirm existing snapshot counters can add producer units without new wire
   fields, and 64 total/16 Service admission limits are acceptable. Do not inherit
   superseded hard-token claims from the historical shared-design proposal.

### Delivery settlement contract

Automatic delivery becomes eligible only after native/output terminal holds settle. Native watchers finish measurements and terminal event forwarding, mark terminal and drop their shared registration, then publish through the existing record, which still retains the registration until ordinary model acceptance. This ordering avoids retaining an assignment after its accepted payload has already been consumed and does not create automatic-drop hints or extra model turns.

For Cells, the existing registered reservation is the delivery fence. The collector can store its bounded payload in that same record while native close is pending; no second queue stores the result. Delegate ownership and dispatch cleanup settle before `CellCompletion::closed` releases the fence. Collector and fallback-watcher read failures terminate the exact cell and bound termination plus close confirmation to 30 seconds. Failure or timeout retains the live dispatch gate and fenced diagnostic for a later interrupt or owned shutdown; a delayed callback releases that one record. Model wait errors preserve this cleanup authority.

The initial `dev-small` dispatcher stack overflow also reproduces on unmodified merged main `f4de69bc0c` and the exact baseline test passes with a 16 MiB test stack; focused validation uses that stack. This is a baseline-supported test execution setting. The failure-cleanup fixture must establish an actual admitted Cell before marking it ready, since readiness never creates a missing native dispatch gate.

A yielded automatic Cell remains visible in pending work while its existing
reservation fences native close. The per-record visibility marker is separate
from deliverability: a stored terminal payload remains `ready=false` and cannot
be claimed until that reservation releases. Inline results/errors stay absent
from pending work. The sole record remains the payload owner.

### Focused validation progress

* Prior affected units: 390 passed across agent control, store/acceptance, native
  processes, Cell bridge, handlers, schema and output. After the final Cell
  visibility refinement, store 24 and Cell bridge 46 passed again (job-xo3zg5jo).
* The same job passed all five actual child cases: TTY Task, compatibility Cell,
  native Service, Cell Service and retained Cell launching a native Task after
  its original final answer, including replacement-generation isolation.
* Existing Cell completion integration: 29 passed. Native completion integration:
  30/31 passed; late-steering authority is under source investigation. Root
  observer/provider acceptance: 6/7 passed in that run; the remaining assertion
  used a stale quiescent revision, so its guarded-close helper now retries only
  a returned still-quiescent revision. Native/Event automatic-delivery settlement
  and real Cell cleanup regressions passed the focused unit gates.
* Exec lifecycle, app-server stdio and model schema snapshot gates remain in
  progress. Final qualification requires their terminal receipts, corrected
  focused acceptance, full formatting and the independent code-review verdict.
