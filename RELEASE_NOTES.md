<!-- This file is the notes section of every GitHub release for this branch.
     codex-fork-release adds the install block before it and the base tag and
     commit list after it. Plain markdown. -->

**Codex Shake keeps long coding sessions smaller by removing older tool output
from active context. In our replay benchmark, shaken runs used 2.3–2.7× fewer
Codex credits.**

Surviving conversation content stays byte-identical, and removed content remains
available in local artifact files. Auto-shake runs by default; `/shake` lets you
preview and trigger it yourself.

Shake v0.2.0 adds an explicit `/smart-compact` action for bounded smart-compaction
handoffs on OpenAI's gpt-6-astra family. A fresh gpt-5.6-luna pass preserves the goals,
decisions, open threads, files, commands, and user context that matter after a
shake, while the mechanical artifacts remain the source of truth if the pass
is unavailable.

The published replay reduced active history from 536k to 176k tokens. Its
measured results were:

| arm | active history | standard Codex credits | end state |
|---|---:|---:|---|
| full history, cold cache | 536k | 840 | 12/12 |
| shaken, cold cache | 176k | 312 | 11/12 |
| full history, warm cache | 536k | 809 | 12/12 |
| shaken, warm cache | 176k | 358 | 12/12 |

This was n=1 per cell, with builds stubbed; the one cold shaken run missed one
structural check. An earlier replay projected 4.8x lower API cost using the API
rate card ($59.46/$12.37); it used 2.42× fewer Codex credits.

No artifact recovery reads were observed in the nine runs covered by the
recovery report. Cache behavior and savings vary by workload; see the
benchmark methodology.

The weekly projection estimated 2.04B fewer input tokens out of 12.37B across
182 historical threads. It is a projection from rollouts, not a plan-quota
measurement.

- Headline numbers: [RESULTS.md](codex-rs/docs/shake-bench/RESULTS.md)
- Docs and config: [shake.md](codex-rs/docs/shake.md)
- Benchmark scripts and full reports: [shake-bench](codex-rs/docs/shake-bench/README.md)
