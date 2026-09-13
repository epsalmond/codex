# Shake benchmark: headline results

One real Codex session (536k-token history, a Rust implementation task) was
replayed from a frozen checkpoint with the same scripted user, once with the
full history and once after a shake. A judge script checks whether the worker
reaches the same end state (12 structural checks). Builds are stubbed. All
runs used gpt-6-astra. Full detail: [reports/2026-09-11/arms-2026-09-11.md](reports/2026-09-11/arms-2026-09-11.md)
and [reports/2026-09-12/warm-2026-09-12.md](reports/2026-09-12/warm-2026-09-12.md).

## Same outcome, a third of the credits

| arm | history after arm | credits (standard) | end state |
|---|---:|---:|---|
| full history, cold cache | 536k | 840 | 12/12 |
| shaken, cold cache | 176k | 312 | 11/12 (one clone, worker variance) |
| full history, warm cache | 536k | 809 | 12/12 |
| shaken, warm cache | 176k | 358 | 12/12 |

Shaken arms cost 2.3x to 2.7x fewer credits per run. Nine of ten shaken runs
across both matrices reached 12/12; the one miss is the same kind of variance
seen between two full-history runs.

## What a shake costs

Shaking a warm thread costs exactly one uncached request of the survivor size.
Only a 9,216-token head stays cached. The thread is fully warm again on the
next request. Priming overhead measured: 129 credits full history, 38 credits shaken.

## Quota meters total tokens, not uncached tokens

Cache warming does not reduce plan quota; token reduction does. Five
attributable runs: quota moved with total input tokens in every case, even at
99.8% cached.

## Nobody reads the elided output back

Zero artifact reads in nine shaken runs. A census of 346 local oh-my-pi
sessions found 17 shake artifacts and zero read-backs. Recovery is a safety
valve that has never opened, which is why the fork ships a path-only
placeholder and no read tool. See [reports/2026-09-12/recovery-and-savings-2026-09-12.md](reports/2026-09-12/recovery-and-savings-2026-09-12.md).

## What auto-shake would have saved in one busy week

| metric | value |
|---|---:|
| threads | 182, of which 152 would have shaken |
| input tokens | 12.37B |
| tokens saved | 2.04B |
| credits | 24,574 of 179,654 |

Estimated by replaying real rollouts through the auto-shake policy at a 272k
window (`scripts/shake-savings-estimate.py`). Avoided compactions are not
modeled, so this is a floor.

## Earlier synthetic study

Before the real-session replay, a synthetic fixture with planted facts measured
retention directly: 100% with retrieval, 81% without, 100% when the assistant
had already restated the facts. See [REPORT.md](REPORT.md).

## Caveats

n=1 per cell on the real session, one worker model, stubbed builds, one
transcript shape. Debugging and planning shapes are the planned next runs.
