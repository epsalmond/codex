# Native semantic navigation

`semantic_navigation` gives the model saved-file definitions and references
through an operator-started local language-server broker, `codex-nas-lsp`.
References include the declaration. The tool supports local, unrestricted read
environments; remote environments and restricted reads return `unavailable`
before socket inspection or connection. Multiple session environments are also
rejected as ambiguous. The operator registers roots and executables. The model supplies only an
operation, a relative source path, and a position.

The [broker contract](../.codex/design/nas-lsp-broker.md),
[language profiles](../.codex/design/nas-lsp-languages.md), and
[acceptance evidence](../.codex/design/nas-lsp-acceptance.md) define the supported
scope and distinguish real-server tests from interactive model-session tests.
This feature needs a fork build containing the broker and native extension.
Merging the implementation does not install a broker, enable a user service, or
change an active Codex installation.

## Workspace policy

Register canonical absolute directories and canonical absolute executable files
(resolve symlinks with `realpath`). Workspace IDs must be unique. Roots cannot
overlap, including entries with different languages, so register separate bounded
subprojects or worktrees. A session's working directory must be inside a registered
root; tool paths are relative to that root, even when the session starts in a
subdirectory. Each worktree has a separate worker identity.

The saved-file inventory is bounded to 2,000 regular files, 32 MiB total,
2 MiB per file, 10,000 visited directory entries, and 128 directory levels.
All non-`.git` files contribute to freshness, including project configuration,
ignored files, generated files, and files under `target`, `node_modules`, or
`.venv`. `exclude_dirs` does **not** exempt these inputs from hashing or limits.
An oversized workspace returns `workspace_too_large`; unreadable files,
symlinks, and special files also fail closed. Register an appropriately bounded
project rather than hiding mutable inputs with exclusions.

Saving, adding, or deleting an inventoried file invalidates the worker's saved
generation. Unsaved editor buffers are outside this interface. Locations outside
the registered root are not returned, and the tool does not rename or edit files.

Registration tells the broker where to index; it does not scan every checkout
on the host. The first query in a registered root creates a worker, snapshots
the bounded project, starts its server, and admits every saved file of that
language, including unopened dependents. The language profile then establishes
project readiness before navigation: Pyrefly lazy-blocking, TypeScript semantic
project membership, or rust-analyzer quiescence. Later queries reuse an unchanged
worker; saved source/configuration additions, edits, or deletions recreate it and
readmit the project. You do not select roots or trigger indexing in each prompt.

## Server installation

The pinned profiles use Pyrefly 1.2.0, Node 24.21.0 with
`typescript-language-server` 6.0.2 and TypeScript 6.0.3, and standalone
rust-analyzer 0.3.3089 from release 2026-10-19 with rustc 1.98.0. Use a private
tool directory outside every registered workspace. With Node 24.21.0 and npm 11.19.0 selected,
for example:

```bash
nav_tools="$HOME/.local/share/codex-navigation-tools"
mkdir -p "$nav_tools"
python3 -m venv "$nav_tools/pyrefly-1.2.0"
"$nav_tools/pyrefly-1.2.0/bin/python" -m pip install 'pyrefly==1.2.0'
npm install --prefix "$nav_tools/typescript-6.0.3" \
  --ignore-scripts --save-exact --no-audit --no-fund \
  'typescript-language-server@6.0.2' 'typescript@6.0.3'
```

Keep the generated package lock for repeatable `npm ci --ignore-scripts`
installs. TypeScript 7's native package does not supply this profile's JavaScript
tsserver interface; do not substitute an unpinned `typescript@latest`.

For Linux x86_64 GNU, provision the pinned standalone rust-analyzer from its
official release and verify the compressed asset before unpacking:

```bash
curl --fail --location \
  https://github.com/rust-lang/rust-analyzer/releases/download/2026-10-19/rust-analyzer-x86_64-unknown-linux-gnu.gz \
  --output "$nav_tools/rust-analyzer-2026-10-19.gz"
echo "8fde1c9dae81459f98e99eedae3654846a9166abea686306c26e57773be558da  $nav_tools/rust-analyzer-2026-10-19.gz" | sha256sum --check
gzip -dc "$nav_tools/rust-analyzer-2026-10-19.gz" > "$nav_tools/rust-analyzer-2026-10-19"
chmod 755 "$nav_tools/rust-analyzer-2026-10-19"
```

Resolve the resulting executable and JavaScript paths with `realpath` for the
registry below. The npm package entries are beneath `node_modules/` in that
private installation. The Node executable must also be canonical; a version
manager's shim is unsuitable. Keep the complete runtime trees and their parent
module directories controlled by the operator and immutable until broker
restart. Rust also needs a canonical real `rustc` from an operator-managed
1.98.0 toolchain: a rustup shim or mutable `stable` selector is unsuitable. The
worker uses that compiler only for version/target introspection; this profile
does not load its standard-library sources. Provisioning commands belong to
operator setup, not navigation calls.

## Registry

The broker reads one immutable TOML registry at startup. Both its registry file
and socket must be direct children of `/tmp/codex-daemon-UID/`, where `UID` is
the service user's numeric ID. This reserved directory is hidden from Codex
sandboxes. It must be owned by that user with mode `0700`; the registry must be
an owner-owned regular file with no group or other write permission, and cannot
be a symlink. The registry is limited to 64 KiB; unknown fields are rejected.

Use a pinned, operator-provisioned language-server installation outside the
workspace. Keep that installation and its dependencies unchanged while serving
queries. The broker does not download servers or accept a model-selected command.

Create the directory only if it does not already exist; if it exists, check its
ownership and permissions instead of replacing it:

```bash
nav_dir="/tmp/codex-daemon-$(id -u)"
if [ ! -e "$nav_dir" ]; then
  (umask 077; mkdir "$nav_dir")
fi
stat -c '%U %a %n' "$nav_dir"
```

Save the following as `$nav_dir/navigation.toml` with mode `0600`, replacing
`1000`, roots, and executable paths with your actual canonical paths. The
paths below are templates, not built-in host defaults. All three roots must
exist and must be separate; remove registrations you do not need. The arguments
and readiness values are required, not tuning hints.

```toml
socket_path = "/tmp/codex-daemon-1000/semantic-navigation.sock"

[limits]
max_workers = 2
max_queue_per_worker = 4
max_queries = 8
max_connections = 32
idle_ttl_secs = 60
memory_max_bytes = 1073741824
pids_max = 64

[[workspaces]]
id = "python-project"
root = "/srv/navigation/python-project"
language = "python"
executable = "/opt/navigation/pyrefly-1.2.0/bin/pyrefly"
args = ["lsp", "--indexing-mode", "lazy-blocking", "--threads", "1"]
source_extensions = ["py", "pyi"]
exclude_dirs = [".git"]
readiness = "pyrefly_lazy_blocking"

[[workspaces]]
id = "typescript-project"
root = "/srv/navigation/typescript-project"
language = "typescript"
executable = "/opt/navigation/node-24.21.0/bin/node"
args = ["/opt/navigation/typescript-6.0.3/node_modules/typescript-language-server/lib/cli.mjs", "--stdio"]
typescript_tsserver = "/opt/navigation/typescript-6.0.3/node_modules/typescript/lib/tsserver.js"
source_extensions = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"]
exclude_dirs = [".git"]
readiness = "typescript_project"

[[workspaces]]
id = "rust-project"
root = "/srv/navigation/rust-project"
language = "rust"
executable = "/opt/navigation/rust-analyzer-2026-10-19"
args = []
rustc = "/opt/navigation/rust-1.98.0/bin/rustc"
source_extensions = ["rs"]
exclude_dirs = [".git"]
readiness = "rust_analyzer_project"
```

Pool limits apply across all registered projects. Workers start on demand and
expire after the idle TTL. Changes to registrations require a broker restart.
A missing registered executable is allowed at startup and produces
`server_unavailable` when queried.

### Python

Use the qualified Pyrefly 1.2.0 profile with lazy-blocking indexing and one
thread. The broker admits every saved `.py` and `.pyi` file before querying,
including unopened reverse dependents. Restricting `source_extensions` narrows
which files may be queried, not which language files contribute to the project.
The inventory limits still include non-Python files. This is bounded local
project navigation; imports outside the registered project do not expand its
returned location scope.

### TypeScript and JavaScript

Register the canonical Node executable, the pinned language server's real
`lib/cli.mjs` entry, and its pinned TypeScript `lib/tsserver.js`. Do not register
an npm launcher symlink or rely on workspace/PATH TypeScript selection. The
JavaScript files must be canonical regular files outside the registered root;
they do not need executable permission. Keep the complete Node/server/TypeScript
installation immutable until restarting the broker.

The root must contain both `package.json` and either `tsconfig.json` or
`jsconfig.json`. A small self-contained TypeScript project can use this root
`package.json`:

```json
{"private":true,"type":"module"}
```

and `tsconfig.json`:

```json
{"compilerOptions":{"strict":true},"include":["src/**/*.ts"]}
```

Imports, package metadata, and configuration must resolve to saved inputs
inside the registered root. Configuration inheritance must stay in that
inventory. Project references, nonempty `compilerOptions.plugins` (including
inherited or nested plugins), unsupported resolution settings, and unresolved
external dependencies are rejected. The pinned TypeScript parser inspects
source/configuration as data, including JSDoc and triple-slash inputs. A
broker-controlled helper uses the registered Node and TypeScript runtime;
repository JavaScript is not evaluated. The server receives no plugins,
disables automatic typing acquisition, and uses the registered semantic
tsserver. No npm scripts or type installation run during navigation.

Ancestor `node_modules` lookup candidates must be absent. The adapter checks
these negative inputs and relevant ancestor package/configuration candidates
before and after each query; creating an outside lookup input invalidates a
result. Arbitrary external mutable npm packages are unsupported. A bounded
in-root dependency still counts toward inventory limits, including its package
metadata. The only external project inputs admitted by this profile are the
operator-pinned TypeScript library files. Keep the runtime and its ancestor
module directories immutable until broker restart.

The worker admits saved TS/TSX/JS/JSX and module variants, then checks semantic
project membership for every admitted source before returning locations.
Extension admission alone does not establish feature qualification; consult the
acceptance evidence for the exact tested matrix.

### Rust

Register both the standalone rust-analyzer binary and a canonical real `rustc`.
The supported automatic model is a bounded standalone Cargo package with no
dependencies. A minimal root `Cargo.toml` is:

```toml
[package]
name = "navigation-example"
version = "0.1.0"
edition = "2021"
```

Use saved `src/lib.rs` and/or `src/main.rs` and ordinary local `mod` files.
The broker parses the manifest as data and synthesizes a local crate graph,
including the binary-to-library edge. It rejects Cargo workspaces/inheritance,
registry/git/path dependencies, features/required-features, proc-macro libraries,
and unsupported target contexts. Additional auto-discovered binaries, tests,
examples, or benches can make a package unsupported; see the acceptance evidence
for qualified target layouts.

For an explicit local multi-crate graph, save `rust-project.json` at the root:

```json
{"crates":[{"root_module":"src/lib.rs","edition":"2021","deps":[],"is_workspace_member":true}]}
```

A binary crate can use `src/main.rs`. List every local crate and express graph
edges with dependency entries such as `{"crate":0,"name":"local_library"}`.
Every crate root and all transitive mutable inputs must be saved inside the
registered workspace. If both models exist, `rust-project.json` takes precedence;
there is no implicit Cargo merge. The graph schema accepts root modules,
editions, dependencies by crate index, optional display names/cfg values, and
`is_workspace_member: true`. External roots, sysroot/target overrides, build
metadata, arbitrary environment variables, proc-macro dylibs, runnables, and
unknown fields are rejected.

Source closure follows ordinary local modules. Missing/ambiguous module files,
`#[path]`, `#[cfg_attr]`, includes, environment macros, and macro tokens that
could generate modules are unsupported. Conditional out-of-line modules need
supported explicit cfg handling; an unopened or orphan source file does not
become a graph member just because its text was admitted. Orphans and `build.rs`
cannot be queried. All inventoried files still contribute to freshness.

The worker supplies its validated graph inline and disables sysroot discovery,
Cargo autoreload, build scripts, proc macros, and check/check-on-save commands.
It invokes neither Cargo metadata nor repository build/check commands. Repository
`rust-analyzer.toml`/`.rust-analyzer.toml` settings are rejected; private user
configuration directories and a clean environment prevent inherited toolchain,
wrapper, and user settings from selecting commands. The pinned compiler performs
fixed version/target introspection. Navigation covers the supported local graph;
standard-library/external dependency semantics, generated code, and proc-macro
expansion are outside this profile. It does not qualify a dependency-heavy Cargo
workspace such as Codex. The readiness barrier verifies admitted text, server
health and quiescence before navigation, and discards results if indexing races
with the request.

## Dedicated Linux user service

Build the broker from the same reviewed source as the Codex client:

```bash
# In your isolated source worktree:
eval "$(codex-worktree env "$PWD")"
cargo build --manifest-path codex-rs/Cargo.toml -p codex-nas-lsp --bin codex-nas-lsp
install -Dm755 "$CARGO_TARGET_DIR/debug/codex-nas-lsp" \
  "$HOME/.local/libexec/codex-nas-lsp"
```

The broker requires Linux cgroup v2 with finite `memory.max` and `pids.max` in
its **own** cgroup. Merely having finite limits on a parent or monitoring RSS
does not satisfy this check. The values must be positive and no greater than
the registry's `memory_max_bytes` and `pids_max`; otherwise navigation returns
`resource_limit_unavailable`. A dedicated systemd user service gives the broker
and its language-server descendants one bounded cgroup.

After provisioning the executable, servers, and registry, launch a dedicated
transient **user service** with absolute canonical paths:

```bash
nav_broker="$(realpath "$HOME/.local/libexec/codex-nas-lsp")"
systemd-run --user --collect --unit=codex-nas-lsp.service \
  -p Type=exec -p MemoryMax=1G -p TasksMax=64 \
  -p KillMode=control-group -p TimeoutStopSec=10 -p UMask=0077 \
  "$nav_broker" --config "$nav_dir/navigation.toml"
systemctl --user status codex-nas-lsp.service --no-pager
journalctl --user -u codex-nas-lsp.service -n 50 --no-pager
```

This uses the transient service mechanism exercised by acceptance. The attempted
`--scope` migration failed on the qualification host; use a service instead.
The startup command's success establishes launch, not successful navigation;
check the unit/log and run a supported saved-file query. Choose an unused unit
name if `codex-nas-lsp.service` already exists. Never start another broker on an
occupied endpoint. The broker owns socket locking and stale-socket recovery;
do not delete a socket to force startup.

Keep the shared `/tmp` view: `PrivateTmp=yes` would put the socket in a different
namespace from the Codex client. Stop with
`systemctl --user stop codex-nas-lsp.service`. After changing the registry, stop
and relaunch the service with the command above; there is no hot reload.

This transient unit is not enabled at boot. `/tmp` may also be cleared at boot;
keep a private persistent registry copy and provision its reserved-directory
copy before any later start. Persistent service installation and user-manager
lifetime are separate operator choices. These instructions do not activate a
service or update an installed Codex build as part of source delivery.

## Exposing the model tool

The native extension snapshots the endpoint when the Codex host initializes.
It exposes `semantic_navigation` when the default socket exists, or when
`CODEX_SEMANTIC_NAVIGATION_SOCKET` explicitly selects a socket in the reserved
directory. For deterministic registration even when the service is down, set
the environment variable before starting a new Codex process:

```bash
export CODEX_SEMANTIC_NAVIGATION_SOCKET="/tmp/codex-daemon-$(id -u)/semantic-navigation.sock"
codex-shake
```

An arbitrary socket outside that directory is rejected. There is no MCP server
entry or model-visible root/command configuration. Starting the broker later
does not add a tool to a host that initialized without an endpoint. Restart the
Codex host to change its endpoint or registration.

The model can call, for example:

```json
{"operation":"definition","path":"src/main.py","line":8,"column":12}
```

Use `"operation":"references"` for references. Lines and columns are 1-based
Unicode characters, not UTF-8 bytes or editor UTF-16 offsets; range ends are
exclusive. Each call has a 10-second native deadline and returns at most 100
locations and 16 KiB of serialized output, or a smaller host/Code Mode budget.
`truncated: true` means the location list is incomplete.

## Availability and fallback

If the service is down, an exposed tool returns a bounded `unavailable` result,
such as `unsafe_or_missing_endpoint` or `broker_unavailable`. It does not start
the daemon, install a server, retry indefinitely, or automatically run `rg`.
Ordinary file search and file reads remain available according to the session's
permissions. The agent chooses those tools when semantic navigation is absent
or unavailable, for example `rg -n 'symbol_name' src` followed by reading the
matching files. Text matches still need inspection to identify the right symbol.

| Result | Interpretation and next step |
| --- | --- |
| `ok`, no locations, `truncated: false` | The qualified server returned no matching locations in the supported project scope. |
| `indexing` or `stale` | Readiness or saved-generation validation did not complete; do not treat this as an empty answer. |
| `busy` | A query, connection, queue, or resource-pressure limit prevented admission. |
| `timeout` or `cancelled` | The operation did not complete; there is no complete location result. |
| `unavailable` | Check the reason, registry, server installation, workspace bounds, and service cgroup; use normal search/read tools as needed. |
| `invalid_request` | Check the registered-root-relative path, language extension, and 1-based position. |

For qualification and the distinction between executor tests and an interactive
model session, see the [acceptance evidence](../.codex/design/nas-lsp-acceptance.md).
