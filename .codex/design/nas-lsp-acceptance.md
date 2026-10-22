# NAS navigation acceptance evidence

This is the reproducible runner and evidence ledger for
[`nas-lsp-broker.md`](nas-lsp-broker.md). The initial harness check on
2026-10-22 establishes the runner only. The first real run on 2026-10-22 failed
reference correctness; the corrected run passed all 556 runtime checks. Both
results and the supporting deterministic tests are recorded below. No service
is installed or enabled by this work.

## Reproduction

Build the focused `codex-nas-lsp` binary and the
`codex-semantic-navigation` example `navigation-driver` from the integrated
commit using the worktree's `codex-worktree env` target directory. Preserve the
build receipt and commit so the source provenance can be matched to the binary
SHA-256 values in the acceptance receipt. Supply `--broker-build-head`,
`--driver-build-head`, and repeatable `--build-receipt` paths to preserve those
operator-provided build references separately from `source_head`, which identifies
the harness checkout. The receipt also hashes the executed harness itself, so
uncommitted harness changes do not disappear behind a commit identifier.
The example invokes the actual installed
native `ToolExecutor`; this is example-executor + broker + server integration,
not evidence of a model-driven interactive session. Core registration, host
verdict derivation, and Code Mode adapter tests are separate evidence.

Run the focused runner checks:

```bash
python3 scripts/nas-lsp-acceptance.py self-test
```

Launch the real run **once** through the event-driven command supervisor, with
an unused unit name and output directory. A transient user service owns the
isolated bounded cgroup and exits when the supervisor finishes. This host denied
moving the agent supervisor into a transient `--scope`, before Python started;
starting a transient service from the user manager worked. Substitute the absolute
paths and source commits from the focused build receipts:

```bash
agent-job -- systemd-run --user --wait --pipe --collect \
  --unit=codex-lsp-acceptance-UNIQUE \
  --working-directory=/home/eric/worktrees/review/codex-lsp-runtime \
  -p MemoryMax=1G -p TasksMax=64 \
  python3 scripts/nas-lsp-acceptance.py exercise \
    --broker BROKER --driver DRIVER \
    --broker-build-head BROKER_HEAD --driver-build-head DRIVER_HEAD \
    --build-receipt BUILD_LOG \
    --server /home/eric/arcade/scripts/codeintel/toolchain/.venv/bin/pyrefly \
    --source-root /home/eric/worktrees/review/codex-nas-lsp/scripts \
    --output-dir /tmp/codex-164/acceptance-UNIQUE
```

The runner creates a new private Git repository containing the fixed corpus
and a linked second worktree sharing the same common Git directory, plus a uniquely named private
registry and socket directly under `codex-uds`' reserved
`/tmp/codex-daemon-UID/` directory. Existing paths and modes are preserved. The
registry selects the absolute canonical server executable and fixed arguments
`lsp --indexing-mode lazy-blocking --threads 1`; there is no model-supplied
command. The runner supervises one broker from startup through SIGTERM shutdown
and writes `setup.json`, `broker.log`, and `receipt.json`. Failed runs preserve
their evidence. Inspect a terminal failure receipt before deciding whether a
corrected rerun is justified.

The current runner also registers a small, independent `server-input-probe`
root. Its `main.py` reads `make().value` through `hidden/bridge.py`, with
`hidden` explicitly configured in `exclude_dirs`. The initial binding must
resolve to `a.py`; after saving only the excluded bridge with a different import,
the next binding must resolve to `b.py`, without restarting the broker. Both
Direct and Code Mode must return exact AST-derived property declaration ranges.
This is a separate regression for the reviewer's reproduced stale-`ok` bug; it
does not alter the fixed corpus, its oracle, or the baseline byte comparison.
Its files remain under the private output directory and its latency and process
membership checks remain in the receipt.

`prepare` can alternatively create the roots before an operator-owned broker
launch. `run --setup SETUP --driver DRIVER --broker-pid PID --server SERVER
--output RECEIPT` observes that existing broker without managing its lifecycle.
The broker must begin with no workers for the cold measurements to qualify.

The optional `--source-root` registers the existing Codex `scripts/` directory
as a third exact root. After corpus testing, the runner queries the actual
`codex_package/layout.py` annotations for `TargetSpec` and `PackageInputs` and
requires their exact class-name definitions in `codex_package/targets.py`, in
Direct and Code Mode. Coordinates are derived independently from saved source
ASTs; before/after source hashes must match. This is additional real source
functionality evidence, outside the fixture byte/latency comparison.

Registering the entire Codex checkout is outside the current bounded-inventory
support: the integration checkout was reported as about 9,585 files / 96 MB,
above the configured 2,000-file / 32 MiB limits. That prior inventory estimate
must not be confused with a measured successful whole-repository query. The
existing scripts subtree fits the intended small-project path; the real run
must still verify it rather than raising limits to force success.

## Measurements and fixed gates

The runner imports the checked-in independent corpus oracle. Both roots are
actual linked worktrees of a newly created throwaway repository; no existing
repository is modified. Their shared common directory and initial fixture
commit are recorded, so a worker incorrectly keyed by Git common directory
cannot pass. It queries all
twelve audited tasks in root A at base, changed, and deleted generations, and
root B at every generation. Root B has one comment line prepended to
`pkg/primary.py`, so definition and declaration ranges differ between the two
roots; identical-looking root contents cannot hide an incorrect worker key.
Root B's first request is the empty-answer probe against a cold worker.

The receipt preserves each exact native response-content string and measures
its UTF-8 bytes. Driver envelopes are excluded. When supplied, the driver's
`serialized_bytes` is cross-checked and its `effective_response_budget` is
enforced. That budget comes from the production `response_byte_budget` seam:
Direct applies the existing host allowance; Code Mode keeps its own 16,384-byte
cap rather than using the host's truncation policy. Tiny host budgets therefore
exercise Direct fallbacks; Code Mode's cap needs its dedicated oversized-result
tests as well. `ceil(bytes/4)` is explicitly an approximate token estimate.

All returned ranges must exactly match the independent oracle, including
duplicates being rejected rather than hidden. A successful empty result must
have `status: ok`. Explicit Stale/Indexing responses can be followed by up to two
new requests; each intermediate response remains in the receipt. An incorrect
Ok response always fails. Worker refresh latency is measured for each request;
the conservative adapter should normally finish its generation replacement in
the first request.

Thresholds are copied unchanged from the design into each setup:

| Measurement | Required gate |
| --- | --- |
| Audited sets and worktree separation | 100% exact |
| Cold and refreshed requests | <=10,000 ms |
| Warm unchanged requests | nearest-rank p95 <=1,000 ms over 30 requests |
| Cancellation return | <=500 ms, with explicit Cancelled |
| Shared cancellation survivor | exact successful answer and same worker PID/start time |
| Response content | <=16,384 bytes, <=100 locations, and <=effective host/tool budget |
| Base-task aggregate navigation bytes | <=60% of the equivalent rg-plus-read baseline |
| Aggregate scope ceiling | finite `memory.max` <=1 GiB and `pids.max` <=64 |
| Broker shutdown | <=5 seconds, successful exit, no observed Pyrefly survivors |

Wall latency includes starting the small native driver process, conservatively
counting that overhead. The rg baseline runs real search, complete candidate
file reads, and fixture-specific AST disambiguation; it has warm OS caches and
is not labeled a cold/warm server comparison. Per-task response bytes and
baseline output remain available in the receipt; the aggregate byte comparison
uses the base twelve tasks only and does not pad baseline output.

At multiple phases, one `/proc` snapshot enumerates broker descendants and
records executable, command, PID/start time and cgroup membership. Every observed
descendant must occupy the broker's bounded cgroup; observed Pyrefly workers are
identified by canonical executable, not process-name matching. The receipt
records actual `memory.max`, `pids.max`, and available `memory.peak`. Sampled
process data is not claimed as a hard memory ceiling.

## Recorded checks

Initial harness focused check: `python3 scripts/nas-lsp-acceptance.py self-test`
passed 5 tests on 2026-10-22. These checks cover the independent 36-case oracle,
worktree-specific range transformation, duplicate/range preservation, exclusive
root preparation, nearest-rank p95, and exact emitted bytes including zero-byte
output. They use no real broker or language server.

Dependencies at harness preparation: corpus source commit
`3bd39aa86e3cb2a49dcc46f17bcde6824512fd82`, reviewed protocol/scaffold source
commit `a393b49ff06c719c949c7a07daa5683c89855511`, original source baseline
`b1473a8df9`. The integrated runtime receipt must record its own actual tested
HEAD and binary hashes; these preparation commits are not runtime qualification.

### First real run: failed reference completeness

On 2026-10-22 at 12:38 PDT, isolated transient user service
`codex-lsp-acceptance-runtime-20261022b.service` completed the full harness in
about 5.4 seconds. Native supervisor job `job-6s_xegz8` exited 1. The preserved
receipt is `/tmp/codex-164/acceptance-runtime-20261022b/receipt.json`; its setup
and broker log occupy the same directory. The earlier `--scope` launch,
`job-rvy2oiem`, failed before Python started because systemd denied migrating
the caller's process. Its job receipt and the named unit journal preserve that
environment failure separately.

The harness checkout was `51f693d400de36b633e7a0c35a782474abd3e640`.
The reused binaries came from an earlier focused build; the integrator reported
only test changes between that build and this harness checkout. The build log is
`/home/eric/.local/state/wait-for-build/build-mw_5j183/build.log`; the receipt
does **not** establish that the binaries were rebuilt at the harness HEAD.

| Executed artifact | SHA-256 |
| --- | --- |
| `codex-nas-lsp` | `6cc272bca85f5ec6107d58979e9c789cf2776661a36ad5fbe6cf1925422d1624` |
| `examples/navigation-driver` | `cca99b093268c2b86df05ff6d093d0f739b4991afe90395c1d0dd4404eb1a0b4` |
| Pyrefly 1.2.0 | `8ff3120d48a68586cf061e509073d247fc76ee17b506d4d8bd89a4ab0b407695` |

The run passed 494 of 534 checks. All 40 failures were exact reference sets.
For example, references at the `primary.py` declaration returned that declaration
alone, while the oracle required eight locations. Queries at consumer uses
returned additional references but omitted the never-opened `transient.py`.
The worker opened only the queried document; Pyrefly's lazy-blocking request
barrier did not establish a complete reverse-dependency index. Those incomplete
answers carried `status: ok`. The correction also declares client workspace-folder
support, which Pyrefly 1.2.0 needs for complete workspace references; admitting
all documents alone was insufficient. The oracle remained unchanged.

The following measurements are diagnostic and **do not qualify the experiment**:
warm p95 was 10.243 ms over 30 requests; base navigation emitted 3,148 bytes versus
18,162 bytes from rg plus reads (17.33%). Missing references reduced output, so
the byte ratio cannot establish the required benefit. Cold/refreshed latency,
definition and Unicode coordinates, worktree-specific ranges, saved source
definitions, source smoke, Direct/Code Mode budgets, remote/restricted rejection,
explicit cancellation with the same surviving worker identities, and observed
cgroup membership passed their individual gates. The cancellation response was
actually `cancelled`, rather than a completed request labeled cancellation.

The observed cgroup reported `memory.max=1073741824`, `pids.max=64`, and a final
`memory.peak=192823296` bytes; every observed descendant remained in that cgroup,
with at most two observed Pyrefly workers. Broker shutdown took 15.793 ms,
exited zero, and left no observed worker survivors. These observations do not
replace deterministic pressure, saturation, eviction, or indexing tests.

### Corrected real run: passed

On 2026-10-22 at 13:11 PDT, the justified rerun completed with **556/556
checks passed**, native job `job-6o4dlb9n` exit 0, and transient service
`codex-lsp-acceptance-runtime-20261022c.service` success. The unmodified runtime
receipt is `/tmp/codex-164/acceptance-runtime-20261022c/receipt.json`.
`build-provenance.json` alongside it supplements build references learned after
terminal completion and hashes that original receipt rather than rewriting it.

The broker was built at `6f571a11fa407198d6f6cd08981b3e562490e824`, from
`/home/eric/.local/state/wait-for-build/build-0sputu8k/build.log`, and executed
with SHA-256 `2e48d0cd2cc6b0e79f02c80dd16c65635eedbba421d0219f698997f675bf630d`.
The integration source was `333224c39612047924b2da30868855b698ab6be9`,
which adds only a core adapter test after that production build. The driver was
reused unchanged from the earlier `d4e48463fa1c6c586bdbb323ab550fe742b1e879`
build with the same hash in the failed-run table; intervening production changes
affected the NAS broker/worker, not the native driver/executor. Pyrefly remained
the same pinned 1.2.0 artifact.

The harness checkout was still `51f693d400de36b633e7a0c35a782474abd3e640`
with its explicit runtime-evidence edits. The exact executed script hash was
`9006c8600dc8020c3ad1c32d9f05cde9325b770499f7297a35bd0a6f06ebefc3`;
this identifies its content independently of the later commit that saves it.

| Measurement | Corrected result |
| --- | --- |
| Exact audited location comparisons | 106/106, including all 12 tasks in A/B at base, changed and deleted generations, 30 warm queries, cold empty answer, survivor and both tool paths |
| Cold definition / cold empty answer | 159.601 ms / 153.759 ms |
| Warm nearest-rank p95, 30 queries | 15.314 ms |
| Slowest changed/deleted A query | 134.128 ms |
| Base navigation / rg-plus-read bytes | 4,107 / 18,162 (22.613%, required <=60%) |
| Approximate per-response token estimates, summed base tasks | 1,032 / 4,544 (`ceil(bytes/4)`, not model token measurements) |
| Largest emitted response in this corpus | 754 bytes; every response fit its effective Direct/Code Mode budget |
| Real cancellation | `cancelled` in 6.302 ms; exact survivor answer and unchanged worker PID/start identities |
| Excluded-import regression | Exact `a.py` then `b.py` property declarations in Direct and Code Mode; first post-save answer 127.693 ms |
| Resource bounds | `memory.max=1073741824`, `pids.max=64`, final `memory.peak=267063296` bytes; every observed descendant in the same bounded cgroup, <=2 observed workers |
| Clean shutdown | 31.465 ms, exit 0, no observed worker survivors |

All corpus correctness checks include Unicode positions, repeated same-line
references, same-named symbols, unopened reverse dependents, changed/deleted
saved files, and linked worktrees with different ranges. Both real scripts
bindings resolved exactly in Direct and Code Mode and their saved source hashes
remained unchanged. Remote/restricted verdicts and unknown roots, traversal, and
missing sockets failed closed. No fixture oracle or threshold changed.

### Supporting deterministic evidence

The NAS tests below passed in the corrected focused test run at
`6f571a11fa407198d6f6cd08981b3e562490e824`: 20 library tests and 10 worker
tests, including the pinned real-server tests, from
`/home/eric/.local/state/wait-for-build/build-njg43kvv/build.log`.
They are separate from the native runtime job. Tool/core tests have their own
receipts below.

Additional preserved focused receipts establish the shared protocol's three
tests (`/home/eric/.local/state/wait-for-build/build-fsyco2pn/build.log`),
all six semantic-navigation tests including escaped oversized output and
exceptional transport fitting
(`/home/eric/.local/state/wait-for-build/build-b0vqulsw/build.log`, native job
`job-thp69a3o`), and generic extension-adapter cancellation propagation
(`/home/eric/.local/state/wait-for-build/build-f3r2pjp1/build.log`). Those
production components were unchanged by the later broker/worker corrections.

The production extension adapter test
`installed_semantic_navigation_through_adapter_preserves_authority_and_source_budget`
passed 1/1 in
`/home/eric/.local/state/wait-for-build/build-v423lr4b/build.log` (native job
`job-hb7fgjsz`). It invokes the actual installed executor through the core
extension adapter in Direct and Code Mode, rather than relying only on the
standalone example driver. Independent delta review at integration
`333224c39612047924b2da30868855b698ab6be9` found no blockers and separately
exercised the corrected retirement/admission, real corpus and excluded-input
regressions. This does not establish a model-driven session or V8 Code Mode
evaluation end to end.

| Requirement | Focused test or remaining gap |
| --- | --- |
| Shared cancellation, queue/pool full, lease drain, idle eviction and worker reap | `pool::tests::cancelled_client_returns_while_lease_drains_and_queued_query_reuses_pid` |
| Slow workspace isolation and bounded pool shutdown | `pool::tests::slow_startup_isolated_by_workspace_and_shutdown_is_bounded` |
| Cancelled replacement/maintenance keeps retiring processes counted until reap | `pool::tests::cancelled_replacement_and_maintenance_keep_retiring_process_counted` |
| Worker timeout, deadline slack and failed refresh invalidation | `tests/worker.rs::timeout_and_failed_refresh_cannot_reuse_stale_state` |
| Oversized LSP frame, external result, in-query saved change, rejected server edits/commands | `tests/worker.rs::protocol_errors_external_results_stale_and_request_rejection` |
| Framing limits, malformed broker JSON, partial frame deadline, response write deadline | `lsp::tests::framing_is_bounded_before_allocation`, `broker::tests::malformed_json_returns_bounded_status_and_partial_frame_times_out`, `broker::tests::response_write_deadline_and_tiny_wire_budgets` |
| Missing/replaced registered executable and roots | `broker::tests::absent_or_replaced_registered_executable_fails_closed`, `broker::tests::rejects_unknown_root_traversal_and_replaced_root` |
| Existing registered executable exits before initialize | `tests/worker.rs::executable_exiting_before_initialize_is_compact_unavailable_and_reaped` |
| Private registry, unknown command fields, symlink escape, resource prerequisites | `config::tests::registry_schema_load_is_private_immutable_and_rejects_unknown_fields`, `config::tests::source_policy_rejects_absolute_traversal_and_symlink`, `resource::tests::rejects_unbounded_or_excessive_kernel_limits` |
| Serialized oversized escaped paths in both tool paths, unavailable/timeout/cancel transport | `semantic-navigation/src/tests.rs::complete_serialization_and_count_fit_for_direct_and_code_mode`, `oversized_malformed_replies_timeout_and_cancellation_are_bounded` |
| Incomplete indexing versus a successful answer | `tests/worker.rs::active_progress_and_server_not_ready_discard_results_until_ready` covers active progress and server not-ready errors, followed by nonempty `ok` on the same worker. A separate real cold query establishes empty `ok`; no combined indexing-to-empty test is claimed. |

The corrected real run repeated all corpus scenarios and the excluded-import
regression. The independent review probe that motivated the latter is preserved at
`/tmp/codex-164/review-exclusion-probe.py`; that probe observed an old `a.py`
answer after changing the bridge to `b.py`, then a correct `b.py` answer only
after broker restart. The final runner requires the correct saved-file answer
while the broker remains alive.

## Qualification boundaries

The runtime receipt intentionally lists additional required evidence separately
from its real corpus checks. The focused tests above supply deterministic
coverage for states the normal real corpus does not reliably force:

* Deterministic indexing, timeout, unavailable executable, malformed frames,
  oversized server messages, and escaped oversized location results.
* Queue/pool saturation, idle eviction, per-worker shutdown, and request deadline
  slack; the normal corpus does not reliably force these states.
* Registered commands only, symlink escape, and resource-prerequisite rejection
  outside a dedicated bounded scope.
* Native direct and Code Mode budget tests, including production adapter fitting
  of oversized and exceptional output, and core environment/registration tests.
* A real cancellation response can race completion on this small corpus. A
  completed query is reported as a failed cancellation-status check, never as
  proof of cancellation isolation. Deterministic shared-worker cancellation
  coverage must supplement that receipt.
* Independent review is tracked by the integrator; runtime source/build
  provenance is preserved above.

No rust-analyzer is installed or qualified. Only the specified local Pyrefly
adapter is eligible for the real run. No remote/restricted broker access,
deployment, active-service changes, or model-session end-to-end behavior is
implied by this evidence.
