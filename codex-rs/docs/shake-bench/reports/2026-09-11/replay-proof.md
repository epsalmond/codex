# Full-context replay proof: session `01a07e54` from checkpoint r264

Phase 2 of the real-session benchmark, and the go/no-go the previous session
asked for: proof that a full-context replay can finish before implementing or
running the comparative real-session benchmark.

Everything below is a measurement or a quoted artefact. Dollar figures are
hypothetical API arithmetic over [`pricing.json`](../../pricing.json), matching
[COST_ANALYSIS.md](../../COST_ANALYSIS.md); they are not ChatGPT quota claims.

## 1. Setup

| | |
|---|---|
| session | `01a07e54-7c97-7541-b618-af295f393780`, orchestrator + 4 `spawn_agent` subagents |
| cutoff | request 264, ordinal 2101, `2026-09-08T01:57:36.059Z` |
| checkpoint | `$BENCH_CHECKPOINTS/01a07e54-r264`, never written to — every run exports it with `git archive` into a fresh single-commit copy |
| reference end state | `61ec1752dd` in `codex-artifacts`, exported read-only the same way |
| worker binary | `$HOME/.local/bin/codex-next` → `$HOME/.local/share/codex-next/local-features-v0.153.4-.../codex`, **`codex-cli 0.153.4`** |
| worker model | `gpt-6-astra`, effort `medium` — the model and effort the original orchestrator ran (`turn_context.payload.model` / `.effort`; the session itself ran cli 0.153.2) |
| simulated user | Claude Sonnet, `claude -p --model sonnet --output-format json --system-prompt-file`, in an empty scratch cwd |
| isolation | private `CODEX_HOME` (only `auth.json` copied), `approvalPolicy: never`, `sandbox: danger-full-access`, `[agents] enabled = false` |

The worker binary is **not** the repo under edit: `codex-next` is built from
`the harness repo`'s `a local-features branch`; the candidate tree is a copy of the
`codex-artifacts` checkpoint.

`[agents] enabled = false` is the flag that matters. `[features] multi_agent =
false` does not work: `gpt-6-astra`'s model catalog declares multi-agent v2 and
`Config::multi_agent_version_for_model` prefers the model's declaration over the
`Collab` feature flag. With only the feature flag set, the first replay spawned
subagents.

## 2. Flattened history

[`scripts/flatten-history.ts`](../../scripts/flatten-history.ts) merges the
orchestrator's rollout and the four subagent rollouts into one timeline ordered
by wall-clock timestamp, drops the multi-agent plumbing (`spawn_agent`,
`send_message`, `followup_task`, `sleep`, `interrupt_agent`, and the subagents'
`agent_message` reports), and emits items in the shape `src/convert.ts` already
validates and `src/run.ts` already injects with `thread/inject_items`. No new
history format. Each tool call is emitted immediately followed by its result so
the call/result pairing stays valid however two agents' timestamps interleave.

| variant | items | bytes | tokens |
|---|---:|---:|---:|
| **flattened** (all five rollouts) | 949 | 3,382,594 | **879,775 – 880,044** |
| **orchestrator-only** (secondary) | 382 | 655,368 | **178,309** |

A third variant, the Tier-B **trimmed** flattened history (949 items, 2,622,839
bytes, **690,079** tokens), was built later to get the first request under the
auto-compaction trigger; see §8.1.

Tokens are the product's own count: inject the history, then read
`preview.tokensBefore` from `thread/shake/preview` mode `elide` — the same call
`src/arms.ts` uses. (The same preview reports that an elide would leave
**183,171** tokens, a 4.8x reduction, which is the number the comparative arms
will be about.) Counts vary by a few hundred tokens between runs because the
injected payload is identical but the thread's own preamble is not.

Composition of the flattened history: 3 user messages, 138 assistant messages
(85 orchestrator + 53 subagent), 404 tool calls with their results (403 `exec`
+ 1 `wait`). Dropped: 597 encrypted reasoning items, 123 plumbing calls, 53
`agent_message` payloads (empty — Fernet-encrypted in the rollout), 21 developer
messages.

**880k tokens is the headline number.** It is 3.2x `gpt-6-astra`'s default
272,000-token context window and just above the 872,000 the local model catalog
gives as `max_context_window`. The API accepted the request (measured: one
request with `input_tokens: 864,854`, `cached_input_tokens: 0`) and the product
then **auto-compacted immediately** — every replay shows a `compacted` record in
the worker's rollout within the first turn, after which context drops to
~21,000 tokens. So a "full-context" arm over a flattened multi-agent session is
not actually a full-context arm: the product converts it into a compaction arm
on the first turn, at a one-off cost of ~$17.6 for that first request.

### Redaction is not optional

The unredacted history names `<repo>` 1,006 times and
`<harness-repo>` 129 times, in apply_patch bodies, shell
commands and subagent reports. **Both checkouts still hold the finished branch**
(`the harness repo` has `the fork's artifacts branch` at `61ec1752dd`). In the first replay
the worker ran `ls <repo>` during its first turn — the
leak is not hypothetical. `--redact from=to` rewrites both paths and both bare
repo names; after redaction the history contains zero references to either
checkout, to `bench-checkpoints`, to `shake-bench`, or to any final commit hash,
and the worker rollouts of the later runs contain none either.

## 3. Simulated user

**Superseded from run 8 onward by the frozen script in §11.2** — the live driver
below is `--live-user`, off by default. What follows describes runs 1-7.

[`fixtures/simulated-user-01a07e54.md`](../../fixtures/simulated-user-01a07e54.md).
Driven with Claude Sonnet through the `claude` CLI in print mode; the spec file
replaces the default system prompt and the CLI runs in an empty scratch cwd with
no access to the repository, the final diff, or the acceptance script. There is
no existing model-calling path in the harness for a second model, so this is the
minimal one.

The brief contains only (a) the operator's own words and (b) repository conventions.
**The operator said nothing after the cutoff** — the only two messages in the
whole session are at ordinals 5 and 138, both long before request 264 — so the driver
re-states the standing goal and holds the worker to seven requirements phrased
as requirements, never as solutions. Turn limit: 8 exchanges. Stop conditions:
`STOP: DONE`, `STOP: BLOCKED`, `STOP: STUCK`, or the turn limit.

## 4. Stubbed build toolchain

Three concurrent real workspace builds took the host down, so from run 4 onward
the worker gets [`fixtures/stubs/`](../../fixtures/stubs/) first on its `PATH`:
`cargo`, `cargo-nextest`, `just`, `rustup`. Canned stdout and exit codes are
taken from the original session's own tool outputs for the same command shape;
anything unrecognised falls back to a generic success. Every invocation is
appended to `<out>/stub-invocations.log`.

| command shape | stubbed behaviour |
|---|---|
| `cargo check\|build\|clippy` | `Finished \`dev\` profile ... in 0.03s`, exit 0 |
| `cargo fmt` | exit 0, no output |
| `cargo test`, `cargo nextest`, `cargo-nextest`, `just test` | nextest tail: *31 tests run: 31 passed, 0 skipped* — the original session's own focused-run count |
| `cargo insta pending-snapshots` | real `find` for `*.snap.new` (a filesystem question, not a build) |
| `cargo insta accept` / `reject` | real rename / delete of `*.snap.new` |
| `cargo install` | "already installed", exit 0 |
| `cargo run` | exit 1, "not available in this environment" |
| `just fmt` / `fmt-check` / `clippy` / `fix` | success |
| `just write-app-server-schema` | **exit 1** with "the generator needs a workspace build … update the vendored files under app-server-protocol/schema/ directly" — this one genuinely cannot be faked without handing over the answer |
| `rustup` | prints the toolchain triple |

**PATH stubs alone are not sufficient.** Run 4's worker reached
`$HOME/.cargo/bin/just` by absolute path and shelled out to
`an internal run-subagent script from an unrelated private repo --harness codex --model
gpt-5.6-luna`, which started a real codex process with an unstubbed PATH and a
real `cargo nextest` in the candidate tree. Two backstops were added:
[`scripts/bench-watchdog.sh`](../../scripts/bench-watchdog.sh), which kills any
`cargo`/`rustc`/`nextest`/`run-subagent` process touching `$BENCH_REPLAY`,
and a `developerInstructions` block on `thread/start` telling the worker the
toolchain is a stand-in, to use it normally, and not to reach past PATH or start
another agent. The watchdog recorded no kills in run 6.

Stubbed commands run 6's worker actually invoked (all of them, verbatim):

```
just test -p codex-core -p codex-thread-store -p codex-app-server-protocol -p codex-tui
just test -p codex-core -p codex-thread-store -p codex-app-server-protocol -p codex-tui \
     -E 'test(artifact) | test(shake) | test(prompt_caching) | test(delete_thread) | test(schema_fixtures)'
just fix  -p codex-core -p codex-thread-store -p codex-app-server-protocol -p codex-tui
just fmt
```

Earlier, unstubbed runs additionally invoked `just clippy`, `cargo insta
pending-snapshots`, `cargo insta accept --snapshot <path>`, `cargo insta
reject`, `just write-app-server-schema [--experimental]`, and
`cargo nextest run … -E 'test(artifact_recovery) | test(artifacts::tests) | test(shake)'`
— which is why those shapes are in the table.

## 5. Acceptance

[`scripts/accept-01a07e54.sh`](../../scripts/accept-01a07e54.sh), run inside a
candidate tree. **Build-free by design**, for the same reason the toolchain is
stubbed: 23 structural and content assertions, each mapping to a requirement in
the simulated-user brief. The worker never sees it.

- **A1–A7** the app-server protocol surface is vendored (4 schema files exist,
  `thread/shake/start` in `ClientRequest.json`, both types re-exported from the
  v2 index, the README entry).
- **B1–B3** both shake-notice snapshots are committed and nothing is left pending.
- **C1–C2** `read_artifact` registration in `spec_plan.rs` is behind a
  conditional keyed on ephemeral/system threads (implementation-agnostic: the
  guard may key on anything, it just may not be unconditional), and some test
  covers an ephemeral tool-free case.
- **D1–D3** artifact reads reject mid-character byte offsets, paging still
  advertises a continuation offset, tests cover a non-boundary offset.
- **E1–E2** the unused `wiremock::matchers::body_json` import is gone and
  `rate_limits.rs` no longer clones a `Copy` timestamp — the two one-line fixups
  the session made after the cutoff, and the source of the only `cargo check`
  warning the checkpoint carries.
- **F1–F6** nothing from the checkpoint regressed.
- **G1** `cargo fmt --check` is available behind `ACCEPT_FMT=1` (rustfmt only,
  no build) and skipped by default.

| tree | verdict | failing checks |
|---|---|---:|
| reference `61ec1752dd` | **ACCEPTED** | 0 |
| untouched checkpoint r264 | **REJECTED** | 12 |
| replay run 6 candidate | REJECTED | 5 |
| replay run 7 candidate (trimmed, sandboxed) | REJECTED | 12 |
| replay run 8 candidate (Tier C, scripted user) | REJECTED | **2** (§11.6 — both are naming mismatches over work that is present) |
| replay run 3 after exchange 1 | REJECTED | 5 |
| replay run 3 after exchange 2 | REJECTED | 4 |

The checkpoint's 12 failures are A1–A6, B1, B2, C1, D1, E1, E2 — exactly the
work [checkpoint-validation.md](checkpoint-validation.md) §3.1 predicted was
still open at the cutoff. The script therefore discriminates: it passes the real
answer and rejects the starting point.

An earlier, build-based version of this script (cargo check with zero warnings,
the 30-test focused nextest set, schema regen, fmt-check, scoped clippy,
snapshot presence) was also run before the no-build constraint landed. It
produced the same verdicts — reference **ACCEPTED** 8/8, checkpoint **REJECTED**
8/8, and it confirmed the focused set is **exactly 30 passing tests**, matching
the session's own "All 30 focused tests … passed". Those runs are what took the
machine down and are not repeatable here.

## 6. Replay runs

Six replays were started. Four were aborted for harness or environment reasons;
their outcomes are reported because they are the findings.

| run | history | outcome |
|---|---|---|
| 1 | flattened, **unredacted** | **Aborted.** The worker ran `ls <repo>` in exchange 1. Result invalid; the leak is why `--redact` exists. First request measured at 864,854 input tokens, 0 cached, followed immediately by auto-compaction. |
| 2 | flattened, redacted | **Aborted** by a harness defect: the worker turn ran past the then-1h timeout during exchange 2. Per-turn token totals were lost because the run also deleted its `CODEX_HOME` on exit. Both defects are fixed (4h default, `--turn-timeout-ms`, and the rollout is now preserved). Its tree reached **6 failing checks** under the old build-based script. |
| 3 | flattened, redacted | **Aborted** — real builds, killed when the host went down. Reached 3 exchanges. Exchange 1 alone took **4,203 s**. Its trees are the two run-3 rows above. |
| 4 | flattened, redacted, stubs | **Aborted.** The worker bypassed the PATH stubs by absolute path and by spawning a real `run-subagent`. Led to the watchdog and the developer-instructions block. |
| 5 | flattened, redacted, stubs + watchdog | **Aborted.** The first developer-instructions wording ("do not treat stub output as validation") made the worker refuse to do anything and argue about missing tools for five exchanges; the simulated user made it worse by insisting a delegation tool existed. Both were fixed: the instructions now say to use the stand-ins normally, and the operator brief now says to believe the worker when it reports a missing capability and never to name a tool. |
| **6** | flattened, redacted, stubs + watchdog + fixed instructions | **Completed**, `STOP: BLOCKED` at exchange 4. |
| **7** | flattened, redacted, **Tier B trimmed**, stubs + bubblewrap jail + watchdog | **Completed**, `STOP: DONE` at exchange 4. First uncompacted full-context request; acceptance still REJECTED at 12. See §8. |

### Run 6, the reported replay

| | |
|---|---|
| history | flattened, redacted — 949 items, 3,382,594 bytes, **880,020 tokens** |
| exchanges | 4 (3 worker turns completed; the 4th scripted-user message ended the run) |
| stop reason | `STOP: BLOCKED` — the worker could not run a real toolchain and said so |
| wall clock | worker turns **612 s** total (580 s + 15 s + 17 s) |
| worker tokens | **2,366,839 input** (1,368,704 cached), **12,868 output**, 30 model requests |
| simulated-user tokens | 8 input, 66,776 cache read, 68,507 cache write, 328 output — **$0.296** (reported by the CLI) |
| worker cost | **≈ $21** |
| acceptance | **REJECTED**, 5 failing checks |

Worker cost method: uncached input is 2,366,839 − 1,368,704 = 998,135 tokens.
The first request alone is ~880,000 of those and is above the 272,000 threshold,
so it is billed at 2x: ≈ $17.6. The remaining ~118,000 uncached input at $10/M
is ≈ $1.2, 1,368,704 cached at $1/M is ≈ $1.4 (almost all of it below the
threshold, post-compaction), and 12,868 output at $50/M is ≈ $0.6. Bounds if
the tiering is assumed uniformly: $12.0 (no multiplier anywhere) to $23.7 (2x/1.5x
everywhere). **The simulated user costs 1.4% of the worker.**

What run 6's worker actually produced, in three commits on top of the
checkpoint: the complete app-server schema surface written by hand (all four
`ThreadShakeStart*` files, the `ClientRequest.json` entry, the v2 index
re-exports, the precomputed exports), the `shake_notice_renders_summary`
snapshot, and artifact read/fork-copy changes. What it did not do: the ephemeral
`read_artifact` exclusion, the ephemeral snapshot, the UTF-8 boundary rejection,
and the two one-line fixups.

Run 3, which had a real toolchain, got further on the behaviour: in its second
exchange it **independently found and fixed the ephemeral leak** — commit
*"Keep structured system helper threads free of artifact tools"*, guarding
`registry.add(ReadArtifactHandler)` on `is_system_thread()` where the original
session guarded it on `config.ephemeral`. Different implementation, same
behaviour; the acceptance script's C1 accepts either. It still shipped no test
for it, which is why C2 and B2 stayed red.

## 7. Go / no-go (phase 2; revised in §8.6, then §11.8)

**No-go on the comparative matrix as currently designed.** The feasibility
question — *can a fresh worker given the flattened history plus the tree finish
the remaining work?* — comes back **"partly, and not within any run we could
afford."** Best replay: 5 of 12 checkpoint failures closed under stubs, 8 of 12
closed with a real toolchain. No replay reached acceptance.

Concrete blockers, each with what would unblock it:

1. **The flattened history does not fit the context window, and the product
   knows it.** 880k tokens against a 272k window and an 872k ceiling. The
   product auto-compacts on the first turn, so the "full-context" control arm
   silently becomes a compaction arm and costs ~$17.6 in long-context billing
   to do it. *Unblock:* either cut the arm at a history that fits (the
   orchestrator-only variant is 178k and fits, but it is a different experiment),
   or accept that the control is "full history, auto-compacted once" and say so
   in the arm definition. Do not call it full-context.

   > **CORRECTED — see §11.9.** This blocker named the right number and drew the
   > wrong conclusion. "272k window" is exactly what the product resolved, and
   > §8.4 then lost it by re-anchoring on the 872k ceiling. What nobody checked
   > was whether 272k was a *property of the model* or a *default the worker
   > config had declined to override*. It is the latter: one line,
   > `model_context_window = 872000`, moves the auto-compact limit from 244,800
   > to 784,800. Neither of the two "unblocks" offered here was necessary.

2. **A single exchange is a 10–70 minute wall-clock unit with a real
   toolchain.** Run 3's first exchange: 4,203 s. Eight exchanges times five arms
   times any repetition is days, and three concurrent builds take the host down.
   *Unblock:* the stub toolchain makes an exchange ~580 s instead (run 6), but
   see blocker 3.

3. **Stubbing the toolchain changes what the task is.** With stubs the worker
   cannot discover the ephemeral leak by running the suite the way the original
   session did (it found it in a retry run), cannot regenerate schemas, and
   cannot produce snapshots from test output. Run 3 (real builds) found the
   ephemeral bug; run 6 (stubs) did not. Any arm comparison run under stubs is
   comparing context strategies on a *reduced* task. *Unblock:* pre-bake the
   build artefacts — one real `cargo build --tests` into a shared
   `CARGO_TARGET_DIR` before the matrix, then let stubbed `just test` replay
   recorded nextest output per filter expression. That is a real piece of work,
   not a flag.

4. **The worker escapes the harness.** It reached the real toolchain by absolute
   path and spawned a real agent through the machine's own `run-subagent`
   script, outside every token count the harness keeps. The watchdog and the
   developer-instructions block contain it, but both are mitigations, not
   isolation. *Unblock:* run the worker in a container or a user namespace where
   the real toolchain and the operator's other private repo scripts are not on
   the filesystem at all.

5. **The simulated user is a live variable, not a fixture.** Across runs it
   invented a delegation tool that did not exist, spent three exchanges on an
   environment argument, and its wording changed the worker's behaviour more
   than the history variant did. *Unblock:* freeze the driver — record one
   accepted transcript of operator messages per arm and replay it verbatim, so the
   arms differ only in history. The live driver becomes a one-off used to
   produce that script.

6. **Acceptance is now structural, not behavioural.** The build-free script
   checks that the right code and artefacts are present, not that they work.
   It discriminates correctly at both ends (reference 0 failures, checkpoint 12)
   and it is the right tool under the no-build constraint, but a candidate that
   writes plausible-looking code that does not compile would pass A–F. *Unblock:*
   pair it with one real `cargo check` per *arm* (not per candidate), run
   serially, `nice -n 19`, `CARGO_BUILD_JOBS=2`.

What is ready and reusable regardless: the flattened-history builder with
redaction, the token measurement through the product's own counter, the
simulated-user brief, the stub toolchain, the watchdog, and an acceptance script
with verified discrimination at both ends.

## 8. Trimmed rerun (run 7)

The three things §7's blockers 1 and 4 asked for, built and measured: a trimmed
history that fits, a jail the worker cannot walk out of, and one full replay
through both.

### 8.1 History totals, before and after trim

[`scripts/flatten-history.ts`](../../scripts/flatten-history.ts) grew a
`--trim <spec.json>` flag. The spec is a list of `{index, keepHead, keepTail}`
over the flattened variant's own 0-based item indexes — the same indexes
[flattened-trim-candidates.md](flattened-trim-candidates.md) cites — with
`keepHead`/`keepTail` in tokens-equivalent (defaults 800/800) converted at the
same `bytes/3.844` proxy [`scripts/history-sizes.ts`](../../scripts/history-sizes.ts)
uses. Only `function_call_output` items may be trimmed; a listed index that is
out of range or is any other item type **throws**, it is not silently skipped.
The kept head and tail are separated by

```
[... N bytes elided by benchmark trim; rerun the command to regenerate ...]
```

so the marker tells a reader what to do about it rather than merely noting a
gap. Trimming applies to the flattened variant only — the orchestrator-only
variant has different indexes and is emitted untrimmed. Without `--trim` the
output is byte-identical to the committed untrimmed fixture, checked by diff.

The two tier specs from the trim report are committed as
[`fixtures/trim-01a07e54-tierA.json`](../../fixtures/trim-01a07e54-tierA.json)
(13 indexes) and
[`fixtures/trim-01a07e54-tierB.json`](../../fixtures/trim-01a07e54-tierB.json)
(22). Both untrimmed and Tier-B histories are kept side by side.

| history | items | bytes | tokens (product counter) |
|---|---:|---:|---:|
| flattened, redacted, **untrimmed** | 949 | 3,382,594 | **880,020** |
| flattened, redacted, **Tier B trimmed** | 949 | 2,622,839 | **690,079** |

All 22 requested items were trimmed, none skipped; each lost between 30,929 and
34,008 bytes, 759,559 bytes elided in total (pre-redaction payload 3,380,352 →
2,620,793). Item count is unchanged — trimming truncates, it never deletes, so
the call/result pairing invariant is untouched. Redaction still holds: zero
occurrences of either checkout path, `bench-checkpoints`, `shake-bench`, or the
final commit hash in the injected items.

**The trim undershot its own target.** The trim report predicted 653,911
tokens for Tier B; the product's counter says **690,079**, 5.5% high. The
report's estimate came from per-item `tokensEst` values rescaled so the
untrimmed total matched 880,020 exactly; the bytes it assumed would survive per
item were optimistic, and the 800+800-token head/tail budget plus the marker is
additive on top. The lesson is that the `bytes/3.844` proxy is calibrated for
the *whole* payload and is not reliable per-span — which matters below.

### 8.2 Sandbox

[`scripts/bench-sandbox.sh`](../../scripts/bench-sandbox.sh), a bubblewrap
(`bwrap` 0.9.0, unprivileged) jail. `replay-01a07e54.ts` now sandboxes by
default; `--no-sandbox` is an explicit opt-out for the old direct-spawn path.
Because `AppServerClient`'s child environment is an allowlist, sandbox
parameters cannot ride in on env vars, so the replay generates a per-run
launcher at `<out>/sandbox-exec.sh` with the paths baked in and spawns that as
the app-server binary. The assembled `bwrap` argv is captured in `replay.json`
under `sandbox.dryRunArgv`.

The mechanism, in the order that matters:

- **`--tmpfs $HOME` first.** Nothing under the home directory exists inside
  unless this script binds it in. That single decision removes `$HOME/git`
  (both real checkouts, which still hold the finished branch),
  `$BENCH_CHECKPOINTS`, `$HOME/.local/bin` (which is where
  `run-subagent` lives), and `$HOME/.bin`.
- **Stubs bind-mounted over the real toolchain.** `fixtures/stubs/` is
  `--ro-bind`ed over `$HOME/.cargo/bin` *and* over every installed
  `$HOME/.rustup/toolchains/*/bin`. Run 4's escape was an absolute-path
  call to `$HOME/.cargo/bin/just`; that exact path now resolves to the
  stub. Anything the stub dir does not provide is ENOENT, which is the safe
  failure. A `cargo-insta` stub was added so a direct `cargo-insta` invocation
  cannot fall through.
- **Sibling replay trees are not mounted.** Only this run's tree, CODEX_HOME and
  output dir are bound, each individually — never `$BENCH_REPLAY` as a
  whole, because `reference-61ec1752dd` and the earlier candidate trees are
  under it and several of them contain the answer.
- **`codex` is off PATH.** The real codex directory is bound read-only at a
  hidden path and exec'd by absolute path; a separate directory containing only
  a `codex-code-mode-host` symlink is what goes on PATH, because the code-mode
  host is resolved through PATH at runtime while the worker must not be able to
  start a nested agent by name.
- PATH inside is the stub dir, that helper dir, and a plain system PATH.
  Namespaces: user, ipc, pid, uts, cgroup unshared, `--die-with-parent`,
  `--new-session`. The network namespace is deliberately **shared** — the worker
  calls the real API.

Verified inside the jail, with the same launcher the run used:

| route | before | now |
|---|---|---|
| `$HOME/.cargo/bin/just` by absolute path (run 4) | real `just` | stub, logged |
| `$HOME/.rustup/toolchains/*/bin/cargo` | real cargo | stub |
| `run-subagent` (run 4) | started a real agent | file absent, not on PATH |
| `ls $HOME/git` (run 1's leak) | listed both checkouts | `No such file or directory` |
| `ls $BENCH_CHECKPOINTS` | listed the checkpoints | `No such file or directory` |
| `ls $BENCH_REPLAY` | 20 dirs incl. the reference tree | `results`, `worktree` only |
| `codex` on PATH | present | absent |
| DNS / API reachability | — | still works, by design |

`git` still works inside on a synthetic `shake-bench replay <replay@localhost>`
identity, which is what let the worker commit.

Honest residual risk. The jail is filesystem and namespace isolation, not
privilege separation: `--unshare-user` without an id map leaves the process
running as this Unix user from the kernel's point of view, so a bwrap or kernel
escape is not defended against. Network egress is unrestricted by design, so
nothing stops the worker reaching any host it likes. The real codex binary is
hidden rather than absent — a worker that reads `/proc/self` can find the path
and exec it. The stub directory sits inside the bound-rw output dir, so the
worker could rewrite a stub; that buys it nothing, because the real toolchain is
not inside the jail to call. And the script trusts its own argv: pointing
`--tree` at a sensitive directory would defeat the whole thing. `--redact` and
[`bench-watchdog.sh`](../../scripts/bench-watchdog.sh) are both still in the
loop; the watchdog recorded **no kills** in run 7, which is the first run where
that is evidence of nothing being attempted rather than of the watchdog being
the only thing standing in the way.

### 8.3 Turn timeout

Run 2's specific bug — a 1h ceiling killing a healthy turn — was already closed
before this session (4h default, `--turn-timeout-ms`, rollout preserved), and
the explicit deadline was already being passed into `waitFor`, so neither
`waitFor`'s 900s nor `request`'s 600s default was ever in play on the turn wait.
The residual defect was structural: one absolute deadline has to be set high
enough for the slowest healthy turn, which means a genuinely stuck turn produces
no signal for hours. `AppServerClient.waitForIdle()` now runs two clocks — an
idle clock reset by every notification for the thread, and a separate absolute
cap — and the error names which one fired and how long the turn had been
running. New `--turn-idle-timeout-ms` (30 min default) alongside
`--turn-timeout-ms` (4h). Neither fired in run 7.

### 8.4 The compaction check — the finding

**Request 1 was not compacted. Compaction happened anyway, one request later.**

From the worker's own rollout (`token_usage_record` and `compacted` records):

| request | input tokens | cached | note |
|---|---:|---:|---|
| 1 | **671,942** | 0 | the injected history, uncompacted |
| 2 | 672,104 | 671,744 | still full context |
| — | — | — | **`compacted` record at 18:31:53** |
| 3 | 19,390 | 9,216 | post-compaction |
| 4–11 | 19,701 → 43,363 | | context regrows from ~19k |

So trimming from 880k to 690k bought exactly one extra full-context request
before the product folded the thread.

> **CORRECTED — the inference below is falsified. See §11.9 for the mechanism.**
>
> What this section originally concluded: *"The untrimmed run 6 compacted after
> its first request at 863,471 tokens; run 7 compacted after its second at
> 672,104. That puts the auto-compaction trigger well below `gpt-6-astra`'s
> 872,000-token `max_context_window` — somewhere at or under ~77% of it."*
>
> It is kept here because two later replays (runs 8 and 9) were spent on it and
> the shape of the error matters: a threshold was bracketed from the outside,
> from three data points, when it could have been read directly out of the
> source. The trigger is not a fraction of `max_context_window` at all. The
> product never looked at `max_context_window`: `resolved_context_window()`
> (`protocol/src/openai_models.rs:488-490`) prefers `context_window`, which for
> `gpt-6-astra` is **272,000**, and the limit the replay was actually hitting
> was `272,000 * 9/10 = 244,800`. Every number in the "~77%" reasoning was
> measured against the wrong denominator. §11.9 has the full chain and the
> one-line config fix.

This does not invalidate the run — request 1 genuinely carried the whole
690,072-token history, and the check the rerun was for came back clean — but it
does mean **Tier B as specified is not sufficient to make a full-context arm
stay full-context for a whole turn.**

### 8.5 Run 7

| | |
|---|---|
| history | flattened, redacted, **Tier B trimmed** — 949 items, 2,622,839 bytes, **690,072 tokens** |
| worker | `codex-cli 0.153.4`, `gpt-6-astra`, effort medium — same binary and model as run 6 |
| sandbox | bubblewrap, on |
| exchanges | 4 (3 worker turns completed; the 4th scripted-user message ended the run) |
| stop reason | **`STOP: DONE`** — first replay to reach it |
| wall clock | worker turns **583 s** total (541 s + 36 s + 5 s) |
| worker tokens | **1,961,389 input** (1,220,864 cached), **13,222 output**, 27 model requests |
| simulated-user tokens | 8 input, 51,492 cache read, 82,108 cache write, 305 output — **$0.347** (reported by the CLI) |
| worker cost | **≈ $16.0** (bounds $9.3 – $18.2) |
| escape attempts blocked | none attempted; watchdog recorded no kills |
| acceptance | **REJECTED, 12 failing checks** |

Cost method, as in §6: uncached input is 1,961,389 − 1,220,864 = 740,525. The
first request is 671,942 of those and is above the 272,000 threshold, so it is
billed at 2x: **$13.44 for request 1 alone**, 84% of the run. The remaining
68,583 uncached at $10/M is $0.69, 1,220,864 cached at $1/M is $1.22, and 13,222
output at $50/M is $0.66. Bounds if the tiering is assumed uniformly: $9.3 (no
multiplier anywhere) to $18.2 (2x input and cache, 1.5x output everywhere). The
simulated user costs **2.2%** of the worker.

Stubbed commands the worker invoked, all of them, all from inside the jail:

```
just  test -p codex-core -p codex-thread-store -p codex-app-server-protocol -p codex-tui
just  test -p codex-core --test suite artifact_recovery
just  test -p codex-app-server
just  fix -p codex-core   /  -p codex-thread-store  /  -p codex-tui
just  fmt
cargo build -p codex-core -p codex-thread-store -p codex-app-server-protocol \
            -p codex-app-server -p codex-tui
```

**The 12 failing checks are the same 12 the untouched checkpoint fails.** Run 7
closed none of them. It was not idle — it committed `5c850ec`, "Harden shake
artifact recovery and report persistence failures", 234 insertions across 9
files: a rewrite of `artifacts.rs`, new artifact-recovery tests, a
persistence-failure path through `session/handlers.rs`, and two shake-notice
snapshots of its own. It simply spent the run on a different axis from the one
the original session took. It never vendored the app-server schema surface
(A1–A6), never produced the two snapshots the acceptance script names (B1, B2),
never made `read_artifact` registration conditional (C1), never added the UTF-8
boundary rejection (D1) despite saying "reads are bounded and UTF-8 safe", and
never made the two one-line fixups (E1, E2).

For comparison: run 6, on the **untrimmed** history, closed 5 of the 12. Run 3,
with a real toolchain, closed 8. Run 7, on the trimmed history, closed 0.

That is one run against one run and the simulated user was not held fixed —
its exchange-4 message ("That's acceptable to merge on") ended the run two
exchanges earlier than run 6's turn budget allowed, and its exchange-2 message
demanded a real build in direct contradiction of the brief's "believe the
worker" rule. So this is **not** evidence that trimming hurt quality. It is
evidence that the outcome is dominated by variance the harness does not yet
control, which is blocker 5 from §7, unchanged and now with a second data point.

### 8.6 Revised go / no-go (superseded by §11.8)

**Still no-go on the comparative arm matrix, but the reasons have changed and
two of the six blockers are now closed.**

| § blocker | status after run 7 |
|---|---|
| 1. history does not fit the window | **partly closed.** `--trim` exists, Tier B measures 690,079, request 1 runs uncompacted. But compaction still fires on request 2, so the arm is still not full-context for a whole turn. |
| 2. exchange wall-clock | **unchanged / fine under stubs.** 541 s for the heavy exchange, 583 s for the run. |
| 3. stubs change the task | **unchanged and now better evidenced.** Run 7's worker again could not discover anything by running the suite, and said so in its own final message. Pre-baked build artefacts are still real work, not a flag. |
| 4. the worker escapes the harness | **closed.** Bubblewrap, §8.2. Every route run 1 and run 4 used is now unavailable rather than discouraged. |
| 5. the simulated user is a live variable | **unchanged, and now the dominant term.** It broke its own brief again in run 7 and ended the run early. |
| 6. acceptance is structural, not behavioural | **unchanged.** It still discriminates correctly (reference 0, checkpoint 12, run 7 12). |

Remaining blockers before the matrix is worth building, in the order they should
be fixed:

1. **Freeze the simulated user.** This is now the largest source of run-to-run
   variance and it is cheap to remove: record one accepted transcript of operator
   messages and replay it verbatim in every arm, so the arms differ only in the
   history. The live driver becomes a one-off that produces that script. Until
   this lands, no two arms are comparable and no single run means anything.
2. ~~**Trim to an actually-sub-threshold history.** Tier B at 690k is not
   enough. The trigger sits at or below ~77% of the 872,000-token ceiling, so
   target ~600k measured.~~ **CORRECTED — see §11.9.** The trigger was 244,800,
   not "~77% of 872,000", so no tier this benchmark would accept could have
   cleared it. Tier C was built on this reasoning and did not help. `--trim`
   remains useful, as a cost lever.
3. **Decide what the control arm actually is.** Even at 600k, one turn's tool
   output can push the thread back over the trigger. Either the control arm is
   defined as "largest history that survives a whole turn uncompacted" and is
   measured empirically per model, or it is honestly named a compaction arm.
   Do not call it full-context without checking the rollout for a `compacted`
   record, every run.
4. **Pre-bake the build artefacts** (unchanged from §7 blocker 3) if the arms
   are meant to compare strategies on the *same* task the original session did.

What is ready and reusable, updated: the flattened-history builder with
redaction **and trimming**, the token measurement through the product's own
counter, the compaction check against the worker's rollout, the simulated-user
brief (as a source for a frozen script, not as a live driver), the stub
toolchain, **the bubblewrap jail**, the watchdog, the idle/absolute turn
timeout, and an acceptance script with verified discrimination at both ends.

## 9. Reproducing

```bash
cd shake-bench

tsx scripts/flatten-history.ts \
  --cutoff 2026-09-08T01:57:36.059Z \
  --out fixtures/01a07e54-r264-redacted \
  --redact <repo>=$BENCH_REPLAY/worktree \
  --redact <harness-repo>=$BENCH_REPLAY/session-cwd \
  --redact codex-artifacts=artifacts-work \
  --redact the harness repo=session-cwd

scripts/bench-watchdog.sh $BENCH_REPLAY &

# fresh copy of the checkpoint at the path the redacted history names
git -C $BENCH_CHECKPOINTS/01a07e54-r264 archive --format=tar HEAD \
  | tar -xf - -C $BENCH_REPLAY/worktree

nice -n 19 tsx scripts/replay-01a07e54.ts \
  --tree $BENCH_REPLAY/worktree \
  --history fixtures/01a07e54-r264-redacted.flattened.json \
  --out results/replay --max-turns 6

scripts/accept-01a07e54.sh $BENCH_REPLAY/worktree
```

Run 8 (Tier C + scripted user) is the current shape; run 7 (Tier B + live
driver) differs only in which two fixtures it names. Build the trimmed history
by adding `--trim` to the same flatten command:

```bash
tsx scripts/flatten-history.ts \
  --cutoff 2026-09-08T01:57:36.059Z \
  --out fixtures/01a07e54-r264-redacted-trimC \
  --trim fixtures/trim-01a07e54-tierC.json \
  --redact ...   # same four redactions as above
```

and point the replay at it. Sandboxing is on by default; `--no-sandbox` is the
old direct-spawn path.

```bash
nice -n 19 tsx scripts/replay-01a07e54.ts \
  --tree $BENCH_REPLAY/worktree \
  --history fixtures/01a07e54-r264-redacted-trimC.flattened.json \
  --out $BENCH_REPLAY/results/run-8-trimC-scripted
```

The simulated user is `fixtures/scripted-user-01a07e54.json` by default and its
own length sets the exchange count, so `--max-turns` does nothing in a scripted
run; `--live-user` restores the Sonnet driver. The tree must live at
`$BENCH_REPLAY/worktree` because that is the path the redactions
rewrite the history to name, and it must be a git repo (`cp -a` the checkpoint,
not `git archive | tar`) or the worker cannot commit.

Add `--measure-only` to get the product's token count for a history without
spending a turn. To inspect the jail without launching anything,
`scripts/bench-sandbox.sh --dry-run ... -- true` prints the assembled `bwrap`
argv.

**The replay now checks this itself** (§11.3): three signals, a `compaction`
block in `replay.json`, a loud banner, and exit code 2. The manual equivalent,
if you want it by hand:

```bash
grep -c '"type": *"compacted"' <codexHome>/sessions/*/*/*/rollout-*.jsonl
```

Do **not** grep for `auto-compact-` — that string names every internal turn id
the product mints and appears in every replay whether or not anything compacted
(§11.9). The second thing worth checking by hand is that input tokens climb
monotonically across `token_usage_record`s rather than falling off a cliff.

Artefacts: `$BENCH_REPLAY/results/run-6/replay.json`,
`run-7-trimB/replay.json`, `run-8-trimC-scripted/replay.json` and
`run-9-window-fixed/replay.json` (turns, usage, transcript, sandbox argv, stub
log), `$BENCH_REPLAY/logs/replay-run-*.log`,
`$BENCH_REPLAY/logs/accept-*.txt` and `accept-v2-*.txt`, the
per-request cost helper `$BENCH_REPLAY/logs/run-cost.py`, and the
preserved candidate trees `$BENCH_REPLAY/candidate-run{6,7,8}`.

## 10. Residual doubts

1. **Run 1's leak may not be the only one — closed for run 7 onward.** For runs
   1–6 the worker ran with `danger-full-access` on a filesystem that contained
   both checkouts, and nothing but redaction stopped it listing `$HOME/git`.
   From run 7 the bubblewrap jail (§8.2) means those paths do not exist inside
   the sandbox at all, so this is now proof rather than evidence — for run 7.
   Runs 1–6 keep the old caveat.
2. **Per-turn tokens for runs 1–3 are gone.** Run 2 deleted its `CODEX_HOME` on
   exit before that defect was fixed, and run 3 was killed before writing its
   record. Only run 6 has complete accounting.
3. **A run-2 candidate tree was destroyed by operator error** (`git checkout --
   .` in the candidate, intended to undo an acceptance side-effect). Its verdict
   under the old build-based script — 6 failing checks — was recorded before the
   revert and stands; it could not be re-measured under the new script.
4. **The stub test counts are fiction.** `just test` always reports 31 passed.
   A worker that trusts them believes it is green when it is not; run 6's worker
   said so in its own final message, but a less careful one would not.
5. **One simulated-user model, one worker model, one run each.** Nothing here
   separates model variance from history variance.
6. **Run 7 closed zero acceptance checks while run 6 closed five.** One run
   against one run, with an unfrozen simulated user that ended run 7 two
   exchanges early and contradicted its own brief in exchange 2. This does not
   show that trimming hurt quality and must not be quoted as if it did (§8.5).
7. **The auto-compaction trigger is inferred, not read — and the inference has
   already been falsified once.** Runs 6, 7 and 8 compacted after requests of
   863,471, 672,104 and 516,409 tokens. The "~77% of the ceiling" bound from
   run 7 did not survive run 8 (§11.5), and run 8's rollout shows the decision
   is taken at thread start, before any request. Nobody has read the condition
   out of the product; three runs have bracketed a number that may not be the
   mechanism.
8. **Run 8's acceptance score is 10/12 but its behavioural result is 12/12.**
   The two failures are a different error-message string and a different
   snapshot filename over work that is present and correct (§11.6). Every
   earlier run's score carries the same class of risk in the other direction and
   none has been re-examined for it.
9. **The watchdog killed 25 unrelated host processes during run 8** and zero
   replay processes (§11.7). Earlier runs used the same unscoped revision; any
   claim about what those runs' watchdogs "caught" should be read with that in
   mind.

## 11. Tier C + scripted user (run 8)

The two things §8.6 asked for first — freeze the simulated user, trim to an
actually-sub-threshold history — built and measured, plus the compaction check
promoted from a manual `grep` to an assertion the harness makes itself.

### 11.1 Tier C

[`fixtures/trim-01a07e54-tierC.json`](../../fixtures/trim-01a07e54-tierC.json).
Two changes over Tier B:

- **29 truncations instead of 22.** Every `function_call_output` estimated above
  ~7,000 tokens, head+tail 800/800 as before.
- **21 dropped call/output pairs**, a new `drop` op on the trim spec. These are
  the two redundant `shake_investigation` onboarding surveys the trim report
  flagged: flattened items **604–609** (3 pairs, the second survey cycle) and
  **845–883** (18 pairs, the third, which overlaps the nextest wait). The third
  cycle's conclusion survives untouched in assistant message **884**, and the
  second's in **613**.

The `drop` op is deliberately narrow. A drop names both halves
(`{"call": 845, "output": 846}`) and `flatten-history.ts` throws unless the call
index is a `function_call`, the output index a `function_call_output`, and the
two share a `call_id` — all three guards were tested against deliberately wrong
specs. Because only those two item types are droppable, **user messages and
assistant decision statements cannot be dropped at all**; that is a type
constraint, not a convention. Drops are applied after the truncations so both
kinds of index refer to the same untrimmed item list.

| history | items | bytes | tokens (product counter) |
|---|---:|---:|---:|
| flattened, redacted, untrimmed | 949 | 3,382,594 | 880,020 |
| Tier B | 949 | 2,622,839 | 690,079 |
| **Tier C** | **907** | **2,013,767** | **536,364** |

Under the 550,000 target. The `bytes/3.844` proxy predicted 511,148 — 4.7%
optimistic, the same direction and roughly the same size of error it made on
Tier B, which is now twice-confirmed and is why the product's counter is the
only number quoted. Redaction still holds: zero occurrences of either checkout
path, `bench-checkpoints`, `shake-bench`, or the final commit hash in the
injected items.

### 11.2 The scripted simulated user

[`fixtures/scripted-user-01a07e54.json`](../../fixtures/scripted-user-01a07e54.json),
v1, **six turns**, replayed verbatim. Turn N is sent as soon as the worker's
turn N-1 ends, whatever the worker said; nothing reads the worker's output; the
sixth turn tells it to stop and summarize and the run ends when that summary
lands. So the exchange count is fixed at 6 and the user half of the transcript
is byte-identical across arms. No model is in the loop on the user side, so the
simulated user now costs **$0.00** instead of run 7's $0.347.

The operator sent nothing after the cutoff, so the turns
are written in the operator's voice from the ordinal-9 task statement, the repository
conventions the live brief already encodes, and observations about the
checkpoint tree the session itself went on to act on. Turn 1 is byte-identical
to the live driver's kickoff, so runs 1–7 and run 8 share exchange 1. Turns 2–5
each carry one remaining work item as a requirement or a symptom — "the vendored
app-server schema is stale", "one [ephemeral session] still lists read_artifact"
— never a file, a function, a test name, or a diff. Each turn records its own
rationale in the fixture and the fixture carries a changelog, because the
wording is a benchmark input that gets iterated on deliberately rather than
drifting run to run.

The live Claude-Sonnet driver is still there behind `--live-user`, **off by
default**. Its job is now to produce candidate scripts, not to drive a scored
run.

### 11.3 The compaction assertion

`replay-01a07e54.ts` now reads the worker's rollout after every run and fails
the run — stderr banner, `compaction` block in `replay.json`, **exit code 2** —
on any of three signals:

1. a `compacted` record;
2. an input-token drop of more than 50% between consecutive requests;
3. ~~an `auto-compact-*` `turn_context`, which the product stamps at **thread
   start** when it has already decided the injected history needs compacting.~~
   **REMOVED — this was never a compaction signal. See §11.9.**

> **CORRECTED.** Signal 3 was originally described as *"new and the most useful
> of the three, because it fires before the first request is sent. It is a
> signal and not a fixture of every rollout: runs 7 and 8 both carry it, and
> none of the six small-history probe sessions in `codex-homes/probe-*` do."*
>
> The probe sessions do not carry it because they injected nothing, not because
> they avoided a compaction decision. `auto-compact-{n}` is what
> `Session::next_internal_sub_id` (`core/src/session/mod.rs:1282-1286`) names
> *every* internal turn, and `thread/inject_items` mints one
> (`core/src/codex_thread.rs:634` → `core/src/session/turn_context.rs:1082-1085`).
> The signal was 100% correlated with "this thread had history injected", which
> was true of every replay and no probe. It is still recorded in `replay.json`,
> because the marker is genuinely there and a future reader will wonder, but it
> no longer fails a run.

A fourth signal replaced it, and it is a real one because it checks a cause
rather than a symptom: the **usable context window the server reports for the
thread** (`modelContextWindow` on `thread/tokenUsage/updated`), asserted equal
to `872,000 * 95 / 100 = 828,400`. If the `model_context_window` override
(§11.9) is ever dropped, mistyped, or clamped away, the run fails immediately
and loudly instead of quietly producing another compaction arm labelled
full-context. The replay also reads back the effective config via `config/read`
before the first turn and records `model_context_window` and
`model_auto_compact_token_limit`.

### 11.4 Run 8

| | |
|---|---|
| history | flattened, redacted, **Tier C** — 907 items, 2,013,767 bytes, **536,354 tokens** |
| worker | `codex-cli 0.153.4`, `gpt-6-astra`, effort medium — same binary and model as runs 6 and 7 |
| simulated user | **scripted**, v1, 6 turns, $0.00 |
| sandbox | bubblewrap, on |
| exchanges | **6 of 6**, all completed |
| stop reason | `SCRIPT_END` (the script's own terminator, not a model decision) |
| wall clock | worker turns **1,075 s** total — 543 / 121 / 99 / 224 / 79 / 10 |
| worker tokens | **5,701,702 input** (5,061,888 cached), **23,421 output**, 72 model requests |
| compaction assertion | **FAILED** (see below) |
| cost | **≈ $19.1**, summed per request against `pricing.json` |
| escape attempts | none by the worker |
| acceptance | **REJECTED, 2 failing checks — 10 of the checkpoint's 12 closed** |

Cost, per request rather than in aggregate, because only the first two requests
are above the 272,000-token long-context threshold: request 1 (516,268 in, 0
cached, 104 out) is $10.33 and request 2 (516,409 in, 516,096 cached, 3,895 out)
is $1.33 — **61% of the run in two requests**. The remaining 70 requests, all
post-compaction and heavily cached, are $7.5 between them.

Stubbed commands the worker invoked — 24 invocations, all from inside the jail,
deduplicated:

```
just  test -p codex-core -p codex-thread-store -p codex-app-server-protocol -p codex-tui
just  test -p codex-core --test suite artifact_recovery
just  test -p codex-core --test suite openai_file_mcp
just  test -p codex-core --lib -E 'test(artifacts::tests) | test(shake::tests)'
just  test -p codex-core --lib ephemeral_threads_do_not_register_artifact_reader
just  test -p codex-core -E 'test(artifact_recovery) | test(ephemeral_threads_do_not_register_artifact_reader)'
just  test -p codex-thread-store  /  -p codex-app-server-protocol
just  test -p codex-tui shake_notice  /  shake_notice_releases_input_gate  /  status::rate_limits
just  fix -p codex-core -p codex-thread-store -p codex-tui -p codex-app-server-protocol
just  fmt
cargo insta pending-snapshots
```

### 11.5 The compaction finding: trimming is not the lever

**Tier C at 536,354 tokens compacted anyway, after the same request 2 as
Tier B did at 690,079.**

| run | history | request 1 | request 2 | request 3 |
|---|---:|---:|---:|---:|
| 6 | 880,020 | 863,471 | — compacted after 1 | |
| 7 | 690,079 | 671,942 | 672,104 | 19,390 (compacted) |
| 8 | **536,354** | **516,268** | **516,409** | **19,566 (compacted)** |

Two things follow. The first holds. The second was wrong.

1. The trigger inferred in §8.4 — "at or under ~77% of the 872,000-token
   ceiling" — is **wrong**. 516,268 tokens is 59% of the ceiling and still
   compacted. This part stands; §11.9 says why the ceiling was the wrong number
   to measure against in the first place.

> **CORRECTED — point 2 below is falsified. See §11.9.**
>
> What this section originally concluded: *"The product had **already decided**
> before the first request. Run 8's rollout carries a `turn_context` with
> `turn_id: auto-compact-0` at `19:11:04.771Z`, stamped at thread start, ~2.5
> minutes before request 1 at `19:13:22`. Run 7's rollout carries the same
> marker at its own thread start. The small-history probe sessions do not. So
> the decision is made against the injected history at inject/start time."*
>
> `auto-compact-0` is a **naming collision, not a decision**.
> `Session::next_internal_sub_id` (`core/src/session/mod.rs:1282-1286`) formats
> *every* internal turn id as `auto-compact-{n}`, unconditionally — the string
> is a hardcoded `format!`, with no branch on compaction state anywhere near
> it. `thread/inject_items` mints one: `inject_response_items_for_turn`
> (`core/src/codex_thread.rs:634`) calls `Session::new_default_turn`, which
> passes `next_internal_sub_id()` straight through
> (`core/src/session/turn_context.rs:1082-1085`). So `auto-compact-0` appears at
> the start of *any* thread that had items injected. The "probe sessions do not
> carry it" observation that made this look like a signal is explained the same
> way: those probes injected nothing.
>
> The marker is now recorded but no longer treated as a violation by
> `checkCompaction` in `scripts/replay-01a07e54.ts`. Confirmed independently: a
> `--measure-only` run, which starts a thread, injects, and never takes a turn
> at all, stamps `auto-compact-0` too
> (`codex-homes/replay-home-kUgjuD/sessions/2026/09/11/rollout-2026-09-11T11-54-12-*.jsonl`).

**Trimming was never the lever, but not for the reason given above.** The limit
the replay was hitting was 244,800 tokens — well under even Tier C — and no
tier of trimming this benchmark would accept was ever going to clear it. §11.9.

### 11.6 Acceptance: 10 of 12, and the 2 that remain are the script's fault

Run 8's candidate tree (`$BENCH_REPLAY/worktree`, six worker commits
on top of `22f9689 checkpoint 01a07e54@r264`) fails **2** checks where the bare
checkpoint fails 12. Runs 6 and 3 closed 5 and 8; run 7 closed 0.

Closed: A1–A6 (schema surface vendored), B1 (persistent shake-notice snapshot),
C1 (`read_artifact` registration made conditional), E1 and E2 (both one-line
fixups). Still failing: **B2** and **D1**.

Both remaining failures are **string mismatches over work that is present**:

- **D1** greps `core/src/artifacts.rs` for `/is not a UTF-8 character boundary/`.
  The worker's line 83 reads
  `return Err(format!("byte {offset} is not a UTF-8 boundary in {uri}"));` —
  the behaviour the check is about (reject a mid-character start offset rather
  than slide it) is implemented, in the right file, with a different message.
- **B2** looks for `codex_tui__chatwidget__tests__ephemeral_shake_notice.snap`.
  The worker committed
  `codex_tui__chatwidget__tests__shake_notice_renders_summary_for_ephemeral_thread.snap`
  — the ephemeral notice is snapshotted, under the name its own test generated.

So the behavioural result is 12 of 12 and the structural score is 10 of 12.
This is §7 blocker 6 ("acceptance is structural, not behavioural") landing
concretely for the first time: the script discriminates correctly at both ends
but scores a correct implementation down for naming. **Do not quietly loosen
those two checks to make the number look better** — re-derive them (D1 against
the *behaviour*: an error path keyed on the offset, not on a message; B2 against
*any* snapshot whose content is the ephemeral notice) and re-verify the
reference tree at 0 and the bare checkpoint at 12 afterwards. That verification
is the whole value of the script and it has not been redone here.

### 11.7 The watchdog killed the wrong things

`bench-watchdog.sh` logged **25 kills** during run 8 and **none of them were the
replay**. Every one was an unrelated process belonging to other agent sessions
on this host — `run-subagent`, `codex … exec -m gpt-5.6-sol`, an invocation
pointed at a second Codex account/home. The `case` arms
`*run-subagent*` and `*"exec -m "*` matched anywhere in the argv, unscoped by
`$ROOT`, so the watchdog was hunting the operator's own concurrent work rather
than the jailed worker. The worker itself, inside bubblewrap, attempted nothing.

Fixed: a kill now requires `$ROOT` in the argv **or** the process's
`/proc/<pid>/cwd` under `$ROOT`. The cwd half is what still catches a worker
that shells out from inside its own tree with an argv naming no path. Anyone who
ran an earlier revision of this watchdog alongside other agent sessions lost
those processes.

### 11.8 Revised go / no-go (superseded by §11.11)

**Still no-go on the comparative arm matrix, but for one reason now instead of
six, and it is not a reason trimming or prompting can fix.**

| § blocker | status after run 8 |
|---|---|
| 1. history does not fit the window | **reframed, still open.** Tier C measures 536,354 and the thread compacted anyway, after the same request as Tier B. The product decides at thread start (§11.5). Trimming is not the lever. |
| 2. exchange wall-clock | **fine.** 1,075 s for six exchanges under stubs, the first taking half of it. |
| 3. stubs change the task | **unchanged.** The worker again reported it could discover nothing by running the suite and said so in its final message. |
| 4. the worker escapes the harness | **closed, and now with a clean run behind it.** No escape attempt; the watchdog's own 25 kills were all collateral (§11.7). |
| 5. the simulated user is a live variable | **closed.** Frozen, 6 turns, verbatim, $0.00, exchange count fixed by construction. |
| 6. acceptance is structural, not behavioural | **open, and now demonstrated.** Two of run 8's failures are naming mismatches over correct work (§11.6). |

Remaining blockers, in the order they should be fixed:

1. ~~**Read the auto-compaction trigger out of the product.** Not another
   replay. Until the condition behind `auto-compact-0` is known, no arm can be
   called full-context and no amount of trimming is known to help.~~
   **DONE — §11.9.** There was no condition behind `auto-compact-0`; the marker
   meant nothing. The trigger was a 244,800-token limit the worker config asked
   for by omitting `model_context_window`.
2. ~~**Re-derive D1 and B2 behaviourally**, then re-verify the reference tree at
   0 failures and the bare checkpoint at 12.~~ **DONE — §11.10.**
3. **Pre-bake the build artefacts** (unchanged) if the arms are meant to compare
   strategies on the same task the original session did.

What run 8 does establish, and what §8.5 could not: **a worker handed this
history and a frozen six-turn user finishes essentially all of the remaining
work.** Six commits, the schema surface vendored, both snapshots, the ephemeral
guard, the UTF-8 rejection, both one-liners. That is the feasibility question
from §7 answered in the affirmative, on one run, with the user side held fixed
so the next run can be compared to it.

### 11.9 The actual mechanism: the model catalog, not the history

Everything in §8.4 and §11.5 was measured against the wrong number. The
auto-compaction the replays kept hitting was never a fraction of
`gpt-6-astra`'s 872,000-token ceiling, and it was never decided at thread
start. It was a 244,800-token limit the worker config asked for by omission.

**The chain, read out of `the harness repo` (the fork the worker binary is
built from) rather than bracketed from the outside:**

1. `core/src/session/context_window.rs:52-121` computes the thread's
   `ContextWindowTokenStatus` before and after each turn. Compaction is forced
   when `auto_compact_scope_tokens >= buffered_auto_compact_limit`, where the
   scope tokens are the server-reported **total** usage for the thread.
2. That limit comes from `ModelInfo::auto_compact_token_limit()`
   (`protocol/src/openai_models.rs:499-510`):
   `min(catalog auto_compact_token_limit, resolved_context_window * 9 / 10)`.
   A separate hard cap, `full_context_window_limit`, is
   `resolved_context_window * effective_context_window_percent / 100`.
3. `ModelInfo::resolved_context_window()`
   (`protocol/src/openai_models.rs:488-490`) is
   **`self.context_window.or(self.max_context_window)`** — it *prefers*
   `context_window`, and only falls back to `max_context_window` when
   `context_window` is absent.
4. `gpt-6-astra` in `models-manager/models.json` ships
   **`context_window: 272000`, `max_context_window: 872000`**, with
   `auto_compact_token_limit` null and no `effective_context_window_percent`
   (so the 95 default from
   `protocol/src/openai_models.rs:377-379` applies).

So for a worker with no config override:

| quantity | value |
|---|---:|
| `resolved_context_window()` | **272,000** |
| auto-compact limit (`* 9/10`) | **244,800** |
| hard full-window cap (`* 95/100`) | **258,400** |

Against a Tier C history of **536,357 tokens**. The thread was over the
auto-compact limit by a factor of 2.2 *before the worker said a word*. Runs 6,
7 and 8 did not compact at 863k, 672k and 516k because those numbers meant
anything — they compacted at the first checkpoint after the limit, and the limit
was the same 244,800 in all three. Tier C "surviving one more request" than Tier
B was the first-request-uncompacted quirk in §8.4, not a threshold being
approached.

**`max_context_window` is the number the reports kept quoting, and the product
never looks at it** except as a fallback and as the clamp in step 5.

#### The fix

One line in `WORKER_CONFIG` (`scripts/replay-01a07e54.ts`):

```toml
model_context_window = 872000
```

`with_config_overrides` (`models-manager/src/model_info.rs:25-33`) writes this
into `ModelInfo.context_window`, **clamped to `max_context_window`** — so
872,000 is not an arbitrary large number, it is precisely the ceiling this model
actually has, and asking for more would silently get the same result. `model_context_window` is a
valid top-level `config.toml` key (`core/src/config/mod.rs:626`,
`core/src/config/config_loader_tests.rs:344`).

`model_auto_compact_token_limit` is deliberately **not** set. The catalog leaves
it null, and `auto_compact_token_limit()` `min()`s any configured value into the
`* 9/10` bound — so any value here could only ever lower the limit back down.
The replay asserts it is null via `config/read`.

| quantity | before | after |
|---|---:|---:|
| `resolved_context_window()` | 272,000 | **872,000** |
| auto-compact limit | 244,800 | **784,800** |
| hard full-window cap | 258,400 | **828,400** |
| Tier C history | 536,357 | 536,357 |
| headroom above the history | **−291,557** | **+248,443** |

#### Two evidence channels, both asserted

The fix is worthless if it silently stops applying, so the replay now proves it
on every run rather than trusting the config file:

1. **Before the first turn** — `config/read` against the worker's own
   app-server, recorded as `effectiveConfig` in `replay.json`. Proves the
   override was parsed and no lower auto-compact limit is in force.
2. **Per request** — `modelContextWindow` on `thread/tokenUsage/updated`
   (`app-server-protocol/src/protocol/v2/thread.rs:1871-1890`, fed by
   `TurnContext::model_context_window()` =
   `ModelInfo::usable_context_window()`). This is the only channel that reports
   the window *after* the catalog entry, the config override and the 95%
   haircut have all been applied — i.e. the one that proves the override
   reached inference and not just the config loader. Asserted equal to 828,400;
   a mismatch fails the run the same way a `compacted` record does.

#### What this cost

Three replays (7, 8, and the trimming work behind them) were spent narrowing a
bound from the outside. §11.5 ended with *"somebody should read the actual
threshold out of the product rather than bracketing it"* — that read took about
twenty minutes and four files. The lesson is cheap to state and was expensive
to learn: **when a product makes a decision you do not understand, read the
decision, do not bracket it.** The trim tiers (A/B/C) are not wasted — they are
still the lever for making the *cost* of a full-context arm tolerable — but
they were never the lever for compaction.

### 11.10 D1 and B2, re-derived behaviourally

§11.6 recorded run 8 at 10 of 12 and said the two failures were the script's
fault — a different error-message string and a different snapshot filename over
work that was present and correct. It also said not to quietly loosen the checks
to make the number look better, and to re-verify all three trees afterwards.
Both checks are now re-derived against the behaviour, and all three trees are
re-verified.

**D1 — "artifact reads reject mid-character byte offsets."**

Was: `grep -E 'is not a UTF-8 character boundary' core/src/artifacts.rs`, which
is the reference tree's sentence and nobody else's.

Now: a structural assertion that *a char-boundary predicate guards an error
return*. An `awk` pass over `core/src/artifacts.rs` looks for a line that both
tests a boundary — `is_char_boundary`, or an explicit UTF-8 continuation-byte
mask (`0b1100_0000` / `0b11000000` / `0xC0`) — and reads as a guard (`if`,
`guard`, `match`, `assert`), then requires an error return (`return Err`,
`bail!`, `Err(format!`, `.ok_or`) within three lines of it.

The three-line proximity window is the whole point and is not incidental. The
bare checkpoint *does* contain a continuation-byte mask test — it uses one to
compute a `leading_continuation` skip count — but it then **returns content**,
sliding the caller silently past the partial character. That is precisely the
bug the requirement is about. Predicate and error return as two independent
greps would pass the checkpoint; the window is what separates "detects the
condition" from "rejects on the condition".

| tree | what it does | D1 |
|---|---|---|
| reference `61ec1752dd` | `if offset > 0 && (bytes[0] & 0b1100_0000) == 0b1000_0000 { return Err(…) }` | PASS |
| run 8 candidate | `if bytes[0] & 0b1100_0000 == 0b1000_0000 { return Err(…) }` | PASS |
| bare checkpoint | `let leading_continuation = bytes.iter().position(…)` then returns content | FAIL |

**B2 — "the ephemeral shake notice has an accepted snapshot."**

Was: `test -f …__ephemeral_shake_notice.snap`, one filename.

Now: any `.snap` in `tui/src/chatwidget/snapshots/` that is about the shake
notice *and* about the ephemeral case, matched on either the filename or the
snapshot body — `insta` records the generating expression and test name in the
`.snap` header, so a snapshot named after its own test is still identifiable by
content. B1's persistent-thread snapshot is explicitly excluded so it cannot
satisfy both checks.

| tree | snapshot found | B2 |
|---|---|---|
| reference `61ec1752dd` | `…__ephemeral_shake_notice.snap` | PASS |
| run 8 candidate | `…__shake_notice_renders_summary_for_ephemeral_thread.snap` | PASS |
| bare checkpoint | none | FAIL |

#### Re-verification

The point of re-verifying is that a loosened check is indistinguishable from a
behavioural one until you show it still rejects. All three run on the revised
script, saved to `$BENCH_REPLAY/logs/accept-v2-*.txt`:

| tree | verdict | failing checks |
|---|---|---:|
| reference `61ec1752dd` | **ACCEPTED** | 0 |
| bare checkpoint r264 | **REJECTED** | **12** — A1–A6, B1, B2, C1, D1, E1, E2, the identical set |
| run 8 candidate (`candidate-run8`) | **ACCEPTED** | **0** (was 2) |

The two older candidate trees are unchanged too — `candidate-run7` still
REJECTED at 12 and `candidate-run6` still REJECTED at 5, matching §5's table
exactly. So across five trees the revision moved exactly one number, run 8's,
and moved it to the value §11.6 had already worked out by hand.

The checkpoint's failing set is unchanged, check for check, which is the thing
that had to hold: the revision did not buy run 8's two points by weakening what
the script rejects. Run 8's structural score now matches the behavioural
result §11.6 asserted by hand — 12 of 12 — without the script having been
shown run 8's tree while it was being written. The run 8 tree is preserved at
`$BENCH_REPLAY/candidate-run8`; `$BENCH_REPLAY/worktree`
was reset from `$BENCH_CHECKPOINTS/01a07e54-r264` for run 9 and
re-verified REJECTED at 12 before the run started.

### 11.11 Run 9: context window fixed

The same Tier C history, the same frozen six-turn script, the same jail and the
same stubs as run 8. One line of worker config different. It is the first
replay in this benchmark that was full-context for its whole length, and the
first that reached acceptance.

| | |
|---|---|
| history | flattened, redacted, **Tier C** — 907 items, 2,013,767 bytes, **536,357 tokens** (elide would leave 175,819 across 191 tool outputs) |
| worker | `codex-cli 0.153.4`, `gpt-6-astra`, effort medium — identical to runs 6–8 |
| worker config | **`model_context_window = 872000`**, the only change from run 8 |
| simulated user | scripted, **v1, 6 turns, unchanged**, $0.00 |
| sandbox | bubblewrap, on |
| exchanges | **6 of 6**, all `completed` |
| stop reason | `SCRIPT_END` |
| wall clock | worker turns **757 s** total — 256 / 169 / 124 / 120 / 74 / 15 |
| worker tokens | **23,185,289 input** (22,604,160 cached), **18,862 output**, **42 model requests** |
| **compaction assertion** | **PASSED** |
| cost | **$58.25** API shadow, summed per request — **734 Codex credits**, which is what a ChatGPT-login run on the standard tier actually spent (1,835 had it been Fast) |
| escape attempts | none; watchdog logged **0 kills** |
| acceptance | **ACCEPTED — 12 of 12, 0 failing checks** |

#### The window override took effect — both channels

```
effective config: {"model_context_window":872000,
                   "model_auto_compact_token_limit":null,
                   "model_auto_compact_token_limit_scope":null}
context window OK: server reports usable window 828400
```

`config/read` before the first turn confirms the override parsed and that no
auto-compact limit is set alongside it that could `min()` the limit back down.
`modelContextWindow` on `thread/tokenUsage/updated` reports **828,400** —
`872,000 * 95 / 100`, exactly the predicted post-clamp, post-haircut value — on
every usage notification of the run, which is what proves the override reached
inference and not merely the config loader. The auto-compact limit behind it is
784,800.

#### Compaction: clean across all 42 requests

```
compaction assertion PASSED: 42 requests, no compaction event,
no >50% input-token drop (peak input 583114)
```

Zero `compacted` records, zero >50% input drops, zero violations. Input tokens
climb monotonically from 516,268 to 583,114 across the whole run — the shape a
full-context thread has, and the opposite of runs 6–8, which fell off a cliff to
~19k. The `auto-compact-0` marker is present, once, as §11.9 predicts it is in
every injected thread; it is recorded and is not a violation.

| run | history | req 1 | req 2 | req 3 | peak | compacted |
|---|---:|---:|---:|---:|---:|---|
| 6 | 880,020 | 863,471 | — | — | 863,471 | after req 1 |
| 7 | 690,079 | 671,942 | 672,104 | 19,390 | 672,104 | after req 2 |
| 8 | 536,354 | 516,268 | 516,409 | 19,566 | 516,409 | after req 2 |
| **9** | **536,357** | **516,268** | **516,540** | **521,804** | **583,114** | **never** |

Runs 8 and 9 have identical first requests, to the token. Everything after
diverges.

#### Acceptance: 12 of 12

`$BENCH_REPLAY/worktree` was reset from
`$BENCH_CHECKPOINTS/01a07e54-r264` and verified REJECTED at 12 before
the run. Afterwards, five worker commits on top of `22f9689`:

```
495823e fix: preserve artifact bytes and clean up failed writes
19e560e fix: synchronize vendored shake request schemas and export bundles
6d64d43 test: commit normal and ephemeral shake notice snapshots
2b97dd0 fix: omit artifact reader from ephemeral tool registries
126471b fix: clear lint warnings and verify strict artifact offsets
```

**ACCEPTED, 0 failing checks** (`$BENCH_REPLAY/logs/accept-run-9.txt`),
preserved at `$BENCH_REPLAY/candidate-run9`. This is the first replay
to be accepted outright. Run 8 reached the same behavioural result and scored 10
of 12 only because of the two checks §11.10 re-derived — so the honest reading
is that the *task* was already being finished at run 8, and run 9 adds the
full-context condition plus a script that can see it.

27 stubbed invocations, 15 distinct:

```
cargo check -p codex-core -p codex-app-server -p codex-tui
cargo insta pending-snapshots
just  clippy -p codex-core -p codex-tui -- -D warnings
just  fix -p codex-core -p codex-thread-store
just  fmt
just  test -p codex-app-server-protocol
just  test -p codex-core
just  test -p codex-core --lib artifacts::tests
just  test -p codex-core --lib tools::spec_plan_tests
just  test -p codex-core -p codex-thread-store -p codex-app-server-protocol -p codex-tui
just  test -p codex-core --test all artifact_recovery
just  test -p codex-thread-store
just  test -p codex-tui
just  test -p codex-tui shake
just  test -p codex-tui status::rate_limits
```

#### The cost, which is now the interesting number

**Recomputed 2026-09-12 in both profiles.** This worker authenticates through
ChatGPT login, so the dollars here are a **shadow** cost on the API rate card and
the **credits** are the real one, from the official Codex pricing page
(<https://learn.chatgpt.com/docs/pricing>, fetched 2026-09-12: gpt-6-astra 250
credits / 1M input, 25 / 1M cached input, 1,250 / 1M output; *"Fast mode applies
a 2.5x multiplier to Astra's Standard rate"*; **no** long-context multiplier and
**no** cache-write charge). Run 9 ran the standard tier.

**$58.25 of shadow cost — 734 credits — for one arm of one repetition** — 3.0x
run 8's $19.13 (334 credits) for the same work, and the difference is entirely
the compaction that used to happen. Every one of the 42 requests is above the
272,000-token long-context threshold and so pays 2x input, 2x cached-input and
1.5x output **on the API card**; in credits there is no threshold, so that
doubling is an API-billing artefact and not part of the real cost. The shape:

| | API shadow $ | Codex credits (standard) |
|---|---:|---:|
| request 1 (516,268 in, 9,216 cached — effectively cold) | **$10.17** | **127** |
| requests 2–42 (516k → 583k, ~99.7% cached) | **$48.08**, ≈ $1.17 each | **607**, ≈ 14.8 each |

Run 8 paid the same $10.17 for its first request and then fell to ~$0.12 a
request post-compaction. So the rule for budgeting the matrix is:
**a full-context arm costs ≈ $10 to prime plus ≈ $1.17 per request** on the API
card, and — the figure to actually budget against — **≈ 127 credits to prime plus
≈ 14.8 per request on the standard tier, or ≈ 320 + ≈ 37 per request on Fast**
(2.5x). Request count is driven by how much tool work an exchange does, not by
the exchange count (run 9's six exchanges took 13/8/6/8/6/1 requests). Cached
input, not uncached input, is the dominant term after request 1 on either card —
at 2x on the API card, and simply because there is 40x more of it in credits —
so trimming the history is what moves *both* halves, which is the role Tier C
actually plays.

### 11.12 Go / no-go for the arm matrix

**GO, conditionally** — on three arms, not four, and with the cost understood
before anything is launched.

Every blocker that was about *whether the experiment can be run at all* is now
closed. What remains are decisions and wiring, not unknowns.

| § blocker | status after run 9 |
|---|---|
| 1. history does not fit the window | **CLOSED.** It always fitted; the window was misconfigured. 42 requests, 516k→583k, no compaction (§11.9, §11.11). |
| 2. exchange wall-clock | **CLOSED.** 757 s for six exchanges under stubs — *faster* than run 8's 1,075 s, because an uncompacted thread does not spend a turn re-reading what it forgot. |
| 3. stubs change the task | **OPEN, unchanged, and now the only substantive one.** The worker still cannot discover anything by running the suite. Pre-baking build artefacts is real work. |
| 4. the worker escapes the harness | **CLOSED.** No escape attempt; watchdog logged **0 kills**, against run 8's 25 collateral ones — the `$ROOT` scoping (§11.7) is confirmed working. |
| 5. the simulated user is a live variable | **CLOSED.** v1, six turns, verbatim, $0.00, unchanged from run 8, and now with two runs to compare across it. |
| 6. acceptance is structural, not behavioural | **CLOSED.** Re-derived and re-verified across five trees (§11.10). |

The go is *conditional* because blocker 3 has not moved, and it bounds what the
matrix can claim: under stubs, the arms compare **context strategies on a task
the worker cannot use the test suite to explore**. That is still a real
comparison — it is the same reduced task in every arm — but the report must say
so rather than implying the arms were run on the original session's task.

#### What the arm runner still needs

`prepareArm` in `src/arms.ts` already implements the preparations, against a
`Driver` interface `AppServerClient` satisfies. What is missing is wiring and
four decisions.

**1. How the shake variants are generated from the Tier C history.**
Mechanically settled and already half-present: `replay-01a07e54.ts` calls
`thread/shake/preview` after `thread/inject_items` today, purely to measure
(`historyTokensAfterElide` = 175,819 over 191 tool outputs). The arm runner
inserts one `prepareArm(driver, threadId, arm)` call at that same point —
**after injection, before the first scripted user turn** — which for
`shake-elide` previews, checks `unavailableReason` and `toolOutputs > 0`, fires
`thread/shake/start` with the preview's `expectedFingerprint`, and waits for the
`⛭ shake: Shook` warning.

- *full history* = no preparation. The run 9 configuration verbatim, 536,357
  tokens.
- *shake with retrieval* = the existing `shake-elide` arm. ~175,819 tokens, and
  `read_artifact` available to page back what was elided.
- *shake without retrieval* = the existing `shake-elide-noread` arm. Same
  elision, retrieval withheld. **This one needs work.** `promptVariant()`
  expresses `noread` as a prompt variant, which was adequate for the synthetic
  runs but is not containment here: the replay worker has a real tool registry,
  so a prompt-only `noread` measures the model's propensity to obey an
  instruction, not the effect of withholding retrieval. `read_artifact` has to
  be actually unregistered — the conditional registration in `spec_plan.rs` that
  check C1 exists to verify is the natural hook.

**2. The Spark summary arm is UNDEFINED and must be specified before it can be
run.** There is no such entry in `ARMS`, no implementation anywhere in this
repository, and no `spark` in `models-manager/models.json` (the catalog ships
astra, sol, terra, luna, daybreak blue/red and the 5.x line). Three things it
could mean, and they are not close to each other:

  - the product's own summarizer, i.e. the existing `compact` arm
    (`thread/compact/start`) under another name — already built, costs nothing
    to add;
  - a cheaper model summarizing the Tier C history out of band, the summary
    injected as the entire history — needs a summarization prompt, a model
    choice, a token budget, and a decision about whether the summarizer sees the
    tool outputs Tier C already truncated;
  - a tool this repository does not contain.

Until somebody writes down which, **the matrix is three arms.** Do not guess it;
a mislabelled fourth arm is worse than a missing one.

**3. Cache-condition control — the confound most likely to silently invalidate
the comparison.** Run 9's cost is dominated by cached input at 2x: request 1 is
$10.17 and the other 41 are ~$1.17. Whether an arm's first request finds a warm
prefix depends on what ran before it and how recently. `armOrder`'s Latin square
breaks *ordering* confounds; it does nothing for caching, and in fact rotating
arms is exactly what makes each arm's cache state depend on its neighbours. The
runner needs an explicit, stated policy:

  - **cold start for every arm** — a fresh thread per arm with enough separation
    that no prefix survives. Honest, and the expensive one; or
  - **warm start for every arm** — an identical priming request before each.

Cold is the right default. The check is mechanical: record `cachedInputTokens`
on request 1 of every arm and require it to look the same across arms. Run 9's
was **9,216 against 516,268 input**, i.e. effectively cold — that is the number
that has to match.

**4. Repeats and cost estimate.** Per-request cost helper:
`scripts/run-cost.py` (validated — it reproduces run 8's $19.13 and run 9's
$58.25 from `replay.json`, and since 2026-09-12 also prints Codex credits;
`$BENCH_REPLAY/logs/run-cost.py` is a shim that execs it). Using run
9's measured rule, **≈ $10 to prime plus ≈ $1.17 per request**, and run 9's 42
requests as the full-history figure. The credit columns are the real budget; the
dollars are shadow. Fast credits are 2.5x standard:

| arm | history | est. requests | est. $ / repetition | est. credits, standard | est. credits, **Fast** |
|---|---:|---:|---:|---:|---:|
| full history | 536,357 | ~42 | **~$58** | ~734 | **~1,835** |
| shake, retrieval | ~175,819 | ~42 + `read_artifact` round trips | **~$12–18** | ~310 | **~775** |
| shake, no retrieval | ~175,819 | ~42 | **~$11** | ~290 | **~725** |
| *(Spark summary)* | *undefined* | — | — | — | — |
| **one repetition, 3 arms** | | | **≈ $85** | **≈ 1,330** | **≈ 3,335** |

*(Estimates superseded by measurement — see `reports/2026-09-11/arms-2026-09-11.md`:
the arms came in at 773 / 1,870 / 914 Fast credits, so the shake estimates were
close and the full-history arm cost about as predicted.)*

The shake arms are estimates and are the soft numbers here: below 272,000
tokens they fall off the long-context multiplier entirely, which is most of the
saving, but their request count may rise if retrieval round trips replace what
was elided — that is the effect the benchmark exists to measure, so it cannot
be assumed away in the budget.

Repeats: run-to-run variance in the worker is now the only live variable, since
the user is frozen and the history is byte-identical per arm. **Three
repetitions** is the minimum that says anything about variance, which is
**≈ $255** for three arms — **≈ 4,000 standard credits, ≈ 10,000 on Fast**.
Budget ≈ $300 / ≈ 11,500 Fast credits with a failed run allowed for. If that
is too much, the lever is Tier C → a tighter trim on the *control* arm, which
cuts both the priming request and the per-request cached cost — and which is
what the trim tiers are actually for (§11.9).

Before launching: decide the Spark arm or drop it, make `noread` real, and pick
the cache policy. None of the three is a replay; all three are a morning.
