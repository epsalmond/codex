# NAS navigation acceptance evidence

This is the reproducible runner and evidence ledger for
[`nas-lsp-broker.md`](nas-lsp-broker.md). The initial harness check on
2026-10-22 establishes the runner only. Real broker/server qualification is
pending integration; no service is installed or enabled by this work.

## Reproduction

Build the focused `codex-nas-lsp` binary and the
`codex-semantic-navigation` example `navigation-driver` from the integrated
commit using the worktree's `codex-worktree env` target directory. Preserve the
build receipt and commit so the source provenance can be matched to the binary
SHA-256 values in the acceptance receipt. The example invokes the actual installed
native `ToolExecutor`; this is example-executor + broker + server integration,
not evidence of a model-driven interactive session. Core registration, host
verdict derivation, and Code Mode adapter tests are separate evidence.

Run the focused runner checks:

```bash
python3 scripts/nas-lsp-acceptance.py self-test
```

Launch the real run **once** through the event-driven command supervisor, with
an unused unit name and output directory. Substitute the absolute paths from
the focused build receipt for `BROKER` and `DRIVER`:

```bash
agent-job -- systemd-run --user --scope \
  --unit=codex-lsp-acceptance-UNIQUE \
  -p MemoryMax=1G -p TasksMax=64 \
  python3 scripts/nas-lsp-acceptance.py exercise \
    --broker BROKER --driver DRIVER \
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

## Outstanding acceptance

No real-server result has been recorded at this preparation stage. Keep #164
open until the runtime receipt and supporting focused-test receipts establish
all gates, with exact source/build provenance. The runner lists additional
required evidence explicitly even if its real corpus checks pass:

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
* Independent review and matching source/binary build provenance.

No rust-analyzer is installed or qualified. Only the specified local Pyrefly
adapter is eligible for the real run. No remote/restricted broker access,
deployment, active-service changes, or model-session end-to-end behavior is
implied by this evidence.
