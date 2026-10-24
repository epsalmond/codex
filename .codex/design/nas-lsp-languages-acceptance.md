# TypeScript and Rust runtime qualification

The [adapter contract](nas-lsp-languages.md) defines the supported profiles.
`scripts/nas-lsp-languages-acceptance.py` independently drives the existing native
`navigation-driver` ToolExecutor, a persistent private broker, and the actual
operator-pinned language server. Direct and Code Mode select the actual executor
source/budget path. This does not establish a model-driven session or V8 Code
Mode evaluation end to end.

No service is installed. Each run owns one transient **user service**, private
roots, an exclusive receipt directory, and a unique private registry/socket.
The service enforces `MemoryMax=1G` and `TasksMax=64`; a scope is unsuitable on
this host because migrating the agent process into a scope was denied in the
original Python experiment. Existing active services and worktrees are untouched.

## Fixed gates

These gates were committed to the harness before any TS/Rust broker measurement:

| Gate | TypeScript | Rust |
| --- | --- | --- |
| Exact complete location sets | 100% | 100% |
| Cold, refreshed and cold-empty latency | <=10,000 ms | <=10,000 ms |
| Warm nearest-rank p95, 30 queries | <=1,000 ms | <=1,000 ms |
| Returned content | <=16,384 UTF-8 bytes and <=100 locations | same |
| Effective Direct/Code Mode budget | actual native driver budget | same |
| One base variant's navigation / rg+read bytes | <=60% | <=60% |
| Explicit cancellation return | <=500 ms | <=500 ms |
| Shared cancellation survivor | exact answer, same PID/start identity | same |
| Aggregate kernel ceiling | <=1 GiB, <=64 tasks | same |
| Broker shutdown | <=5 seconds, exit zero, no observed workers | same |

Each tool invocation retains its existing ten-second outer deadline. Only an
explicit `indexing` or `stale` response permits at most two immediate new
requests; intermediate responses remain in the receipt. The latency gate counts
all attempts. An incorrect `ok`, an unavailable server, or a timeout never gets
a retry. No sleeps establish index readiness. Startup socket supervision alone
uses bounded backoff. Wall times include native driver startup. Byte comparisons
use exactly one base variant in both numerator and denominator; the actual rg
search, candidate-file reads, and independent fixture source resolution are
preserved. `ceil(bytes/4)` estimates are not model token measurements.

## Reproduction

Build the broker and `navigation-driver` at the integration head, preserving the
build receipts and the actual source commit for each artifact. The pinned task
manifest is `/tmp/codex-nav-languages/toolchains.json`: TSLS 6.0.2, TypeScript
6.0.3, canonical Node 24.21.0, rust-analyzer 2026-10-19, canonical rustc 1.98.0.
The runner checks executable/entry/tsserver/rustc hashes before and after the run.
The manifest's installed-package and rust-src manifests supply wider provenance;
this restricted Rust profile does not load a sysroot.

Run once per language, using a new unit name and output directory each time:

```sh
agent-job -- systemd-run --user --wait --pipe --collect \
  --unit=codex-nav-typescript-UNIQUE \
  --property=MemoryMax=1G --property=TasksMax=64 \
  /usr/bin/python3 /ABSOLUTE/WORKTREE/scripts/nas-lsp-languages-acceptance.py exercise \
  --language typescript \
  --broker /ABSOLUTE/TARGET/debug/codex-nas-lsp \
  --driver /ABSOLUTE/TARGET/debug/examples/navigation-driver \
  --toolchains /tmp/codex-nav-languages/toolchains.json \
  --broker-build-head ACTUAL_BROKER_COMMIT \
  --driver-build-head ACTUAL_DRIVER_COMMIT \
  --build-receipt /ABSOLUTE/BUILD/LOG \
  --output-dir /tmp/codex-nav-languages/runtime-typescript-UNIQUE
```

For Rust, change language/unit/output and add
`--source-root /home/eric/git/wezterm/base91` for the existing-source smoke.
`strace -D -f` observes the actual broker as the runner's direct child; executable
registration is unchanged. The exec trace must contain only the broker, pinned
Node or RA, and fixed rustc introspection for Rust. No Cargo, rustup, npm,
repository wrappers/build scripts/plugins, or unexpected executable is allowed.
The trace is independent process observation, separate from sentinel absence.

Run the original `scripts/nas-lsp-acceptance.py exercise` unchanged against the
same assembled broker and driver, with pinned Pyrefly, in its own bounded user
service. Its [556-check acceptance](nas-lsp-acceptance.md) is a separate regression
gate. Shared/adaptor focused tests supplement runtime evidence for forced
indexing, saturation, malformed frames, oversized escaped output and pressure.

## Corpus and boundary evidence

The independent corpora originate at
`f469a8eb1b58b0d9de875819b5a2076314aa03f6`; they are imported without changes.
Each has 14 ordered tasks and source-audited definition/reference sets across
base, changed, and deleted scenarios. Cold reference queries run at declarations
before consumer queries. All saved documents are admitted by the broker, so
complete reverse dependencies include never-queried helper/transient users.
The two actual linked worktrees share a Git common directory but B's sources are
shifted two lines; after each A mutation B must continue returning its untouched
base ranges. A separate fresh root establishes a genuinely empty cold answer.
Repeated uses, same names, astral Unicode columns, source edits/deletions, exact
Direct/Code Mode responses and tiny budget fallbacks remain audited.

TypeScript additionally changes plugin config, package metadata and an absent
ancestor `node_modules` lookup on a warm generation, then restores supported
inputs and requires exact answers. A separate local alias fixture changes only
tsconfig paths and requires the definition to switch from left to right in both
sources. Fresh negative roots exercise the checked-in JSDoc `@import`, triple
slash path, nested plugins, inherited config escape recipes, plus JSDoc
`import()` type and missing root config/package boundaries.

Rust fresh roots exercise orphan queries, path/include/env/cfg_attr directives,
a macro-wrapped raw-identifier include, missing Cargo model and proc-macro model
payload. An unsafe external graph created on a warm root must fail, then deleting
it must restore the exact saved Cargo graph answer. A separate supported fixture
has hostile `.cargo/config.toml`, a rust-toolchain override and build.rs; the
local graph must still answer exactly and the private execution sentinel must
remain absent. Querying build.rs itself must fail. The existing base91 smoke
requires exact navigation from an actual `ENCTAB` use to its local declaration,
and verifies the saved source hash stayed unchanged.

These checks qualify bounded saved-file projects only. Rust has no general
dependency/std/generated/proc-macro support here; base91's local binding does not
prove its `std::io` semantics. No large TypeScript monorepo, installed dependency
tree, whole Codex Cargo workspace or mutable external input is qualified.
Snapshot/file/entry bounds remain unchanged.

## Recorded results

Harness self-test: three tests pass, including both independent 112-query source
oracle audits, declaration-first ordering, shifted ranges and duplicate
preservation. `strace -D` process ownership was checked independently with
`/usr/bin/true`; that check alone is observer-mechanics evidence. The integrated
runtime results below supersede preparation status. Rust negative recipes are
not yet passed.

### First integrated TypeScript build and Python regression

The successful clean build used source
`75e237f95f9755f206de1f58b24c5df7326eae83`, with native build receipt
`/home/eric/.local/state/agent-jobs/job-40yye71a/receipt.json` and build log
`/home/eric/.local/state/wait-for-build/build-j0ad10q0/build.log`. Private copies
in `/tmp/codex-nav-languages/artifacts-75e237f95f/` preserve the broker SHA-256
`4908b73f21d1f95259f861cf0f844b762ab673c1e8cad48b38104794a37225d5`
and native driver SHA-256
`a314cc5c89934bec218517929595c09d41348dd786914f2db400447117a8f7fc`.

On 2026-10-24, the unchanged Python harness passed **556/556**, including its
actual-source smoke and excluded readable-import invalidation, against these
artifacts. Warm p95 was 11.465 ms; navigation returned 4,107 bytes versus 18,162
recorded rg/read bytes (22.613%). Evidence:
`/tmp/codex-nav-languages/runtime-python-75e237f-20261024-a/receipt.json`,
native supervisor `/home/eric/.local/state/agent-jobs/job-d8pomh_z/receipt.json`.
The TypeScript run used harness `11e1821` and its own separate transient service.

### TypeScript first runtime: one persistent-stale boundary failure

The first TypeScript runtime finished **747/748**. Every exact location query,
including complete declaration-first references before consumers were queried,
passed. Both linked worktrees, changed/deleted generations, new cold empty root,
config alias changes, Direct/Code Mode paths, cancellation/surviving worker
identities, byte budgets, all other negative cases, pinned pre/post artifact
hashes, independent executable trace and bounded shutdown passed. Warm p95 was
486.578 ms; navigation returned 4,500 bytes versus 21,452 baseline bytes
(20.977%). Observed cgroup peak was 308,989,952 bytes; two TSLS workers and their
two tsserver Node helpers remained in the bounded cgroup.

The sole failure was `config/ancestor-node-modules-created`. Adding the previously
absent ancestor directory to a warm project returned
`{"locations":[],"reason":"typescript_external_lookup","status":"stale","truncated":false}`
on the initial query and both allowed follow-ups. The harness requires an
unsupported-input rejection (`unavailable` or `invalid_request`) after the
bounded transition allowance; it never received an incorrect `ok` answer.
After removing that same ancestor directory in the same broker/socket run,
`correct/config/ancestor-restored/a/cold_primary_references` passed with all six
exact deleted-scenario references in 63.381 ms. The create/reject/remove/recover
sequence is already enforced by `Runner.config_changes`; recovery is not inferred
from restarting the broker.
The test and exact location oracles remain unchanged pending the adapter owner's
classification/lifecycle fix. No rerun has yet been justified by a source fix.

Preserved evidence:
`/tmp/codex-nav-languages/runtime-typescript-75e237f-20261024-a/receipt.json`,
`/tmp/codex-nav-languages/runtime-typescript-75e237f-20261024-a/execve.log`,
native supervisor `/home/eric/.local/state/agent-jobs/job-xvt9v31k/receipt.json`.

### Corrected TypeScript build: shared cancellation regression

Source `854d6d37c84420eac0ef5bd3b7bc62c498cfeb44` changes unsupported-input
classification and retires sessions after invalid external inputs or incomplete
readiness. Its build receipt is
`/home/eric/.local/state/agent-jobs/job-mo9svfam/receipt.json`, log
`/home/eric/.local/state/wait-for-build/build-bb7kb02r/build.log`. Private broker
copy `/tmp/codex-nav-languages/artifacts-854d6d37c8/codex-nas-lsp` has SHA-256
`17e8033f87c2d69ba53b6222985b5a77ca48956ac065d2cfcac7382cac0039d1`;
the driver hash remains `a314cc5c89934bec218517929595c09d41348dd786914f2db400447117a8f7fc`.

The unchanged Python rerun completed **555/556**: all exact answers passed, but
`cancel/same_surviving_processes` failed. Two warm worker identities existed
before cancellation, and only the untouched B worker remained afterward. The
cancelled A request returned `cancelled` in 7.050 ms; its concurrent survivor
answered exactly in 6.631 ms. This does not meet the same-process acceptance
gate. The new unconditional session retirement in shared validation/readiness
error paths is the source-level candidate; cancellation must be distinguished
from invalid/incomplete state before any rerun. Warm p95 was 8.650 ms; byte
accounting was unchanged at 4,107 / 18,162.

Preserved receipt:
`/tmp/codex-nav-languages/runtime-python-854d6d3-20261024-a/receipt.json`,
native supervisor `/home/eric/.local/state/agent-jobs/job-kj_orxi1/receipt.json`.

The strict TypeScript rerun at this same head completed **742/742**, with every
location oracle and both source paths unchanged. The six-check reduction comes
only from eliminating two stale follow-ups, each of which adds three transport
checks. Ancestor creation returned `unavailable` in 13.593 ms; removing it
restored all six exact references in the same broker/socket in 1,015.579 ms.
Cold declaration references were 996.251 ms (A) and 984.208 ms (B); the new cold
empty root was 1,059.143 ms. Warm p95 was 432.717 ms, navigation/baseline bytes
remained 4,500 / 21,452 (20.977%), cgroup peak was 309,276,672 bytes, and shutdown
completed in 3.197 ms with exit zero and no observed survivors. Cancellation
identities, independent exec observation, pins and output budgets all passed.

TypeScript passing receipt:
`/tmp/codex-nav-languages/runtime-typescript-854d6d3-20261024-a/receipt.json`,
native supervisor `/home/eric/.local/state/agent-jobs/job-35oegr3y/receipt.json`.
This head still fails the separate Python cancellation gate above and is not
qualified as a complete multi-language delivery.

### Assembled adapter and cancellation-fix build

The assembled source is `bf3da3fbc5ad367b3f11aa1c34558b8df109b198`; build
receipt `/home/eric/.local/state/agent-jobs/job-_9xlkuae/receipt.json`, log
`/home/eric/.local/state/wait-for-build/build-y1ritr70/build.log`.
Private copies in `/tmp/codex-nav-languages/artifacts-bf3da3fbc5/` preserve broker
SHA-256 `6351a5670ba41e9675e1c8ea436d2af1e8d481ae92b9e622d64d23f9b6de1244`
and driver SHA-256
`a314cc5c89934bec218517929595c09d41348dd786914f2db400447117a8f7fc`.
Native runs use harness checkout
`84d56aaf94384b77857fa851812b727c5ec8646e`, language harness SHA-256
`432dbea4fc8c1dfe85ede11264028e15f9f5513d212c0e4730712ba920abb7a0`,
and unchanged Python harness SHA-256
`9006c8600dc8020c3ad1c32d9f05cde9325b770499f7297a35bd0a6f06ebefc3`.

Python completed **556/556**, including same-worker cancellation retention,
with warm p95 9.015 ms, 4,107 / 18,162 response/baseline bytes, and 15.385 ms
clean shutdown. Evidence:
`/tmp/codex-nav-languages/runtime-python-bf3da3f-20261024-a/receipt.json`,
native supervisor `/home/eric/.local/state/agent-jobs/job-xp1xnw2o/receipt.json`.

TypeScript at this assembled head passed **742/742**. Cold A/B declaration
references were 1,131.519 / 1,692.197 ms, cold empty 1,624.276 ms, warm p95
485.121 ms, bytes 4,500 / 21,452 (20.977%), peak 309,112,832 bytes, shutdown
3.179 ms with exit zero. Receipt:
`/tmp/codex-nav-languages/runtime-typescript-bf3da3f-20261024-a/receipt.json`,
native supervisor `/home/eric/.local/state/agent-jobs/job-f24bg401/receipt.json`.

Rust completed **829/847**, so this assembled head is **not qualified**. All 18
failed exact-query checks returned `stale` / `files_changed_during_query` after
the allowed follow-ups: all 14 A changed-scenario tasks, the first A deleted
reference query, model-deletion recovery, and both actual base91 source modes.
Base cold/warm queries, shifted B isolation, other later deleted queries,
cancellation/survivor identities, every safety negative, sentinel absence,
executable pins, resource limits and shutdown passed. Warm p95 was 18.403 ms,
bytes 4,488 / 19,922 (22.528%), peak 194,654,208 bytes, shutdown 113.564 ms exit
zero. The actual base91 `src/lib.rs` remained hash-identical, but its navigation
did not qualify.

Read-only post-run diagnosis found actual saved-file changes: pinned rustc
introspection crashed with `failed to create helper thread` (`Os` code 11,
`WouldBlock`, `Resource temporarily unavailable`) under the 64-task service
ceiling. It wrote 79 `rustc-ice-*.txt` reports in A and eight in the existing
base91 root. The freshness guard correctly rejected the genuinely changing
inventory. These new reports are preserved, including the source-root reports;
no cleanup or inventory exclusion workaround was applied. Production needs to
bound its Rust introspection concurrency and contain crash output before a
justified rerun, while retaining the fixed kernel and exact-answer gates.

The independent exec audit passed: 63 RA launches and 504 rustc introspections,
no Cargo, rustup, build script or repository executable. Each rustc argv matched
one of the four fixed version/cfg/target-spec introspection commands, verified by
`/tmp/codex-nav-languages/audit-rust-adapter-execve.py` after terminal completion.

Preserved Rust evidence:
`/tmp/codex-nav-languages/runtime-rust-bf3da3f-20261024-a/receipt.json`,
`/tmp/codex-nav-languages/runtime-rust-bf3da3f-20261024-a/execve.log`,
`/tmp/codex-nav-languages/runtime-rust-bf3da3f-20261024-a/rustc-ice-evidence.json`,
native supervisor `/home/eric/.local/state/agent-jobs/job-jsslh9gi/receipt.json`.

### Qualified Rust resource-fix result and final ledger

The architect traced excess threads to RA's separate global Rayon pool, which
is independent of its configured main worker count. Rust-only child environment
now sets `RAYON_NUM_THREADS=2` and points pinned rustc's `RUSTC_ICE` directory to
existing private child scratch space. This changes neither the 1 GiB / 64-task
ceilings nor any source, answer, latency, retry, or output gate.

The canonical, independently audited archive of exactly eight task-created
base91 reports was completed by the architect under root authorization before
the final run. The actual archive is
`/tmp/codex-nav-languages/rustc-ice-archive-bf3da3f/base91`; execution receipt
`/tmp/codex-nav-languages/rustc-ice-archive-bf3da3f/move-receipt.json` records each
original path, destination, hash, size and mtime. It preserves source hash
`82626519922c397afc5849c4c01b9a053129d6c85a5a24f9b692081e2bcc828f`
and zero remaining base91 reports. The acceptance agent's alternative
`base91-ice-archive-plan.json` was preparation-only and never executed.
The failed corpus roots and all previous failure receipts remain preserved.

Final Rust broker source: `65b2d1d630211fc08ee618e64991ee00dda80e8d`, build
receipt `/home/eric/.local/state/agent-jobs/job-ik9qo98o/receipt.json`, log
`/home/eric/.local/state/wait-for-build/build-n8mpguh0/build.log`.
Private artifacts `/tmp/codex-nav-languages/artifacts-65b2d1d630/` have broker
SHA-256 `6e133da63b73d63e8eb5392407af57a16956d43e6f28cd2b39069704ec40e2ee`
and driver SHA-256
`a314cc5c89934bec218517929595c09d41348dd786914f2db400447117a8f7fc`.
The Rust native run used harness commit
`e736dc097f6ffb39f10bd5a1230bc4150b8740dc`, SHA-256
`165cb162fa6ecff5d28d3fa9c4a998b8bebd9326ed3d4559219f84688d025e5c`.
That harness adds a strictly stronger before/after check for compiler crash
reports in every registered root; no earlier assertion was relaxed.

The final Rust run passed **731/731**. All exact saved-scenario answers and
complete cold reverse references passed, including changed/deleted files,
config model removal/recovery, both shifted linked worktrees and Unicode.
Cold references were 218.219 ms (A) and 504.471 ms (B); cold empty was
213.051 ms; warm p95 was 15.132 ms. Navigation returned 4,488 bytes versus
19,922 rg/read baseline bytes (22.528%). Cgroup peak was 57,036,800 bytes,
shutdown 7.271 ms, exit zero, no observed worker survivors. Cancellation
identities, safety negatives, private sentinel absence and all budgets passed.
Check-count changes relative to the failed run reflect removal of stale
follow-up queries, with the new crash-report assertion added.

The actual existing base91 `ENCTAB` use resolved exactly to `src/lib.rs:38:7-13`
in Direct (212.722 ms) and Code Mode (11.204 ms). Its source hash remained
unchanged and every registered root's compiler-report inventory was empty both
before and after the run. The independent exec audit passed 75 launches:
nine RA and 66 canonical rustc invocations, each from the fixed allowed
introspection argv list; no Cargo, rustup, build script or repository executable.
This establishes local base91 navigation only, with the Rust dependency/std
limitations above unchanged.

| Final profile | Runtime checks | Source | Warm p95 | Navigation / rg+read |
| --- | --- | --- | --- | --- |
| TypeScript | 742/742 | `bf3da3fbc5` | 485.121 ms | 4,500 / 21,452 (20.977%) |
| Rust | 731/731 | `65b2d1d630` | 15.132 ms | 4,488 / 19,922 (22.528%) |
| Unchanged Python | 556/556 | `bf3da3fbc5` | 9.015 ms | 4,107 / 18,162 (22.613%) |

The final Rust-only change leaves the qualified TypeScript/Python paths
unchanged; they were not needlessly rerun. A separate actual bounded-service
Rust regression also passed all 11 integration tests (five pinned-real), with
two live workers and saved-source regeneration: pids peak 44, `pids.events`
`max 0`, memory peak 44,285,952 bytes. Its receipt is
`/tmp/codex-nav-languages/rust-resource-regression/receipt.json`.

Final Rust evidence:
`/tmp/codex-nav-languages/runtime-rust-65b2d1d-20261024-a/receipt.json`,
`/tmp/codex-nav-languages/runtime-rust-65b2d1d-20261024-a/execve-audit.json`
(audit-script and trace hashes included),
native supervisor `/home/eric/.local/state/agent-jobs/job-k2sxqrmy/receipt.json`.
These are native executor/broker/server qualification receipts. No active
installation or model-session/V8 Code Mode end-to-end acceptance is claimed.
