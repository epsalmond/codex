# Run plan — warm-cache batch, 2026-09-12

Five runs, **strictly sequential**, on the account's **standard tier**. The operator's
call, 2026-09-12: do **not** request the Fast tier for this batch (no
`--service-tier`, so `thread/start` sends no `serviceTier` and the worker's
`config.toml` carries no `service_tier` key). Standard rates are 2.5x cheaper
than Fast, which is most of why the batch is affordable at 82% of the 7-day
window already used. The four arms in arms-2026-09-11.md were Fast, so the
**cost columns here are not directly comparable to theirs** — compare the
credit-per-request rules, which are tier-independent up to the 2.5x.
Everything else is held exactly as in
[arms-2026-09-11.md](../2026-09-11/arms-2026-09-11.md): Tier C flattened history
(`fixtures/01a07e54-r264-redacted-trimC.flattened.json`, 907 items, 536,357
tokens), the frozen six-turn scripted user v1, `model_context_window = 872000`,
the bubblewrap jail, the stubbed toolchain, and a worktree reset from
`$BENCH_CHECKPOINTS/01a07e54-r264` and verified **REJECTED at 12**
before each launch.

**Why sequential and not scheduled around a reset window.** The only window
`account/rateLimits/read` reports for this account is the 7-day one
(`windowDurationMins = 10080`, resetting 2026-09-15T01:22:07Z); `secondary` is
absent on every read. The hour-scale reset the operator watches **is not observable
through this channel**, so there is nothing to time the batch against. Run them
back to back and record `resetsAt` on both sides of each run, which is what
marks a delta invalid if a window does roll over mid-run.

## Preconditions, before every single run

1. **Classified process census** — `python3 scripts/codex-census.py`.
   Exit 0 means no other process on the personal account (`$CODEX_HOME`) is alive.
   Processes on another account (a second Codex home) spend other quota and do not
   pollute this run; they are listed but do not block.

   **This batch runs with `--allow-busy-host`, deliberately.** Sixteen
   personal-account codex processes are alive and the operator has classified them as
   effectively inactive: idle foreground TUIs attached to live bash parents on
   ptys (`Sl+`, ~0.4% CPU), not orphans, and **none is to be killed**. Being
   alive is not the same as spending, so the attributability question is settled
   per run by evidence instead of by process count:

2. **Other-session activity check, per run** —
   `python3 scripts/rollout-watch.py snapshot` before and
   `... compare <snapshot>` after. A codex session writes its rollout on every
   model request, so a rollout under `$CODEX_HOME/sessions` whose mtime advances
   while the run is in flight is direct evidence of concurrent spend on this
   account. If any advanced, the run's **quota column is marked
   "unattributable"**; the cost columns and acceptance are unaffected. The
   worker's own rollout lives under the run's `CODEX_HOME`
   (`bench-replay/results/codex-homes/…`), never under `$CODEX_HOME`, so it cannot
   be mistaken for another session's.
3. **Worktree reset and rejected**:
   ```bash
   rm -rf $BENCH_REPLAY/worktree
   cp -a $BENCH_CHECKPOINTS/01a07e54-r264 $BENCH_REPLAY/worktree
   scripts/accept-01a07e54.sh $BENCH_REPLAY/worktree   # expect REJECTED at 12
   ```
   (`$BENCH_CHECKPOINTS` is read-only source material — copy from it,
   never into it.)
4. **A `codex --version` sanity check**: every run in the matrix so far is
   `codex-cli 0.153.4`. A different worker binary is a different experiment.

## The five runs, in order

Common flags, written once:

`scripts/run-warm-arm.sh` does every step of the protocol above for one arm —
census, rollout snapshot, worktree reset, REJECTED precondition, replay,
activity verdict, acceptance, candidate tree, both cost columns — and is
idempotent, so the five runs are five invocations of it:

```bash
cd shake-bench
scripts/run-warm-arm.sh --label B3-full-cold        --arm full        --cache cold
scripts/run-warm-arm.sh --label A2-shake-cold       --arm shake-elide --cache cold
scripts/run-warm-arm.sh --label Bw-full-warm-self   --arm full        --cache warm --warm self
scripts/run-warm-arm.sh --label Aw-shake-warm-session --arm shake-elide --cache warm --warm session
scripts/run-warm-arm.sh --label Aw-shake-warm-self  --arm shake-elide --cache warm --warm self
```

The underlying invocations, written out, share:

```bash
COMMON="--history fixtures/01a07e54-r264-redacted-trimC.flattened.json \
  --tree $BENCH_REPLAY/worktree \
  --scripted fixtures/scripted-user-01a07e54.json \
  --allow-busy-host"
```

### (a) full arm, cold — the third repetition of B

```bash
npx tsx scripts/replay-01a07e54.ts $COMMON \
  --arm full --cache cold \
  --out $BENCH_REPLAY/results/run-B3-full-cold
```

n goes from 2 to 3 on the only arm that has a variance estimate, which is what
§11.12's three-repetition minimum asks for first.

### (b) shake + retrieval arm, cold — the second repetition of A

```bash
npx tsx scripts/replay-01a07e54.ts $COMMON \
  --arm shake-elide --cache cold \
  --out $BENCH_REPLAY/results/run-A2-shake-cold
```

A has **no** variance estimate at all today. This is the cheapest row in the
plan and the one that makes A's 773 credits quotable as more than a single
sample.

### (c) full arm, warm self

```bash
npx tsx scripts/replay-01a07e54.ts $COMMON \
  --arm full --cache warm --warm self \
  --out $BENCH_REPLAY/results/run-Bw-full-warm-self
```

Expected: **request 1 cached fraction > 0.8** (the runner checks and logs it).
This is the full arm's steady state with the cold-start request taken out.

### (d) shake + retrieval arm, warm session

```bash
npx tsx scripts/replay-01a07e54.ts $COMMON \
  --arm shake-elide --cache warm --warm session \
  --out $BENCH_REPLAY/results/run-Aw-shake-warm-session
```

The live-session condition: prime on the **full** history, then shake, then run.
Expected and **asserted-by-logging**: request 1 **misses** (cached fraction
< 0.05, because the shake rewrote the prefix that was primed) and request 2
recovers (> 0.8). This is the run that prices what a shake costs a session that
was already warm — the number a product decision actually needs.

### (e) shake + retrieval arm, warm self

```bash
npx tsx scripts/replay-01a07e54.ts $COMMON \
  --arm shake-elide --cache warm --warm self \
  --out $BENCH_REPLAY/results/run-Aw-shake-warm-self
```

The shake arm's steady state, and — with (c) — the pair that moves the cached
fraction independently of history size. That is the **only** experiment that can
separate total-token from uncached-token quota metering, which n = 1 cold arms
structurally cannot (arms-2026-09-11.md: both A and B sit at ~97.7% cached, so
total and uncached move together, 2.49x against 2.58x).

## After every run

```bash
scripts/accept-01a07e54.sh $BENCH_REPLAY/worktree \
  | tee $BENCH_REPLAY/logs/accept-run-<label>.txt        # expect ACCEPTED 12/12
cp -a $BENCH_REPLAY/worktree $BENCH_REPLAY/candidate-run<label>
python3 scripts/arms-row.py <label> $BENCH_REPLAY/results/run-<label>/replay.json
python3 scripts/run-cost.py     $BENCH_REPLAY/results/run-<label>/replay.json
```

`arms-row.py` prints both cost columns, the quota delta, the classified census
counts, the cache condition with request 1's cached fraction, and the priming
overhead as its own line.

## Expected cost

Measured rules, from arms-2026-09-11.md and run 9, **restated at STANDARD
rates** (gpt-6-astra 250 / 25 / 1,250 credits per 1M input / cached input /
output, official Codex pricing fetched 2026-09-12; **no 2.5x**, since this batch
does not request Fast):

- full arm: request 1 ≈ **129 credits** (516,268 uncached input), then ≈ **14.4
  credits** per request, ~39–44 requests.
- shake arm: request 1 ≈ **38 credits** (152,140 uncached input), then ≈ **5.4
  credits** per request, ~51–59 requests.
- a warm run's priming request costs what the cold-start request it replaces
  would have cost — 129 credits priming the full history, 38 priming a shaken
  one — plus a handful of output tokens.

| # | run | priming | arm | **total, standard credits** | API shadow $ |
|---|---|---:|---:|---:|---:|
| a | full, cold | — | ~711 | **~711** | ~$57 |
| b | shake+retrieval, cold | — | ~309 | **~309** | ~$12 |
| c | full, warm self | ~129 | ~596 | **~725** | ~$58 |
| d | shake+retrieval, warm session | ~129 | ~309 | **~438** | ~$25 |
| e | shake+retrieval, warm self | ~38 | ~276 | **~314** | ~$12 |
| | **batch total** | **~296** | **~2,201** | **≈ 2,500 credits** | **≈ $164** |

Call it **≈ 2,500 standard credits**, and budget **≈ 2,800** for the ~10–12%
run-to-run spread measured on the expensive arm (B vs B2) — 2.5x less than the
same batch on Fast, which is the reason for the tier choice. The dollar column is
a shadow cost on the API rate card; **nobody is billed it** — the credits are the
real spend.

Wall clock: worker turns ran 422–604 s per arm, plus ~2–4 minutes of injection
and preparation each, so the batch is **≈ 75–90 minutes** back to back.

## Stop conditions

- **A 7-day window reading at or above 97% before a run starts** → stop and
  report; do not start the run.
- **A quota read error** on either snapshot → stop and report.
- **Any harness failure** → stop, fix once, continue.
- Live personal-account processes are **no longer a stop condition** (operator
  decision, 2026-09-12): proceed with `--allow-busy-host` and let the per-run rollout
  check decide whether that run's quota number is attributable.
- **Cache condition NOT MET** on a warm run (logged, not fatal) → the run is
  still usable, but it must be reported as its observed condition, not its
  requested one.
- **Compaction assertion failure** → since the 2026-09-12 fix this can no longer
  be the shake false positive, so treat it as a real finding: the run is not
  full-context and must not be quoted as one.
- **Quota read error, or `resetCrossed: true`** on either window → that run's
  delta is invalid; the run still counts for cost and acceptance.
