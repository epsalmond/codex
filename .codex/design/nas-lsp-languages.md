# TypeScript and Rust navigation adapters

Follow-up to the merged [broker contract](nas-lsp-broker.md), baseline
`78e2f1ba6a`. This document is the implementation and review contract. It does
not authorize service installation. Python behavior and the native wire schema
remain compatible. All source inputs are saved files.

## Scope and immutable configuration

Add exactly two operator-selected profiles:

| language | readiness | executable and arguments | extra registry field |
| --- | --- | --- | --- |
| `python` | `pyrefly_lazy_blocking` | existing fixed Pyrefly invocation | none |
| `typescript` | `typescript_project` | canonical Node executable; `["/canonical/operator/typescript-language-server/lib/cli.mjs", "--stdio"]` | `typescript_tsserver = "/canonical/operator/typescript/lib/tsserver.js"` |
| `rust` | `rust_analyzer_project` | canonical rust-analyzer binary; `[]` | `rustc = "/canonical/pinned/toolchain/bin/rustc"` |

The JavaScript entry and tsserver are operator-managed immutable runtime files,
outside the registered root. Validate their canonical absolute regular-file
paths; only the Node executable needs executable bits. No shell, PATH-selected
launcher, workspace TypeScript selection, extra flags, or model-controlled
configuration. Missing registered files give explicit availability failure.
The worker identity includes readiness and optional tsserver/rustc paths.
Same-root entries for different languages stay rejected by the existing
non-overlapping-root rule; register separate bounded subprojects for this stage.

Defaults for query source extensions depend on language. Python stays py/pyi;
TypeScript admits and can query ts/tsx/js/jsx/mts/cts/mjs/cjs (including .d.ts);
Rust uses rs. Query policy may select a subset, but admission always includes
all supported documents of that language in the saved generation. Extension to
LSP language ID mapping is explicit (typescript, typescriptreact, javascript,
javascriptreact, rust); it is not the registry language verbatim for TSX/JS.

Keep the existing complete inventory and limits: only .git is excluded, maximum
10,000 entries, 2,000 files, 2 MiB/file, 32 MiB total. No target/node_modules
exclusion or arbitrary limit increase. A normal built Cargo checkout or installed
Node monorepo may exceed these limits and returns workspace_too_large. Source,
manifest/config changes and additions/deletions recreate the worker. Snapshot
before/after each query still rejects concurrent saved changes. External mutable
dependencies/configuration are unsupported, rather than silently outside the
freshness claim. Operator-pinned language runtimes are immutable until restart.

## Adapter interface and shared owner

The integrator owns config.rs, worker.rs, lsp.rs, lib.rs, existing worker tests,
manifests/lockfile, language/mod.rs, language/python.rs, and this design. Language
implementers own only their adapter, dedicated tests, and fixture subtree.
Each helper worktree starts from the committed shared scaffold, and contributors
must not revert edits by other lanes.

`language/mod.rs` dispatches a prepared profile, with this fixed interface:

```rust
pub(crate) enum Adapter { Python, TypeScript(typescript::Adapter), Rust(rust::Adapter) }
// Each language Adapter has these same methods:
async fn prepare(config: &WorkspaceConfig, snapshot: &Snapshot, deadline: Instant,
                 cancel: CancellationToken) -> Result<Self, RequestError>;
fn initialization_options(&self) -> serde_json::Value;
async fn ready(&self, session: &mut Session, deadline: Instant,
               cancel: CancellationToken) -> Result<ReadinessPermit, RequestError>;
async fn validate_inputs(&self, deadline: Instant,
                         cancel: CancellationToken) -> Result<(), RequestError>;
// Common dispatcher methods:
fn language_id(&self, relative: &Path) -> Option<&'static str>;
fn requires_server_status(&self) -> bool;
fn validate_query(&self, relative: &Path) -> Result<(), &'static str>;
struct ReadinessPermit { epoch: Option<u64> } // None only for existing Python
```

Prepare runs for each new generation before spawning a server. It validates the
language project model and constructs only broker-controlled initialization
options. The worker supplies common UTF-16/navigation/sync/workspace capabilities,
adds Rust's experimental server-status capability only for Rust, initializes,
admits every saved document, then invokes ready immediately before each actual
navigation request. After the response it verifies readiness/progress again and
compares the monotonic indexing epoch captured immediately before navigation.
Any intervening progress begin/end or Rust health/quiescence transition discards
the result even if currently idle again. Python preserves its existing completed
lazy-blocking query and progress semantics. The existing normalization,
full-output byte fitting, queue, deadlines, cancellation leases, process groups
and cgroup boundaries remain shared and unchanged.

Session accepts bounded workDoneProgress/create requests with a null reply;
tracks bounded progress tokens and Rust server health/quiescence; and wakes
readiness waiters with a notification rather than sleeps or repeated requests.
`RequestError::Indexing(&'static str)` maps to Indexing with no locations;
`RequestError::Stale(&'static str)` maps to Stale with no locations. Failure,
Timeout and Cancelled retain their existing mappings.
`Session::readiness_epoch()` and `Session::server_status()` expose bounded typed
state. `Session::wait_rust_ready(deadline,cancel)` waits on a Notify/watch change;
`Session::permit_valid(permit, require_status)` checks the epoch and current
health/progress atomically under one state lock. No language code owns the
reader/lease/cancellation task. The worker calls validate_inputs before every
query (including unchanged warm generations) and after its response, so absent
external lookup candidates cannot appear without invalidating the answer.
The shared owner supplies `ChildEnvironment` with a private scratch directory
and clean per-profile environment; both Session and the TS parsing helper retain
it for their child lifetime. Python launch behavior remains unchanged.
No arbitrary server request becomes executable: applyEdit stays applied:false,
executeCommand stays rejected, other unknown requests stay method-not-allowed.
If workspace/configuration is needed, reply only from the prepared immutable
profile to known language sections; never read repository/user settings or
accept new configuration supplied by the server. Adapter-owned outbound
read-only readiness requests are fixed constants, never model input.

## TypeScript lane

Own `nas-lsp/src/language/typescript.rs` and `nas-lsp/tests/typescript.rs`.
The independent corpus lane owns `scripts/fixtures/nas-lsp-typescript/**`;
consume its fixtures without editing them. No edits to shared files. Use the
privately provisioned pinned language-server/TypeScript/Node paths from
`/tmp/codex-nav-languages/toolchains.json` for real tests; tests that need these
tools explicitly require environment variables and otherwise remain ignored.

Initialize with explicit tsserver.path, fallbackPath equal to that path,
plugins:[], disableAutomaticTypingAcquisition:true, and a single semantic
tsserver (useSyntaxServer:"never"). Disable logs/telemetry/client watchers.
No npm acquisition, plugin configuration, code action, build or script command.
Empty initialization plugins alone is NOT a security claim: inspect and test
the pinned tsserver loader. Before spawn, parse EVERY discoverable tsconfig/jsconfig JSONC
and recursively extended configuration from the snapshot; reject nonempty
compilerOptions.plugins, external/unresolved config inheritance and project
references. Reject plugin execution even when requested from an inherited or
nested config. A small broker-owned Node helper embedded in typescript.rs loads
only the canonical pinned typescript.js sibling of tsserver.js. Pass saved text
as bounded JSON stdin; call the compiler's preProcessFile(text,true,true) and
parseConfigFileTextToJson, never import repo code or evaluate config. JSDoc
imports are not covered by preProcessFile: use the same pinned compiler's
createSourceFile AST/JSDoc traversal (including JSDocImportTag and ImportTypeNode)
or scanner comment-token rejection for unsupported JSDoc import forms. Include
adversarial `/** @import {T} from "../../outside" */` and
`/** @type {import("../../outside").T} */` tests. No ad hoc
source lexer. The helper runs once per generation under the existing cgroup,
same absolute deadline/cancel, bounded input/output (maximum 64 MiB/4 MiB), clean
environment, kill-on-drop and explicit process-group kill/reap on every exit.

Require root tsconfig.json/jsconfig.json AND package.json to stop ancestor
configuration/package discovery. Only self-contained local projects qualify.
Account for relative/absolute import/export/require/import() specifiers, TS
import=require, JSDoc @import and triple-slash path/type/lib directives through
the pinned parser; reject unsupported resolution syntax explicitly. Escaping
root paths and URL specifiers fail. Bare specifiers must resolve to in-root
aliases or in-root dependencies; unresolved external inputs fail. Config
inheritance/references stay in snapshot. Audit files/include/exclude,
rootDirs/typeRoots/baseUrl/paths/rootDir, types/lib,
resolvePackageJsonImports/Exports and other supported path-bearing options;
unhandled resolution options fail closed. Validate package
main/types/typings/exports/imports/typesVersions mappings and config references
as in-root paths. Unsupported complex mappings fail explicitly.

Require every ancestor node_modules lookup candidate absent; guard these
negative inputs (and ancestor config/package candidates where relevant) before
and after each query. External candidate creation invalidates an answer even
if projectInfo.fileNames is unchanged. This bounded absence check avoids
scanning unrelated external trees or claiming mutable external dependencies.

After all documents are admitted, use the pinned server's documented
workspace/executeCommand with command `typescript.tsserverRequest` and exact
arguments `["projectInfo", {file: <saved file URI>, needFileNameList:true}]`
as a blocking semantic-project barrier. Verify the pinned response shape and
semantic service availability. Check project file membership for every admitted
source (deduplicate equivalent projects where valid); every non-runtime input
reported by tsserver must exist in the snapshot. Only files under the canonical
pinned TypeScript lib directory may be external immutable runtime inputs.
Active loading or unsupported/incomplete project membership returns indexing or
unavailable, never an empty ok. Completed definition/references follows this
barrier; references includeDeclaration:true and exact reverse dependents.

Real tests must establish the exact source sets for cold/warm definitions and
references from both use and declaration, unopened reverse users, same-name
symbols in separate modules, repeated uses, Unicode scalar columns, genuinely
empty answers, tsconfig alias changes, source/config edits/deletions, ancestor
lookup creation, package metadata changes with unchanged source membership, worktree
isolation and a malicious plugin sentinel which must never run. Record which
JS/TSX/declaration/config features are actually exercised, rather than expanding
claims from extension admission alone.

Implemented TS profile: all static import/export declarations, including
namespace re-exports, are collected with the pinned AST as well as preprocessing.
All JSDoc import forms and JSX import-source/runtime
pragmas are rejected with the pinned scanner; no partial import discovery is
accepted. An ancestor node_modules candidate makes the profile unavailable and
retires the worker, so restoring the absent guard starts a fresh session.
Declaration maps and
sourceMappingURL redirects, dynamic unresolved imports, conditional/pattern
package mappings, imports/typesVersions and unhandled compiler resolution
options are also explicit unsupported cases. Worker qualification exercises .ts,
.js import/use, .tsx declarations/use and .d.ts; it does not qualify JSX elements
or separate module-extension matrices. Parent native/broker acceptance remains a
separate gate from these worker tests.

## Rust lane

Own `nas-lsp/src/language/rust.rs` and `nas-lsp/tests/rust.rs`.
The independent corpus lane owns `scripts/fixtures/nas-lsp-rust/**`;
consume its fixtures without editing them. No edits to shared files.

Discover bounded standalone dependency-free Cargo packages by parsing saved
Cargo.toml as TOML data and synthesizing an inline graph. Support package
name/edition, default or declared local lib/bin roots, and the package/lib-name
dependency edge from bin to lib. Reject workspace/inherited fields, nonempty
registry/git/path dependencies, features/required-features, target-specific
dependencies, proc-macro libraries and unsupported Cargo target contexts.
Auto targets in src/bin, tests, examples or benches must either be completely
represented (including cfg(test) where needed) or cause explicit rejection;
silently omitting reverse users is forbidden. A basic Cargo package needs no
hand-authored graph. Existing real candidates are WezTerm's bounded base91 and
bintree crates; acceptance must establish actual navigation, not assume std
semantics from the manifest.

For explicit local multi-crate graphs, accept a saved rust-project.json with
the small validated schema below and pass inline linkedProjects. If both models
exist, select rust-project.json, with no implicit Cargo merge. Never invoke
Cargo metadata or infer arbitrary Cargo workspaces: Cargo configuration and
toolchain overrides can execute code even with build scripts disabled.
Unsupported Cargo semantics return unsupported_cargo_project. This delivery
does not claim dependency-heavy workspaces such as Codex. Operators may provide
complete local graphs, with all transitive mutable inputs inside the snapshot.

Rust source closure is deliberately syntactic. Use the existing locked syn 2
parser (full+visit) plus proc-macro2 token trees; no Rust code execution or custom
lexer. Parse saved .rs files before spawn; reject unsupported parse syntax,
all custom macro_rules definitions, #[path]/#[cfg_attr] attributes and include!/include_str!/include_bytes!/
env!/option_env! invocations. Reject these spellings in macro token trees too,
including raw identifiers, and reject macro token trees capable of generating
module declarations (`mod` tokens), rather than implementing macro expansion.
This conservative profile forbids external/.git inputs and generated modules.
Comments and ordinary string contents do not become directives. Before syn,
the iterative token walk limits group nesting to 32, uninterrupted sibling runs
to 64 and total tokens to 65,536 per file. Skip recursive expression visitors;
deep/unsupported syntax returns unavailable instead of risking parser stack
overflow. This is a bounded source profile, not unrestricted Rust syntax.

Derive a bounded reachable source-file set from declared crate roots and normal
syn ItemMod declarations: handle inline modules and the ordinary name.rs or
name/mod.rs rules, with exactly one saved candidate inside the snapshot and no
.git path. Reject missing/ambiguous module files. Out-of-line modules must be
unconditional, or use exact cfg(test) with test enabled in the selected model;
reject other cfg-controlled module-file contexts rather than calling an inactive
file indexed. Model cfg(test) explicitly where tests are qualified; conditional
items inside an otherwise supported file follow the selected runtime/graph cfg.
Only reachable graph members may be queried: validate_query rejects orphan.rs
and build.rs with unsupported_rust_source. Admission of text to the VFS and
viewFileText are not proofs of crate membership. An unreachable file remains
fingerprinted and admitted, but never gains query eligibility merely by opening
it. Tests include external/.git directive edit/create/delete, macro-wrapped
directives, nested ordinary modules, inactive/missing modules, and orphan queries
that must return unavailable rather than false empty ok.

Accept only local crates with root_module, edition, deps, optional display_name,
cfg and is_workspace_member:true. Dependencies are crate indices into this same
graph. Validate every module root as a saved .rs input inside the root. Reject
sysroot/sysroot_src, target overrides, external roots, proc_macro_dylib_path,
runnables, build metadata, arbitrary env and unknown schema fields. No implicit
sysroot discovery: this initial profile covers the explicitly described local
crate graph, without std/external dependency or generated/proc-macro semantics.
Use linkedProjects inline, cargo.sysroot:null, cargo.buildScripts.enable:false,
procMacro.enable:false, checkOnSave:false, check.enable:false as supported by
the pinned version, cfg.setTest:false, and no workspace discovery commands.
Set cargo.autoreload:true only for this nonempty validated inline graph. In the
pinned server, false leaves the initial queued workspace load undrained and
permanently reports unhealthy pending reload; a reloadWorkspace request merely
queues the same blocked operation. With inline linkedProjects, the enabled queue
reloads that fixed graph through load_inline, bypassing manifest discovery. No
detached files, discovery command, sysroot or sysroot source is supplied, so this
does not enable Cargo discovery or builds. Explicit cfg values alone control
test-module membership; the server's default injected test cfg is disabled.
Reject saved rust-analyzer.toml/.rust-analyzer.toml wherever RA could discover
them, and isolate user settings with private HOME/XDG_CONFIG_HOME. The LSP client
returns only these immutable settings if asked. Sanitize inherited command
injection variables (NODE_OPTIONS/NODE_PATH and Rust/Cargo wrapper/override
variables) for the relevant child profile; never install a global environment.
Require registered canonical real rustc: even inline projects invoke compiler
introspection in the pinned RA. Set a clean env, fixed RUSTC/PATH, private
HOME/CARGO_HOME/XDG_CONFIG_HOME/TMPDIR, offline policy and no wrappers/toolchain
variables. The supporting process-real trace
`/tmp/codex-nav-languages/inline-rust-probe/execve.log` saw only RA plus four
rustc version/target-cfg/layout commands, no Cargo/rustup/build/check despite
hostile Cargo config/build.rs/rust-toolchain files. Final qualification repeats
this sentinel evidence; the probe alone is not adapter acceptance. The original
false-autoreload probe established execution restrictions but did not establish
healthy readiness. Qualification must demonstrate healthy quiescence and exact
references with the enabled inline-graph reload queue, retaining every existing
health, admitted-text and readiness-epoch check.

Enable experimental.serverStatusNotification. After complete admission, first
wait for healthy quiescence, then issue
one fixed read-only rust-analyzer/viewFileText request per saved source URI and
require the exact admitted text. This causal barrier follows final didOpen:
pinned main_loop.rs handles/process_changes after every event before processing
the next request; handlers/request.rs reads the analysis snapshot; dispatch.rs
returns a default while VFS is unready. A text mismatch returns Indexing, without
request retries or sleeps. A successful text pass is cached only for that saved
generation. Warm queries still require healthy quiescence and a fresh permit.
After the text barrier, wait
for health:ok and quiescent:true with no active work progress, then issue the
navigation request. An initialize result, fixed delay, or first empty answer
does not establish readiness. Missing/non-quiescent status is indexing with no
locations; health warnings/errors are unavailable with no locations. Initial
status true alone cannot bypass the post-admission text barrier. Use the
same absolute request deadline and event notifications; cancellation returns
cancelled. Verify readiness again after a response to discard racing indexing.

Real tests exercise local cross-module/crate definitions and complete references
(including unopened reverse users and declaration), repeats, same names,
Unicode, cold empty, saved source/model changes/deletions and separate roots.
Include hostile build.rs, .cargo/config rustc wrappers, rust-toolchain and a
proc-macro/runnable model payload as sentinels: preparation must reject unsafe
model fields and no repository code may execute. Demonstrate the pinned RA
launch performs no cargo/build/check/proc-macro command in this profile.

## Acceptance, outages and documentation

Keep the existing Python focused regression suite. New deterministic tests cover
profile validation, wrong language/readiness, source mapping, progress creation,
missing/bad readiness, unsafe configs, server absence/failure, cancellation and
survivor behavior. Real per-language tests cover the source matrix above with
100% exact audited location sets and zero cross-root leakage. Predeclare budgets:
10s per query including startup (the native outer timeout), retained hard resource
and serialized output caps. Measure cold/warm latency; no sleep-based threshold
or timeout increase may turn incomplete indexing into success. Root acceptance
owns broker/cgroup process-real qualification and Direct/Code Mode byte caps.
Mock, real worker, broker/native adapter, and live model-session E2E evidence
remain distinct. No live model session is implied by direct adapter tests.

No endpoint at host startup (and no configured socket) omits the tool. A lost
endpoint returns unavailable; a stalled endpoint ends at the existing 10s outer
timeout. No retries or automatic rg fallback. A short tool-description hint may
tell the model to continue with bounded rg/read when navigation is unavailable.
Operator docs must include the private registry TOML, canonical tool setup and
versions, manual start command, per-language source examples, bounds, immutable
runtime policy, Rust project-model limitations, TS plugin/config safeguards and
outage behavior. Add a README pointer. No service is deployed by this work.

## Review gate and evidence

Independent Sol/high plan review precedes shared changes. Resolve material
contract findings, commit the shared compileable scaffold, then start the two
Sol/high implementers from that exact commit in separate helper worktrees.
Their commits are cherry-picked serially by the integrator; each lane runs its
focused mocked and pinned-real tests. The independent root reviewer owns the
final assembled review. This file is updated with durable API/scope changes;
operator docs and acceptance receipts carry reproducible runtime evidence.

Upstream references: [rust-project.json](https://rust-analyzer.github.io/book/non_cargo_based_projects.html),
[RA status](https://rust-analyzer.github.io/book/contributing/lsp-extensions.html),
[RA security](https://rust-analyzer.github.io/book/security.html),
[tsls readiness command](https://github.com/typescript-language-server/typescript-language-server/blob/master/src/commands/tsserverRequests.ts),
[tsls server profile](https://github.com/typescript-language-server/typescript-language-server/blob/master/src/tsServer/spawner.ts).
