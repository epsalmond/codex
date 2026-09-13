# Checkpoint reconstruction and validation: session `01a07e54` at request 264

Script: [`scripts/reconstruct-checkpoint.ts`](../../scripts/reconstruct-checkpoint.ts).
The cutoff (request 264, the first point after a long background test run
where the orchestrator's own state was idle and no subagent request was
outstanding) was chosen by analyzing the source session's timeline against
the token-usage record at each candidate request.

## 1. What was built

| | primary | fallback |
|---|---|---|
| cutoff | request 264, ordinal 2101, `2026-09-08T01:57:36.059Z` | request 251, ordinal 1983, `2026-09-08T01:51:20.325Z` |
| output dir | `$BENCH_CHECKPOINTS/01a07e54-r264/` | `$BENCH_CHECKPOINTS/01a07e54-r251/` |
| manifest | `$BENCH_CHECKPOINTS/01a07e54-r264.manifest.json` | `…-r251.manifest.json` |
| exec tool calls in window | 403 of 498 | 380 of 498 |
| mutations applied | 64 (59 `apply_patch` + 5 `just fmt`) | 64 (59 + 5) |
| mutations skipped | 21 | 19 |
| files changed vs base | 21 | 21 |

Base commit `ceff9549eb2b78d11f8560992991d1cd4259fba1` in
`<repo>` (never written to — the tree is taken with
`git -C <repo> archive`).

Both checkpoints have **the same tree**, because the last worktree-affecting
operation before either cutoff is the `prompt_caching.rs` patch at
`01:49:53.150Z`:

```
$ git -C $BENCH_CHECKPOINTS/01a07e54-r251 rev-parse HEAD^{tree}
f44cde8e16cbb2e695d70d97e63d5e1144c1b2f7
$ git -C $BENCH_CHECKPOINTS/01a07e54-r264 rev-parse HEAD^{tree}
f44cde8e16cbb2e695d70d97e63d5e1144c1b2f7
```

Each copy has exactly one commit, one ref, and no remotes:

```
$ git -C $BENCH_CHECKPOINTS/01a07e54-r264 rev-list --count HEAD
1
$ git -C $BENCH_CHECKPOINTS/01a07e54-r264 for-each-ref --format='%(refname)'
refs/heads/checkpoint
$ git -C $BENCH_CHECKPOINTS/01a07e54-r264 remote
(empty)
```

### Rollout set

The orchestrator rollout plus the four subagent rollouts it spawned. Mapping
verified from each rollout's `session_meta` payload rather than assumed:

| rollout | `parent_thread_id` | `agent_path` | `cwd` | exec calls |
|---|---|---|---|---|
| `…17-04-21-01a07e54-…` | (none, `thread_source: user`) | — | harness-repo | 195 |
| `…17-07-37-01a07e57-…` | `01a07e54` | `/root/shake_investigation` | harness-repo | 48 |
| `…17-21-12-01a07e63-…` | `01a07e54` | `/root/artifact_implementation` | harness-repo | 194 |
| `…17-53-16-01a07e81-…` | `01a07e54` | `/root/artifact_review_fixes` | harness-repo | 43 |
| `…17-54-17-01a07e82-…` | `01a07e54` | `/root/artifact_delete_lifecycle` | harness-repo | 18 |

Seven other rollouts in `$CODEX_HOME/sessions/2026/09/07/` from the same hour
have different parents belonging to unrelated, separate repos and are
correctly excluded. No rollouts exist after `T17-54` on 2026-09-07 until
`T23-12`, so no subagent was spawned later in the session.

### apply_patch provenance

`apply_patch` is the real in-product implementation, reached through codex's
arg0 dispatch (a symlink named `apply_patch` pointing at the codex binary):

```
<harness-repo-install>/releases/0.154.0-b27a53a7e01f-b8997ff68753/codex
codex-cli 0.154.0
```

The session itself ran `cli_version 0.153.2`. The patch format and applier are
unchanged between those builds; no hunk in this replay depended on version
behaviour (all 59 applied cleanly, see §3).

## 2. Two parser findings that mattered

Both were caught because the replay *failed loudly* rather than silently
producing a different tree.

1. **`String.raw` templates must not be unescaped.** Later patches in this
   session pass the patch body as ``String.raw`*** Begin Patch…` `` so that a
   Rust source line containing `\n` survives. Cooking that escape turns it into
   a real line break, which strips the `+` prefix from the following patch line
   and makes apply_patch reject the hunk. Before the fix: 19 replay failures.
   After: 0.
2. **`just fmt` has to be replayed.** The session ran `rtk just fmt` five times
   before the cutoff (`00:27:08`, `00:27:41`, `00:48:21`, `01:00:08`,
   `01:01:52`). The repo formats with `rustfmt --config imports_granularity=Item`,
   so later patches expect imports already split one-per-line; skipping fmt made
   subsequent context matches fail. `scripts/format.py` also shells out to git,
   so the replay keeps a scratch git repo in the output dir during replay and
   destroys it in step (d).

`just fmt` is also why the checkpoint touches two files no transcript message
names — `codex-rs/app-server/src/request_processors.rs` and
`codex-rs/tui/src/app_server_session.rs` (4 lines each). Both appear with the
same content in the session's own commit `942d94123f`, which is independent
confirmation that the fmt replay reproduced what the session did.

## 3. Validation

### 3.1 Checkpoint file set is a subset of the final file set

Validation repo: base tree, checkpoint tree and `61ec1752dd` tree committed in
sequence into a scratch repo, so all three are diffable locally.

```
$ git diff --shortstat base checkpoint
 21 files changed, 984 insertions(+), 36 deletions(-)
$ git diff --shortstat base final
 36 files changed, 1364 insertions(+), 47 deletions(-)
$ comm -23 <(git diff --name-only base checkpoint|sort) <(git diff --name-only base final|sort)
(empty)
```

**Every one of the 21 checkpoint-modified files is also in the 36-file final
set.** No checkpoint-only file exists.

The 15 files in the final set but not the checkpoint are exactly the work
identified as still open at request 251/264:

- app-server schema regeneration output (11 files: `ClientRequest.json`,
  both `codex_app_server_protocol*.schemas.json`, `v2/ThreadShakeStart*.{json,ts}`,
  `typescript/ClientRequest.ts`, `typescript/v2/index.ts`, two precomputed `.zst`),
- the two `insta` snapshots (`shake_notice_renders_summary.snap`,
  `ephemeral_shake_notice.snap`),
- `codex-rs/core/tests/suite/openai_file_mcp.rs` and
  `codex-rs/tui/src/status/rate_limits.rs` (1-line fixups landed later).

### 3.2 Is checkpoint content a plausible ancestor of final content?

For each changed file, every line the checkpoint adds over base was checked
against the final version of that file (whitespace-normalised):

```
file                                                       added  survive  lost
codex-rs/core/src/shake.rs                                    99       99     0
codex-rs/core/src/shake/recovery.rs                           52       52     0
codex-rs/thread-store/src/local/delete_thread.rs              92       92     0
codex-rs/core/src/session/handlers.rs                         11       11     0
codex-rs/tui/src/chatwidget/tests/mcp_startup.rs               6        6     0
codex-rs/core/src/artifacts.rs                               192      177    15
codex-rs/core/src/artifacts_tests.rs                          92       85     7
codex-rs/core/tests/suite/artifact_recovery.rs               245      238     7
codex-rs/core/src/tools/handlers/read_artifact_spec.rs        32       30     2
codex-rs/core/src/tools/handlers/read_artifact.rs             53       52     1
codex-rs/core/src/tools/handlers/mod.rs                        3        2     1
codex-rs/core/src/session/mod.rs                              17       16     1
codex-rs/app-server/README.md                                  9        8     1
(11 further files: 0 lost)
TOTAL added 917, lost 35 (3.8%)
```

**No file looks mis-replayed.** A mis-replay shows up as garbled structure —
lost `+` prefixes, duplicated blocks, orphaned hunks. Every one of the 35 lost
lines is instead a later, deliberate edit of code the checkpoint already had in
recognisable form:

- `artifacts.rs` / `artifacts_tests.rs` (22 lines): the UTF-8 byte-offset paging
  block was reworked after the cutoff. The API is identical in both trees
  (`pub(crate) fn read(&self, uri: &str, start_byte: Option<u64>)` at line 64 of
  both) and the paging message is byte-identical
  (`"{output}\n[artifact source: {uri}; more content; use start_byte={next_offset}]"`);
  the whole delta is inside the boundary arithmetic:

  ```
  $ git diff checkpoint final -- codex-rs/core/src/artifacts.rs
  -        let leading_continuation = bytes
  -            .iter()
  -            .position(|byte| (*byte & 0b1100_0000) != 0b1000_0000)
  -            .unwrap_or(bytes.len());
  +        if offset > 0 && (bytes[0] & 0b1100_0000) == 0b1000_0000 {
  +            return Err(format!(
  +                "artifact {uri} start_byte {offset} is not a UTF-8 character boundary"
  +            ));
  +        }
  ```

  That is precisely the change identified as prepared
  at request 317 in a later, out-of-repo scratch patch — i.e. the
  checkpoint correctly holds the *pre*-hardening variant, which is the whole
  point of cutting at request 264.
- `artifact_recovery.rs` (7 lines): future-boxing and fixture-size tweaks made in
  the post-cutoff retry rounds.
- `read_artifact.rs` / `read_artifact_spec.rs` / `handlers/mod.rs` (4 lines):
  visibility tightening, e.g. checkpoint `pub struct ReadArtifactHandler;` →
  final `pub(crate) struct ReadArtifactHandler;`.
- `session/mod.rs`, `app-server/README.md` (2 lines): one reworded README bullet
  and one refactored `if let` for the fork-copy path.

### 3.3 The ~17 files identified as touched by request 251

All 18 repo files identified as touched by request 251 are present in
the replay, and nothing expected is missing:

| predicted file | in replay |
|---|---|
| `core/src/artifacts.rs` | yes |
| `core/src/artifacts_tests.rs` | yes |
| `core/src/tools/handlers/read_artifact.rs` | yes |
| `core/src/tools/handlers/read_artifact_spec.rs` | yes |
| `core/src/tools/handlers/mod.rs` | yes |
| `core/src/tools/spec_plan.rs` | yes |
| `core/src/lib.rs` | yes |
| `core/src/session/mod.rs` | yes |
| `thread-store/src/local/delete_thread.rs` | yes |
| `core/src/shake.rs` | yes |
| `core/src/shake/recovery.rs` | yes |
| `core/tests/suite/artifact_recovery.rs` | yes |
| `core/tests/suite/mod.rs` | yes |
| `core/tests/suite/prompt_caching.rs` | yes |
| `app-server/README.md` | yes |
| `tui/src/chatwidget/tests/mcp_startup.rs` | yes |
| `justfile` | yes |
| `app-server-protocol/scripts/write_schema_fixtures.py` | yes |

Three files the replay produced that the report did not list:
`core/src/session/handlers.rs` (patched at `00:27:03.231Z`; the report only
noted that the *final* version of this file contains post-251 ephemeral logic),
plus the two `just fmt` byproducts from §2.

### 3.4 Idempotency

`--out` is wiped and rebuilt on entry. Rebuilding the r264 checkpoint into a
second directory produced the same tree object as the first build (see §5).

## 4. Manual review: what was deliberately not replayed

### r264 (5 items)

| timestamp | what | why skipped |
|---|---|---|
| `01:53:33.323Z` | `rtk proxy cargo insta accept --snapshot tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__shake_notice_renders_summary.snap` | accepts a `.snap.new` produced by an 8,723-test nextest run; not reproducible without re-running that suite, and fabricating the snapshot would be a guess |
| `01:00:27.791Z` | `apply_patch` add `~/.bin/tmp/codex-artifacts-validation.sh` | scratch script outside the repo |
| `01:24:00.438Z` | `apply_patch` add `~/.bin/tmp/codex-junit-failures.py` | ditto |
| `01:43:08.608Z` | `apply_patch` update `~/.bin/tmp/codex-artifacts-validation.sh` | ditto |
| `01:53:43.866Z` | `apply_patch` update `~/.bin/tmp/codex-junit-failures.py` | ditto |

r251 has only the three `.bin/tmp` items — the `insta accept` is after that
cutoff, so **the r251 checkpoint has no known content gap at all**, while the
r264 checkpoint is missing exactly one file relative to the true r264 worktree:
`codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__shake_notice_renders_summary.snap`.

### Skipped because the session's own call failed

The other 16 (r264) / 16 (r251) skips are `apply_patch` calls whose code-mode
script reported `Script failed` in the transcript — `apply_patch verification`
errors and one JS `SyntaxError`. apply_patch is all-or-nothing, so those calls
never modified the real worktree either; replaying them would *introduce*
divergence.

### Not present in this window

No `sed -i`, no `cat >` / redirect writes, no `git apply` / `git checkout` /
`git restore`, and no non-empty `write_stdin` payloads occur at or before either
cutoff. The only worktree-writing shell commands in the window are the five
`just fmt` runs and the one `cargo insta accept`.

## 5. Compile state

### 5.1 What the session's own transcript says

The checkpoint should not be expected to be green just because it compiles —
the question is what state the session was in. Orchestrator messages
(rollout `01a07e54`, own `agent_message` records):

- `01:40:22.889Z` — the orchestrator reported that the combined build had
  passed and the 8,723-test suite was starting.
- `01:53:22.955Z` — the orchestrator reported that the broad run had finished
  with failures.
- `01:57:07.859Z` (`shake_investigation` FINAL_ANSWER, the message that defines
  cutoff 264) — totals reported: 8,723 tests; 8,459 passed, 264 failed, 0
  skipped, 0 errors; `suite::artifact_recovery::shake_saves_and_reads_artifact_after_resume`
  passed in 7.320s.
- `01:57:55.034Z` — the failure review found missing-helper errors, six
  tool-list expectation failures, and unrelated UI snapshot drift.

So the tree **did compile** at the cutoff: the workspace built at `01:40`, and
the 264 failures were runtime failures (missing `test_stdio_server` /
`codex-code-mode-host` binaries, tool-list expectations, snapshot drift), not
compile errors.

One caveat: the last worktree mutation before the cutoff is the
`prompt_caching.rs` expectation patch at `01:49:53.150Z`, which lands *after*
the `01:40` build. At the cutoff nothing had recompiled that file yet — the
session itself did not know whether the checkpoint tree compiled. That is a
property of the real session state, not of the reconstruction.

### 5.2 `cargo check` on the checkpoint

There is no `check` recipe in the repo's `justfile` (it has `clippy`, `fmt`,
`test`), so the workspace equivalent was run directly on the four crates the
checkpoint changes, with tests included:

```bash
cd $BENCH_CHECKPOINTS/01a07e54-r264/codex-rs
RUST_MIN_STACK=8388608 CARGO_TARGET_DIR=~/.cache/shake-bench-ckpt-target \
  cargo check --all-targets \
  -p codex-core -p codex-tui -p codex-thread-store -p codex-app-server
```

**Result: PASS.**

```
warning: unused import: `wiremock::matchers::body_json`
  --> core/tests/suite/openai_file_mcp.rs:47:5
warning: `codex-core` (test "all") generated 1 warning
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 8m 35s
EXIT=0
```

Zero errors; one warning. That warning is itself corroborating evidence rather
than a defect of the reconstruction:

- the orchestrator reported it live at `01:38:10.056Z`, noting that the
  remaining diagnostic so far was an unused import in an unrelated existing
  test;
- `codex-rs/core/tests/suite/openai_file_mcp.rs` appears in the final commit
  range with exactly `1 -` (one deleted line) — the session removed that import
  *after* the cutoff, which is why the checkpoint still carries it.

Note that running cargo inside the checkpoint dirties `codex-rs/Cargo.lock`;
it was restored with `git -C <dir> checkout -- .` afterwards, and the checkpoint
working tree is clean again.

## 6. Reproducing

```bash
cd shake-bench

# primary checkpoint (request 264)
tsx scripts/reconstruct-checkpoint.ts \
  --base ceff9549eb2b78d11f8560992991d1cd4259fba1 \
  --cutoff 2026-09-08T01:57:36.059Z \
  --out $BENCH_CHECKPOINTS/01a07e54-r264 \
  --label "checkpoint 01a07e54@r264"

# fallback checkpoint (request 251, literal 75%)
tsx scripts/reconstruct-checkpoint.ts \
  --base ceff9549eb2b78d11f8560992991d1cd4259fba1 \
  --cutoff 2026-09-08T01:51:20.325Z \
  --out $BENCH_CHECKPOINTS/01a07e54-r251 \
  --label "checkpoint 01a07e54@r251"
```

Notes for whoever consumes these checkpoints:

- The copies carry a gitignored `.ruff_cache/` produced by the replayed
  `just fmt`, exactly as the real worktree did. It is not in the commit.
- Running `cargo`/`just` inside a checkpoint dirties its working tree
  (`Cargo.lock`, `codex-rs/target/`). `git -C <dir> stash` or a fresh rebuild
  restores the checkpoint state; the commit itself is never touched.

## 7. Residual doubts about fidelity

1. **The accepted snapshot is missing from r264.** See §4. Use r251 if that
   matters; the trees are otherwise identical.
2. **Timestamps are the only ordering signal across rollouts.** Orchestrator and
   subagent rollouts are merged by `timestamp`, ties broken by `ordinal`. Two
   agents patching the same file within the same millisecond would be ordered
   arbitrarily; no such collision occurs in this window (no two `apply_patch`
   calls share a timestamp).
3. **apply_patch is 0.154.0, the session ran 0.153.2.** All 59 patches applied
   cleanly, so no version-dependent fuzzy matching was exercised, but this is
   not a formal guarantee of identical behaviour.
4. **`just fmt` is replayed with today's toolchain** (rustfmt 1.95.0 via
   `rustup`), not necessarily the exact rustfmt the session used. A rustfmt
   version difference would show up as spurious reformatting across the whole
   workspace; `git diff base checkpoint` touches only 21 files, all of them
   files the session itself changed, so this did not happen.
5. **Untracked scratch files are out of scope.** The two `~/.bin/tmp/`
   scripts the session wrote are not part of the repo and are not reconstructed.
