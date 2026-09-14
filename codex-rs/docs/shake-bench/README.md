# shake-bench

**Headline numbers: [RESULTS.md](RESULTS.md).** The rest of this page is how to run it.

A retention benchmark for **shake**, the Codex fork's surgical context-reduction
feature. Shake deletes item classes from a thread's live history (tool outputs,
images, thinking blocks) and leaves recovery placeholders pointing at per-thread
artifacts the model can page back in with the `read_artifact` tool.

The question this harness answers is: *after a shake, how much of the thread's
usable state survives, and what does recovering the rest cost?*

Everything runs through the shipped product path — `codex-next app-server` over
stdio JSON-RPC — rather than a reimplementation of the context pipeline.

## What is and is not included here

This directory has the benchmark's methodology, harness code, and result
tables. It does **not** include:

- Any raw Codex session transcript (`.jsonl` rollout files) used to build a
  checkpoint or replay.
- Any packed checkpoint (a working tree reconstructed from a session at a
  cutoff point).
- The `fixtures/` and `results/` directories from the original working
  checkout — generated/derived data and hand-written test fixtures that are
  either large, environment-specific, or (for a couple of files) contain
  synthetic-but-still-real conversation text from an evaluation run.

If you want to reproduce a checkpoint or a full-context replay yourself, start
from your own Codex rollout files (normally under `$CODEX_HOME/sessions/...`,
which defaults to `~/.codex/sessions/...`) and:

1. Point `scripts/mine-checkpoint.ts` at a rollout `.jsonl` to get a per-request
   token timeline, tool-call inventory, and file-mutation list — this is how
   you'd pick your own cutoff point.
2. Feed that cutoff to `scripts/reconstruct-checkpoint.ts` to rebuild the dirty
   working tree at that point (see "Checkpoint reconstruction" below).
3. Feed the checkpoint plus a flattened history (`scripts/flatten-history.ts`)
   to `scripts/replay-01a07e54.ts` (or your own equivalent driver script) to
   run a full-context replay.

None of these scripts need the original session's transcript to be published
anywhere — they operate on rollout files that already exist on your own
machine once you've had the relevant Codex session.

## Attribution

The fixture generator (`src/fixtures.ts`) is copied **verbatim** from
[`algal/pi-openai-server-compaction`](https://github.com/algal/pi-openai-server-compaction/tree/main/benchmarks/native-vs-text)
(`benchmarks/native-vs-text/fixtures.ts`, MIT licence). The evaluation prompt,
answer schema shape, answer parsing, exact-match scoring, Wilson intervals and
exact-McNemar test in `src/convert.ts` and `src/analyze.ts` are adapted from the
same benchmark's `run.ts` and `analyze.ts`. Per-file headers record this.

The generator and question design remain upstream, while the injection adapter
adds deterministic answer-neutral padding to make the shipped elision path
eligible. Results are therefore comparable in fixture design, with this
adapter difference disclosed below.

## Fixture

Each fixture is a ~35k-token synthetic project conversation: 165 distractor
exchanges, 325 authoritative state items, provisional values that are later
corrected, and an answer-free shared tail. It carries 75 pre-selected hidden
questions in five categories (15 each):

`exact_recall`, `relational_state`, `tool_history`, `distractor_resolution`,
`task_continuation`.

Scoring is exact string match after trimming and quote stripping — no LLM judge.
**The full-context control must score 75/75 or the fixture is invalid.**

The generator remains verbatim upstream. Before injection, `prepareInjectItems`
adds deterministic, answer-neutral padding to every `function_call_output` that
is below the product's elision threshold. For fixtures 1 through 6, both
`plain` and `echo` currently produce 65 eligible tool outputs out of 65; this
padding changes the injected workload and is part of the benchmark adapter.

The fixtures themselves (the generated fixture JSON, the scripted-user turn
sequences, and the stub outputs under `fixtures/stubs/`) are not included in
this copy — `src/fixtures.ts`'s generator reproduces the fixture shape
deterministically from a seed, so nothing here depends on a specific
checked-in fixture file.

## Arms

| Arm | Procedure before the evaluation turn |
|---|---|
| `full` | none (control) |
| `shake-elide` | `thread/shake/preview` + `thread/shake/start` with `mode: "elide"` |
| `shake-elide-noread` | same, but the evaluation prompt forbids tool use |
| `compact` | `thread/compact/start` |
| `shake-then-compact` | shake elide, then compact |

Arms are **not** output-budget-matched. Each arm's natural downstream input
footprint is reported alongside its accuracy.

Arm order rotates by fixture and trial index so order drift is distributed across
the cells. With a subset of arms, the same rotation applies to that subset; this
is not a five-arm Latin square.

## Requirements

- Node ≥ 22
- `codex-next` on `PATH` (codex-cli 0.153.x with shake support)
- A logged-in `$CODEX_HOME/auth.json` (defaults to `~/.codex/auth.json`)

Each run copies only the required `auth.json` into a private temporary
`CODEX_HOME` outside the results directory and uses a minimal configuration with
no MCP declarations. The temporary home is removed at exit. Sanitized rollout
and artifact evidence is copied into `results/<run>/runtime`; credentials are
not persisted there, and **your real `$CODEX_HOME` is never written to**.

## Usage

```bash
npm install

npm test                                 # all self, analyzer, cost, runner and protocol tests
npm run self-test                       # fixture invariants, conversion, framing

npm run run -- --dry-run                # print the request sequence, launch nothing
npm run run -- --fixtures 1 --trials 1  # smoke: 1 fixture x 1 trial x 5 arms
npm run run -- --fixtures 6 --trials 2  # full run

npm run analyze -- results/<run-dir>
```

### `run.ts` flags

| Flag | Default | Meaning |
|---|---|---|
| `--fixtures N` | `6` | number of fixture seeds (1..N) |
| `--trials N` | `2` | repeats per fixture |
| `--arms a,b` | all five | subset of arms to run |
| `--out DIR` | `results/<timestamp>_<model>` | results directory |
| `--model ID` | codex default | passed as `-c model=ID` and as `thread/start.model` |
| `--variant v` | `plain` | `plain` (upstream) or `echo` (assistant restates each tool result) |
| `--force` | off | re-run cells that already have a result file |
| `--dry-run` | off | print the JSON-RPC sequence without starting codex |

Each `(fixture, trial, arm)` cell writes `trials/<fixture>-<trial>-<arm>.json`
atomically (`.partial` then rename). The manifest records an immutable
fingerprint and the complete `expectedCells` identity matrix. Resume requires
that fingerprint to match; analysis requires every expected cell exactly once,
with matching fixture, trial, arm, model and variant identities.

## Fixture variants

`--variant plain` preserves the upstream history shape: a tool result appears
nowhere except in its `function_call_output`, which is exactly what shake's
elide mode deletes. The injected tool output is padded as described above, so
this is a padded worst-case transcript, not an unmodified upstream payload.

`--variant echo` post-processes the generated history so the assistant restates
each tool result in prose immediately after the exchange — what real agents do.
Assistant messages survive elide, so comparing `shake-elide-noread` accuracy on
the `tool_history` category between the padded variants bounds how much of the
measured loss is an artifact of a transcript that never repeats itself.

Questions and expected answers are identical across variants, and
`src/fixtures.ts` stays verbatim upstream; the variant is a post-processing pass
in `src/convert.ts`.

## Cost model

`npm run estimate` is pure arithmetic over `pricing.json`, no benchmark needed.
Shake makes **no model call** — its cost is the lost input cache on the next
request plus any `read_artifact` round trips — while compaction is billed at the
pre-compaction context. Each scenario separates input tokens added before a
request from generated output tokens, which become uncached input on the next
request. Recovery tool-call output and artifact pages are charged on their
continuation requests, and the final answer output is charged on the final
request. The script prints all four strategy costs over T turns and the
break-even turn count for shake versus no-shake.

```bash
npm run estimate
npm run estimate -- --model gpt-6-astra --context 300000 --post-shake 120000 --post-compact 30000 \
  --turns 5 --per-turn 3000 --output-per-turn 1000 --compaction-output 2000 \
  --reads 3 --recovery-tokens-per-read 768 --recovery-output 100 --cache warm
npm run estimate -- --model gpt-6-astra --context 300000 --post-shake 120000 \
  --hypothetical-no-long-multiplier
```

`pricing.json` records a source URL and fetch date per model. The published
tables include cache writes at 1.25x the uncached input rate and the published
long-context multipliers. GPT-6 Astra explicitly publishes 2x input and cache
rates plus 1.5x output above 272K. The GPT-5.6 entries publish 2x input and
1.5x output; their 2x cache read and write multipliers follow the published
prompt-caching ratios applied to the applicable uncached input rate. Use
`--hypothetical-no-long-multiplier` to print the original no-multiplier
hypothesis separately; it is not published pricing.

Sources: [GPT-6 Astra pricing](https://developers.openai.com/api/docs/models/gpt-6-astra),
[GPT-5.6 Sol pricing](https://developers.openai.com/api/docs/models/gpt-5.6-sol),
[GPT-5.6 Terra pricing](https://developers.openai.com/api/docs/models/gpt-5.6-terra),
[GPT-5.6 Luna pricing](https://developers.openai.com/api/docs/models/gpt-5.6-luna),
and the [prompt-caching guide](https://developers.openai.com/api/docs/guides/prompt-caching).
The generated scenario tables and read-sensitivity analysis are in
[COST_ANALYSIS.md](COST_ANALYSIS.md).

## Offline savings estimate

The release archive includes a small, read-only estimator for existing Codex
JSONL rollouts. With a current `codex-shake` install, run:

```bash
codex-shake-estimate --since 168
```

From a source checkout, the equivalent is:

```bash
python3 codex-rs/docs/shake-bench/scripts/shake-savings-estimate.py --since 168
```

The command reads `$CODEX_HOME/sessions` (or `~/.codex/sessions`) and the
bundled `pricing.json`; it makes no network requests and never changes a
transcript. `--since` selects session files by modification time and models
each selected transcript in full. The default policy mirrors the current fork:
gpt-5.6 shakes at
160,000 input tokens, gpt-6-astra at 40% of its context window, and other
families at 60%, with a 30% minimum removable share, a 4,000-token floor
(2,000 on the escalated pass), and a 16,000-token protected tail.
It models threshold-triggered shakes; the separate prompt-cache-expiry
cold-resume trigger is not inferred.
`--context-window` overrides all recorded values; otherwise a raw
`model_context_window` in `config.toml` wins, then the recorded 95%-usable
window is reconstructed to its raw size, and finally a 1,050,000-token
fallback is used. Tool output sizes use the rough UTF-8 bytes/4 proxy. Credit
values are
projections from the dated Codex rate card printed in the report, not
measurements of plan quota; no weekly quota conversion is reported.

## Outputs

```
results/<timestamp>_<model>/
  manifest.json        model, versions, flags, fingerprint, expected cell matrix
  runtime/<thread>/     sanitized rollout and artifact evidence
  trials/*.json        one file per (fixture, trial, arm) cell
  trials.jsonl         concatenated cells (written by analyze)
  scores.csv           one row per scored question
  summary.json         all aggregates
  GENERATED_RESULTS.md measurement tables
```

`GENERATED_RESULTS.md` reports overall and per-category accuracy per arm, paired
outcomes against the `full` control, a recovery-cost table for the shake arms
(`read_artifact` attempts, derived page-byte estimates when possible, and
extra tokens), shake preview estimates with raw offset and ratio against
realized downstream input, and per-arm mean downstream input tokens and
latency. The analyzer rejects incomplete matrices, failed turns, parse
failures, malformed score sets, and non-perfect full controls before writing
these outputs. A `shake-elide-noread` tool call is reported in inclusive
results and excluded from the conditional compliant view.

The recovery lane enables the app-server code-mode host and requests
`executed_tool_call_metadata`; GPT-6 Astra is measured through this code-mode
path. A disabled code-mode host is a harness configuration failure, not a
recovery result. The runner reads the metadata attached to outer
code-mode execution outputs, deduplicates the complete nested tool-call
inventory, and rejects incomplete or forbidden inventories. This observes the
shipped runtime boundary without changing the product. Metadata records nested
attempts; actual handler-returned bytes are never available. Derived expected
page bytes from immutable artifact content and offsets are estimates only and
do not establish that the handler returned those bytes. A strict manifest
requires complete nested telemetry; an explicit `nestedTelemetryPolicy:
"unavailable"` records null attempt/byte fields and marks nested compliance
unverified rather than treating missing data as zero. For wrapper cells,
nested-tool exclusivity remains unverified when native inventory metadata is
absent; an attempt count alone cannot establish exclusivity.

Full result tables from an actual run are in [`reports/2026-09-11`](reports/2026-09-11)
and [`reports/2026-09-12`](reports/2026-09-12); the headline numbers and
interpretation are in [REPORT.md](REPORT.md), not repeated here.

## Checkpoint reconstruction

`scripts/reconstruct-checkpoint.ts` rebuilds the *dirty working tree* of a Codex
coding session at a chosen cutoff timestamp, so a replay benchmark can start
from the state the session was actually in rather than from a finished commit.
It needs your own session's rollout `.jsonl` files (see "What is and is not
included here" above) — nothing here ships a rollout for you to point it at.

```bash
tsx scripts/reconstruct-checkpoint.ts \
  --base <base-commit-sha> \
  --cutoff 2026-09-08T01:57:36.059Z \
  --out "$BENCH_CHECKPOINTS/<session-id>-r264" \
  --label "checkpoint <session-id>@r264"
```

It exports the base tree with `git archive` (the source repo is never written
to), replays every file-mutating tool call at or before the cutoff in global
timestamp order across the orchestrator rollout *and* any subagent rollouts it
spawned, then wipes `.git` and re-inits so the copy carries exactly one commit
and no history from after the cutoff. `apply_patch` is the real in-product
implementation (arg0 dispatch on the `codex` binary), not a reimplementation of
the patch format. Shell commands are replayed only when they are on the
safe/deterministic allowlist (`just fmt`, `cargo fmt`); everything else that
could write to the worktree is recorded for manual review instead of being
guessed at. The run is idempotent — the output dir is wiped and rebuilt on
entry — and a manifest of the replay lands beside it at `<out>.manifest.json`.

`BENCH_CHECKPOINTS` defaults to `~/bench-checkpoints` if you don't set it.

Reconstruction methodology, correctness validation (does the reconstructed
tree survive comparison against the session's real final commits, does it
compile, is it idempotent) and residual fidelity caveats for one worked
example are in
[`reports/2026-09-11/checkpoint-validation.md`](reports/2026-09-11/checkpoint-validation.md).

## Full-context replay proof (phase 2)

Before any comparative real-session arm is run, the design calls for a
feasibility spike: can a fresh worker, given the session's history up to the
cutoff plus the reconstructed tree, actually finish the remaining work?

```bash
# 1. flatten the rollouts into one single-agent history
tsx scripts/flatten-history.ts \
  --cutoff 2026-09-08T01:57:36.059Z \
  --out fixtures/<session-id>-r264-redacted-trimC \
  --trim fixtures/trim-<session-id>-tierC.json \
  --redact /path/to/repo-under-test=$BENCH_REPLAY/worktree \
  --redact /path/to/harness-checkout=$BENCH_REPLAY/session-cwd \
  --redact codex-artifacts=artifacts-work \
  --redact codex-statusline=session-cwd

# 2. replay from a fresh copy of the checkpoint tree
tsx scripts/replay-01a07e54.ts \
  --tree /path/to/fresh-checkpoint-copy \
  --history fixtures/<session-id>-r264-redacted-trimC.flattened.json \
  --scripted fixtures/scripted-user-01a07e54.json \
  --out results/replay-1        # sandboxed by default; --measure-only to size only

# 3. judge the candidate tree (the worker never sees this script)
scripts/accept-01a07e54.sh /path/to/fresh-checkpoint-copy
```

The tree must be a **git repo** (`cp -a` a checkpoint, not `git archive | tar`)
or the worker cannot commit, and it must live at the path the `--redact`
rewrites name. Verify it REJECTS *before* the run: an accepted starting tree
means the reset did not happen.

Real builds are stubbed: `fixtures/stubs/` (cargo, cargo-nextest, cargo-insta,
just, rustup) returns canned outputs taken from the original session, because a
single real workspace build takes tens of minutes and concurrent ones take the
host down. `scripts/accept-01a07e54.sh` is build-free for the same reason: it
is a set of structural and content assertions specific to this benchmark's
worked example, verified to pass on the session's real final commits and fail
on the bare checkpoint. `accept-01a07e54.sh` and its acceptance criteria are
specific to the `01a07e54` worked example used while developing this harness;
adapt it (or write an equivalent) for your own session.

The worker runs inside `scripts/bench-sandbox.sh`, a bubblewrap jail (on by
default; `--no-sandbox` opts out). PATH stubs alone were not enough — an
earlier replay reached the real `just` by absolute path and shelled out to the
host's own agent-launcher tooling. The jail makes the operator's home
directory a tmpfs, bind-mounts the stub directory *over* `~/.cargo/bin` and
every rustup toolchain `bin/` so absolute-path invocations hit a stub, and
never mounts the real checkouts, the checkpoints, or the sibling replay trees
at all. `scripts/bench-watchdog.sh` remains as a mechanical backstop that kills
any real build the sandbox failed to stop.

`scripts/flatten-history.ts` interleaves the orchestrator's and any subagents'
tool calls by timestamp and drops the multi-agent plumbing (`spawn_agent`,
`send_message`, `followup_task`, `sleep`, `interrupt_agent`, and the subagents'
`agent_message` reports), so the result reads as one agent that did all of the
work itself. It emits the orchestrator-only variant as a secondary history and
writes byte sizes to `<out>.sizes.json`.

`--trim <spec.json>` truncates oversized `function_call_output` items, keeping
a head and a tail and inserting an explicit elision marker, so an oversized
history can be brought down to a size worth paying for. Trimming is a **cost**
lever, not a compaction lever — an over-full context window is a
misconfiguration, not a history-size problem (see replay-proof.md §11.9). A
spec may also carry a `drop` list of `{call, output}` pairs, which deletes a
`function_call` together with its `function_call_output` — both halves are
required and must share a `call_id`, so a drop can never leave a dangling call
or an orphan result, and only those two item types can be dropped at all, which
puts user messages and assistant decision statements structurally out of reach.

Use `scripts/history-sizes.ts` to size candidates and
`replay-01a07e54.ts --measure-only` to get the product's own count; the
`bytes/3.844` proxy in the sizing script is calibrated for the whole payload
and runs a few percent optimistic per span, so the product's counter is the
number that counts. Trimming is never enough on its own — the replay's own
compaction assertion (below) is what decides whether a run was full-context.

**The worker config raises the context window, and this is load-bearing.**
`WORKER_CONFIG` in `replay-01a07e54.ts` sets `model_context_window = 872000`.
Without it, `gpt-6-astra` resolves its window from the catalog's
`context_window` (272,000), *not* its `max_context_window` (872,000) —
`ModelInfo::resolved_context_window()` prefers the former — which puts the
auto-compact limit at 244,800 and compacts every one of these replays before
request 3, whatever the history was trimmed to. Do not remove the override, and
do not set `model_auto_compact_token_limit` alongside it (it is `min()`d in and
can only lower the limit). Full derivation and cites: replay-proof.md §11.9.

`--redact from=to` is not cosmetic. A real session names its repositories by
absolute path in a thousand apply_patch bodies and shell commands, and both
checkouts may still hold the finished branch, so an unredacted history hands
the worker a one-command route to the answer. Redact every path and bare repo
name that resolves to a real checkout. The item shape is the one
`src/convert.ts` already validates and `src/run.ts` already injects with
`thread/inject_items` — no new history format.

The simulated user is **frozen and scripted by default**: a fixed-length turn
sequence replayed verbatim (`fixtures/scripted-user-01a07e54.json` in the
worked example — not included here; see the top of this README for how to
build your own). Turn N is sent as soon as the worker's turn N-1 ends, whatever
the worker said, and the last turn tells it to stop and summarize — so the
exchange count is fixed and two arms that differ only in their injected
history get byte-identical user input. Each turn carries its own rationale and
the fixture carries a changelog, because the wording is part of the benchmark
and gets iterated on deliberately rather than drifting. A live driver behind
`--live-user` (an LLM standing in for the user, from a text brief) is still
there and off by default; its job is to produce candidate scripts, not to
drive a scored run. Neither contains the final diff or any test name.

**Compaction assertion.** `replay-01a07e54.ts` reads the worker's own rollout
after every run and fails the run loudly — a banner on stderr, a
`compaction` block in `replay.json`, exit code 2 — if it finds a `compacted`
record or an input-token drop of more than 50% between consecutive requests.
Two signals rather than one, because a first request can look clean and the
product can fold the thread on the second (replay-proof.md §8.4). Never call a
run full-context without it.

It also asserts the **cause**, not just the symptom: the usable context window
the server reports for the thread (`modelContextWindow` on
`thread/tokenUsage/updated`) must equal the expected value, and the effective
config is read back through `config/read` before the first turn. A dropped or
clamped `model_context_window` override fails the run immediately rather than
quietly producing another compaction arm labelled full-context. An
`auto-compact-*` `turn_context` is recorded but is **not** a violation — that
string is what `Session::next_internal_sub_id` names every internal turn,
including the one `thread/inject_items` mints, so it appears in every replay
regardless.

Setup, measured history sizes, costs, acceptance results and the go/no-go call
for the worked example are in
[`reports/2026-09-11/replay-proof.md`](reports/2026-09-11/replay-proof.md).

**The first full-context replay accepted:** same trimmed history, same frozen
scripted-user turns, same jail and stubs as the runs before it; one line of
worker config different (`model_context_window` raised to match the model's
real max context window). The run produced no compaction anywhere and was
accepted on every structural/content check — closing every gap the bare
checkpoint failed. The prior runs had all compacted early; the cause was the
context-window resolution bug above, not history size — full derivation in
§11.9 of replay-proof.md.

**Three arms have now been run once each on this worked example.** Results,
the quota-measurement method and the cached-vs-uncached analysis are in
[`reports/2026-09-11/arms-2026-09-11.md`](reports/2026-09-11/arms-2026-09-11.md).
Headline: shake with retrieval finished the same task, accepted on every
check, at roughly a fifth of the shadow cost of the full-history arm — because
every one of its requests sits below the long-context pricing threshold — and
it made **zero `read_artifact` calls** doing it. The worker authenticates via
a ChatGPT-style plan login, so the real cost is **plan quota**: the runner
reads `account/rateLimits/read` before the first turn and after the last and
records both snapshots plus the delta. Caveats that bound all of it: n = 1 per
arm, other concurrent Codex sessions can share the same quota (so every quota
delta is an upper bound), the `noread` arm is prompt-discouraged rather than
capability-withheld, and a warm-cache condition needed a separate follow-up
run (see [`reports/2026-09-12/warm-2026-09-12.md`](reports/2026-09-12/warm-2026-09-12.md)
and [`reports/2026-09-12/plan-warm.md`](reports/2026-09-12/plan-warm.md)).

**Go/no-go on the worked example: GO on three arms**, conditionally. The
remaining known gap is that **stubbing the real build changes the task** — the
worker still cannot discover anything by running the suite, so the matrix
compares context strategies on a reduced task, and must say so. Before
launching a full comparative run, the arm runner needs: the `noread` arm made
real (`read_artifact` actually unregistered, not merely discouraged in the
prompt), an explicit cache-condition policy (cold start per arm, checked
against request 1's `cachedInputTokens`), and a definition for a summarization
arm, which does not currently exist in `ARMS`, in the code, or in the model
catalog. §11.12 of replay-proof.md has the detail.

Artifact-recovery usage in practice — whether anyone ever reads a shake
artifact back, and an estimate of what auto-shake would save across real
sessions — is in
[`reports/2026-09-12/recovery-and-savings-2026-09-12.md`](reports/2026-09-12/recovery-and-savings-2026-09-12.md),
produced with `scripts/omp-artifact-census.py` and
`scripts/shake-savings-estimate.py` against local Codex session history.

## Limitations

- Synthetic workload. `tool_history` questions are the stress case for elide;
  real coding sessions differ.
- Tool outputs are padded after generation so the shipped elision threshold is
  exercised. The `plain` and `echo` runs are comparable to each other under this
  adapter, but their injected payloads are not byte-for-byte upstream fixtures.
- `read_artifact` behaviour depends on the model's tool-use propensity, which is
  why the `noread` arm exists — it separates representation loss from retrieval
  failure.
- Recovery metadata can contain several nested `read_artifact` attempts inside
  one outer code-mode continuation. Attempt counts are not model request counts,
  and actual handler-returned bytes are unavailable. Derived expected page bytes
  do not establish handler success.
- Single model, single shake pass. Repeated shake chains are unmeasured.
- This copy excludes raw transcripts, packed checkpoints, and the `fixtures/`
  and `results/` directories from the working checkout that produced the
  numbers above — see "What is and is not included here".

## Licence

MIT, matching the upstream benchmark this work derives from.
