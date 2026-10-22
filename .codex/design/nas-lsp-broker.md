# NAS semantic-navigation broker, stage 1

Contract for fork issue #164; source baseline `b1473a8`. This document fixes the
implementation boundaries before parallel work. Nothing here installs or enables
a service. Model-visible scope is saved-file definitions and references only.

## Existing integration seams

Use `codex-tools::ToolExecutor<ToolCall>` through the existing
`core/src/tools/handlers/extension_tools.rs::ExtensionToolAdapter`. The adapter
already passes the exact step's selected environments and `ToolCallSource`;
`ToolCall::response_byte_budget` already distinguishes Direct and Code Mode.
`ext/skills/src/tools/list.rs` demonstrates whole-JSON response fitting. Return a
bounded `JsonToolOutput`, whose `code_mode_result` preserves the same JSON value.
Do not create a second tool dispatcher, environment manager, or cancellation
framework. Reuse `codex-uds` socket-directory helpers and established process-group
shutdown patterns from `rmcp-client/src/stdio_server_launcher.rs`.

One necessary host addition is an explicit access verdict, not an exposed
filesystem or permission profile. `ToolEnvironment` currently has no locality or
read-policy query. Add `LocalNavigationAccess::{Allowed, Remote, Restricted}`;
`ToolEnvironment::new` defaults to Restricted and
`with_local_navigation_access(self, verdict) -> Self` supplies the host verdict.
The host computes it from `TurnEnvironment.environment.is_remote()` and the
step's effective `sandbox_context(grants).should_read_from_sandbox()`. Extend
`StepContext::environments` (or add a dedicated adjacent helper) to capture grants
once and construct the callback accessor and verdict together; do not separately
reread live grants in the adapter. A local-looking
URI or environment id is not authority. Add a callback-scoped cancellation token
to `ToolCall`, cloned from the existing invocation token; no new cancellation
service. Existing constructing test fixtures need mechanical token defaults.

## Packages, lane ownership, and fixed interfaces

The integrator alone owns workspace `Cargo.toml`, `Cargo.lock`, initial package
manifests, shared protocol, and this document. Lane worktrees start from its
committed scaffold. Contributors must not revert other lanes' changes.

| Lane | Owned files | Responsibility |
| --- | --- | --- |
| A: broker | `codex-rs/nas-lsp/src/{config,broker,pool,resource}.rs`, `main.rs`, broker-specific tests | Operator registry, UDS service, request admission, shared workers, shutdown, eviction, memory policy |
| B: language worker | `codex-rs/nas-lsp/src/{worker,lsp,freshness}.rs`, worker-specific tests | Bounded stdio LSP, disk snapshots, positions, Pyrefly readiness, result normalization |
| C: native tool | `codex-rs/ext/semantic-navigation/**`; `tools/src/tool_call.rs`, `tools/src/lib.rs`; `core/src/session/step_context.rs`; adapter and its tests; app-server registration/dependency | Tool schema, access verdict, client, cancellation, direct/Code Mode cap, extension registration |
| D: acceptance | `scripts/fixtures/nas-lsp/**`, `codex-rs/nas-lsp/tests/acceptance/**`, `codex-rs/nas-lsp/examples/**`, `.codex/design/nas-lsp-acceptance.md` | Fixed corpus/oracle, process-real integration and adversarial protocol tests, reproducible benchmark |

New packages: `codex-lsp-protocol` at `lsp-protocol/` (serde wire types, bounded
framing and response fitting), `codex-nas-lsp` at `nas-lsp/` (library and
`codex-nas-lsp` binary), `codex-semantic-navigation` at
`ext/semantic-navigation/`. Each module begins with a link to this design.
No dependency from the broker to core or app-server. No new public app-server
protocol or generated client types. Install the extension in
`app-server/src/extensions.rs::thread_extensions`; current TUI/exec use that
server path. Other custom hosts opt in with `install`.

Shared Rust names (snake_case wire fields; enums snake_case; deny unknown input
fields):

```rust
// codex_lsp_protocol
pub const MAX_REQUEST_BYTES: usize = 8192;
pub const MAX_RESPONSE_BYTES: usize = 16384;
pub const MAX_RESULTS: usize = 100;
pub enum Operation { Definition, References }
pub struct Position { pub line: u32, pub column: u32 }
pub struct Location { pub path: String, pub start: Position, pub end: Position }
pub enum Status { Ok, Unavailable, Indexing, Stale, Timeout, Cancelled,
                  Busy, InvalidRequest }
pub struct NavigationRequest {
    pub version: u32,                  // exactly 1
    pub cwd: PathBuf,                  // host canonical cwd, never model input
    pub path: String,                  // workspace-relative saved source path
    pub operation: Operation,
    pub position: Position,
    pub max_results: usize,
    pub max_response_bytes: usize,
    pub timeout_ms: u64,
}
pub struct NavigationResponse {
    pub status: Status,
    pub locations: Vec<Location>,
    pub truncated: bool,
    pub reason: Option<String>,        // bounded stable code, no server stderr
}
pub fn fit_response(response: NavigationResponse, budget: usize)
    -> Result<NavigationResponse, BudgetTooSmall>;
pub struct BudgetTooSmall;
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R, limit: usize)
    -> io::Result<Vec<u8>>;
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8],
                                             limit: usize) -> io::Result<()>;

// codex_nas_lsp::worker; all owned handles Send, shared handles Sync.
pub struct Worker; // owns one process group, may have an internal reader task
pub struct WorkerQuery {
    pub operation: Operation, pub path: PathBuf, pub position: Position,
    pub max_results: usize,
}
impl Worker {
    pub async fn start(config: Arc<WorkspaceConfig>, deadline: tokio::time::Instant,
                       cancel: CancellationToken) -> Result<Self, NavigationResponse>;
    pub async fn query(&mut self, query: WorkerQuery, deadline: tokio::time::Instant,
                       cancel: CancellationToken) -> NavigationResponse;
    pub async fn shutdown(&mut self); // 2 seconds maximum including force kill
    pub fn process_id(&self) -> Option<u32>;
}
// Config is the broker lane's public typed configuration. Root and command
// already canonicalized/validated before Worker::start.
pub struct WorkspaceConfig {
    pub id: String, pub root: PathBuf, pub language: String,
    pub executable: PathBuf, pub args: Vec<String>,
    pub source_extensions: Vec<String>, pub exclude_dirs: Vec<String>,
    pub readiness: Readiness, // initially PyreflyLazyBlocking only
}
pub enum Readiness { PyreflyLazyBlocking }
pub struct BrokerConfig {
    pub socket_path: PathBuf, pub workspaces: Vec<Arc<WorkspaceConfig>>,
    pub limits: PoolLimits,
}
pub struct PoolLimits {
    pub max_workers: usize, pub max_queue_per_worker: usize,
    pub max_queries: usize, pub max_connections: usize,
    pub idle_ttl: Duration, pub memory_max_bytes: u64, pub pids_max: u64,
}
```

`nas-lsp/src/lib.rs` exports `pub mod config; pub mod broker; pub mod worker;`
and other modules as needed. Broker API:
`Broker::new(config: BrokerConfig) -> Result<Broker, BrokerError>`;
`Broker::query(&self, request: NavigationRequest, cancel: CancellationToken)
-> NavigationResponse`; `Broker::serve(self, shutdown: CancellationToken)
-> io::Result<()>`. Broker holds the shared per-workspace worker slots; queries
serialize within a slot while separate workers can execute concurrently. Worker
owns freshness synchronization within that same exclusive query lock. Pool
owns last-used time, admission, and exclusive idle eviction. Each slot owns the
worker task/actor and LSP reader independently of the client handler's lifetime.
Client disconnect cancels its request token and returns promptly, but its lease
remains held by that task until the response drains or its operation deadline
expires. Never drop a mutable Worker query future merely because the client went
away. Queued requests retain the shared slot and can use its surviving worker.

Extension public API: `install<C>(&mut ExtensionRegistryBuilder<C>)`; it does
not need to depend on core to interpret host configuration. Endpoint
defaults to `codex_uds::shared_daemon_socket_directory()/semantic-navigation.sock`,
optionally fixed by `CODEX_SEMANTIC_NAVIGATION_SOCKET` at host startup to another
socket within that same reserved directory. Reject paths outside the reserved
directory. Codex already hides this directory and its aliases from sandboxed
processes (`linux-sandbox/src/bwrap.rs` and daemon socket tests); reusing it closes
the restricted-shell bypass that an arbitrary CODEX_HOME socket would expose.
No endpoint in model arguments or repository config. Do not create the broker, register roots, or
start servers from the extension. Expose `semantic_navigation` only when the
host endpoint is configured/present, and allow compact unavailable responses
when it subsequently disappears.

## Model and transport contract

Tool arguments are exactly `{operation: "definition" | "references", path:
string, line: integer >= 1, column: integer >= 1}`. References include the
declaration. Description: "Find saved-file definitions or references. Paths are
relative to the registered workspace; line and column are 1-based Unicode
characters. Returns locations and an explicit availability/freshness status."
One environment must be selected; ambiguous multiple selections fail unavailable
(`ambiguous_environment`). Remote/restricted verdicts fail before socket I/O.
Canonicalize cwd through its supplied filesystem accessor, then use its native
local path. Server never receives a model-chosen cwd/root/command/method.

Wire transport is one request and one response per private local Unix stream,
four-byte unsigned big-endian length followed by UTF-8 JSON. No request IDs or
multiplexing are needed: connection loss is request cancellation. Client races
the call token and deadline; dropping its stream cancels the broker handler.
Server monitors EOF while executing that connection's request. Use Linux
`tokio::net::UnixStream` for `peer_cred` and split support; reuse codex-uds
directory/identity helpers without changing its portable transport API. Partial reads,
oversized lengths, unsupported versions, invalid JSON, and invalid paths close
the connection or return bounded InvalidRequest. Frames are checked before
allocation. A fixed 1-second header/body read deadline applies before parsing;
all response writes, including rejected Busy responses, have a 250ms deadline.
Maximum 32 accepted connections; an additional connection receives Busy and
closes without spawning an unbounded task. Absolute request deadline begins at admission and includes
queue, startup, freshness, and LSP. Hard maximum is 15 seconds, default 10.

External positions are 1-based Unicode scalar columns (not bytes or UTF-16),
exclusive ends. Worker converts from saved UTF-8 contents to/from zero-based LSP
UTF-16; advertise UTF-16 alone, accept default UTF-16, reject another encoding.
Reject out-of-range lines/columns and mid-surrogate results. Normalize both
Location and LocationLink results, canonicalize returned files, require paths
inside the exact registered root (otherwise Unavailable `external_result`),
deduplicate and sort by path/start/end, then
cap results. Never copy source snippets, diagnostics, or raw server messages.

The response is exactly NavigationResponse JSON. `reason` is an enum-like code
of at most 64 ASCII bytes; paths max 1024 UTF-8 bytes each; count max 100. Fit the
entire serialized JSON including escaping and metadata by dropping locations
and setting truncated. A fully omitted oversized item still sets truncated.
Broker clamps byte/count/deadline request limits to its constants. Extension
fits again with `ToolCall::response_byte_budget(MAX_RESPONSE_BYTES)`, returning
the identical fitted JSON via Direct and Code Mode. Measure response-content
JSON bytes, not outer Responses API envelopes/call ids. For a budget smaller
than the smallest status object, return the smallest valid JSON value that
fits (null for >=4 bytes, 0 for >=1, empty text for zero); never emit an oversized
error string. Null/zero fallbacks remain JsonToolOutput values, but zero-byte
empty-text fallback requires a tiny ToolOutput implementation; a JSON empty
string serializes to two bytes. Parse/argument/validation errors also use fitted
status values, never unbounded FunctionCallError messages. Tests cover this
exceptional host budget explicitly.

## Trust, registry, and workspace identity

Operator starts `codex-nas-lsp --config /absolute/private/registry.toml`.
Registry must be a regular owner-owned file with no group/other write bits;
socket parent must be the reserved daemon directory, owner-owned, mode 0700,
not a symlink. Reuse its strict preparation helper. Socket mode 0600;
validate same-UID peers on Linux. Validate client parent/socket ownership before
sending a path. Do not unlink a live socket; lock service startup, and remove a
stale socket only while holding that lock. No TCP listener or remote executor.

Registry contains explicit workspace entries with exact canonical roots, a
single supported language, absolute canonical executable, fixed argument vector,
source extensions, readiness adapter, and pool limits. `exclude_dirs` remains a
compatibility field, but cannot omit mutable input from inventory or admission:
only `.git` metadata is excluded in this stage. In particular a custom `hidden`
directory containing imported Python sources remains fingerprinted and admitted.
No shell, environment interpolation, command search on PATH, or dynamic
registration API. Reject overlapping/duplicate roots or ambiguous configurations.
Match canonical cwd equal to or below one registered root; reject unknown roots.
Resolve the model path against that registered root, reject absolute/traversal
paths and symlinks leaving it; require a regular UTF-8 source file. Validate root
identity again before each query; a replaced/missing root invalidates the worker.
Server configuration is immutable until broker restart.

Worker key is `(canonical root + filesystem device/inode identity, language,
SHA-256 of registered executable/args/readiness/source policy)`. Git's common
directory or repository name is never the key. Distinct worktrees get distinct
workers; same key reuses a worker across sessions. Operator-registered servers
are trusted programs and can read host files, import project configuration, or
launch subprocesses. Local full-read eligibility is mandatory: broader indexes
cannot be made safe for restricted profiles by filtering returned snippets.
Qualification applies to the pinned Pyrefly stdio profile and descendants that
remain in its process group. Operator binaries that daemonize, change process
groups, or depend on unrelated mutable services are outside this lifecycle
qualification. The enclosing cgroup still bounds descendant memory/processes,
but process-group shutdown does not prove that an arbitrary escaping daemon was
reaped; such a server is not qualified by this stage.

## Saved-file freshness and indexing

Use a bounded full regular-file inventory per query as the simple first-stage policy:
walk the registered root without following directory symlinks, omit only Git
metadata (`.git` directory or linked-worktree pointer file), and hash every other
regular-file content, including files inside fixed/operator exclusion hints,
.pyi, arbitrary nested project
configuration, manifests, and newly created/deleted files. Source extensions
control queried file eligibility, not what gets fingerprinted. Cap traversal at
10,000 entries, 2,000 files, 2 MiB/file and 32 MiB total bytes.
Exceeding any bound returns Unavailable (`workspace_too_large`), never partial Ok.
Include file contents in the hash so same-size/same-mtime rewrites are detected.
These defaults deliberately support bounded registered Python projects. The
whole Codex checkout is outside this inventory scope (approximately 9,585 files
and 96 MB in the initial source survey), and the installed host has no usable
Rust server. A registered `scripts/` subtree is a real supported Python target;
the selected environment cwd must resolve inside that registered root. The model
cannot choose a different root through a path argument. Larger inventories or
additional language adapters require explicit operator policy and qualification.
Generated/cache directories such as `target`, `node_modules`, `.venv`, and
`__pycache__` do not relax these limits: if they make an otherwise registered root
too large, return Unavailable. Pyrefly can read imports/configuration inside a
nominally excluded directory, so omitting their bytes would permit stale Ok.
Reject file symlinks rather than following them. No broad repository indexing or
background prompt injection is added. External mutable project dependency roots
are unsupported; qualification uses a self-contained project and operator-pinned
toolchain/stdlib. Server/toolchain files outside the registered root are trusted
immutable runtime inputs until broker restart; the service does not claim
freshness for independently changed external dependencies. Reject out-of-root
definition/reference targets explicitly rather than reporting a false empty Ok.

First query snapshots saved files before initialization and admits every saved
`.py` and `.pyi` document to the worker, including reverse dependents that have
never been queried. Require a supported document synchronization capability;
without complete admission, return explicit Unavailable/Indexing rather than
calling a declaration-only reference set complete. Advertise
`workspace.workspaceFolders: true` alongside the supplied workspace folders:
Pyrefly 1.2.0 otherwise ignores those folders and can infer different module
identities for a declaration and its import. Both complete admission and this
capability are necessary for exact declaration-position references. Before every later
query compare the complete inventory with the worker generation. For any added,
changed, deleted source/config file, shut down and recreate that worker before
querying and readmitting the complete saved generation; this deliberately
conservative policy avoids relying on inconsistent
server watcher support and clears deleted-file indexes. Reuse remains warm when
the inventory is unchanged. Synchronize the queried saved document with didOpen
and full-text didChange when supported; no unsaved buffer inputs. Inventory
again after the answer: if it changed during processing, discard locations and
return Stale (`files_changed_during_query`). Do not silently use the prior
generation on a failed refresh.

LSP has no universal indexed/completed handshake. Initial supported real adapter
is operator-registered Pyrefly with fixed `lsp --indexing-mode lazy-blocking
--threads 1`; after complete saved-source admission, a completed query is the
adapter's readiness barrier. Initialize
alone is insufficient. Track work-done progress and explicit server-not-ready
errors; active indexing gives Indexing, with zero locations. Unknown readiness
policies fail configuration rather than converting an empty answer to Ok.
Readiness behavior must be verified against the installed server, including a
cold empty-answer probe and warm changed/deleted corpus cases. A fixture-only
adapter is test support and does not qualify real language support.

On LSP timeout/cancel send `$/cancelRequest` for just that request ID, continue
draining its eventual response, and keep the shared process alive for subsequent
queries. An unresponsive cancelled request may occupy the worker until its
bounded operation deadline; queued callers can get Busy/Timeout. Do not kill a
shared worker merely because one caller cancels. Protocol failure or exhausted
shutdown deadlines may mark a worker unhealthy and restart it for future calls.

Allow only initialize/initialized, document open/change/close, definition,
references, cancellation, shutdown/exit, and bounded progress traffic. Reject
server-initiated workspace edits, command execution, arbitrary configuration
pulls, registration, and unknown requests with JSON-RPC method-not-found (applyEdit
may return applied:false). Drain/discard diagnostics without retaining them.
LSP header cap 8 KiB, message body cap 2 MiB, bounded stderr tail 4 KiB for host
logs only. A frame violation marks the worker unhealthy.

## Resource policy and lifecycle

Defaults: at most 2 workers, 1 active operation/worker, 4 queued operations/worker,
8 total admitted queries, 32 connections, 60-second idle TTL. Queue admission is
try-acquire and returns Busy rather than growing tasks without bound. Evict only
idle, unleased workers, oldest first; full active pool returns Busy (`pool_full`).
Retiring workers remain counted against the pool cap until their processes are
reaped. Retirement belongs to the pool/actor, so cancelling a replacement caller
cannot detach a removed worker and free its capacity early. Use one broker-owned
periodic maintenance task for idle/resource checks, not
per-session worker supervisors. No workers start until a registered query needs
them. Spawn into a process group; shutdown sends LSP shutdown/exit, then TERM and
KILL to the process group within 2 seconds. Broker shutdown has a 5-second bound
for all workers together. Child stderr is always drained with bounded storage.

Linux runtime requires a dedicated cgroup v2 containing broker and descendants,
with an operator-established finite `memory.max` at or below the configured
aggregate ceiling (default 1 GiB) and finite `pids.max` at or below the configured
process ceiling (default 64). Validate
these before starting workers; fail Unavailable (`resource_limit_unavailable`)
when no enforceable hard limit exists. An operator may launch a transient user
service/scope with MemoryMax/TasksMax; the broker does not change shared services
or create privileged host configuration. The aggregate cap includes the broker
and complete worker trees, avoiding per-process-RSS accounting gaps. Sample
worker process-tree RSS for admission/observability and retire idle workers at
80% of the hard budget; memory-pressure/OOM worker loss yields Unavailable.
Do not claim a sampled RSS limit is a hard aggregate ceiling. Tests with small
limits and temporary cgroups/scopes must verify this runtime prerequisite; if
unavailable, record the untested gate and keep #164 open.

## Early vertical path and acceptance thresholds (fixed before measurement)

First integrate one registered Python root, real Pyrefly, one definition and one
cross-file references query through UDS and the installed extension ToolExecutor.
Then add saved-file generation changes, resource policy, and adversarial cases.
Do not call an in-process fixture or direct broker unit test the vertical path.

Acceptance corpus is versioned under `scripts/fixtures/nas-lsp/`: at least
12 audited symbol tasks (6 definition + 6 references), across >=4 files, including
same-name symbols in separate scopes, >=3 occurrences on one line, Unicode before
the queried symbol (including a supplementary scalar), imports, a zero-result
case, and two worktree copies with different definitions. Store exact expected
path/ranges, symbol-specific rg commands and bounded file-read commands in an
oracle manifest; keep search false positives explicit. References mean semantic
uses/declarations, not comments/string matches. Compare the same navigation
questions, not arbitrary whole-repository grep output.

Success requires all of the following; an unmet threshold remains explicit and
does not justify changing it after measurement:

* 100% exact audited definition/reference location sets, with no cross-worktree
  contamination and no same-name or repeated-occurrence errors.
* Warm changed-file and deletion probes yield exact updated sets, or explicit
  Stale/Indexing while incomplete followed by exact updated sets; never stale Ok.
* 100% explicit-status/negative coverage: absent executable/socket, indexing,
  timeout, cancel, oversized result, pool/queue saturation, idle eviction,
  remote/restricted profiles, unknown roots/commands, symlink escape, malformed
  frames, and shared-session cancellation preserving another successful query.
* Every serialized response <=16,384 bytes and <=100 results, for Direct and
  Code Mode, including escaped paths, huge server errors and a tiny host budget.
* On the fixed small corpus: real cold query <=10 seconds; warm unchanged queries
  p95 <=1 second over 30 queries; refreshed queries <=10 seconds; request deadline
  return <= configured deadline +250ms, cancellation return <=500ms; worker and
  broker shutdown obey their 2/5-second bounds; idle eviction within TTL +2s.
* No more than configured workers/processes/queue entries; real descendants
  remain in the validated memory/pid cgroup; rejection when that prerequisite is
  absent. Cancellation test observes the same surviving worker PID.
* Across audited tasks, navigation response-content bytes <=60% of the equivalent
  rg-plus-bounded-file-read response bytes in aggregate. Record per-task values
  and failures; do not pad baseline output to achieve this threshold. Report
  bytes/4 as an explicitly approximate token estimate, not measured model tokens.

Record exact source head, server version/executable/args, OS/cgroup settings,
corpus manifest hash, command receipts, cold/warm/refreshed timings, response
bytes, estimates, correctness, and all untested boundaries in
`.codex/design/nas-lsp-acceptance.md`. Focused package tests plus a small host
adapter test are sufficient; avoid rebuilding half of core for a fixture loop.

## Sources and deferred scope

Position encoding, cancellation, lifecycle and readiness limitations follow the
[Microsoft LSP 3.17 specification](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/specification.md)
and its [initialize contract](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/general/initialize.md).
[Pyrefly 1.2.0 workspace initialization](https://github.com/facebook/pyrefly/blob/1.2.0/pyrefly/lib/lsp/non_wasm/server.rs#L2677-L2688)
requires the workspace-folder capability before it accepts supplied folders;
its [module identity fallback](https://github.com/facebook/pyrefly/blob/1.2.0/pyrefly/lib/lsp/non_wasm/workspace.rs#L148-L159)
explains why merely opening every source did not fix declaration references.
Prior Arcade Pyrefly qualification is context only; it is not this acceptance run.
No MCP, remote execution, restricted-read broker access, raw JSON-RPC tool,
workspace edits, rename, commands, diagnostics injection, tagged reads, hashline
edits, arbitrary model roots/executables, unsaved buffers, installation or
deployment is included. Continue ordinary file reads and existing apply_patch.
