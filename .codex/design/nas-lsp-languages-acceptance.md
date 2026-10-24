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
