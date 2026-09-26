<!-- This file is the notes section of every GitHub release for this branch.
     codex-fork-release adds the install block before it and the base tag and
     commit list after it. Plain markdown. -->

**Codex Shake keeps long coding sessions smaller by removing older tool output
from active context. In our replay benchmark, shaken runs used 2.3–2.7× fewer
Codex credits.**

Surviving conversation content stays byte-identical, and removed content remains
available in local artifact files. Auto-shake runs by default; `/shake` lets you
preview and trigger it yourself.

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

**Subagent context reduction caps how large a spawned or resumed child's
context can grow.** By default, children shake and then compact once active
context reaches 272,000 tokens or the limit they would otherwise inherit from
the parent, whichever is lower; the root session is unaffected. Nested
children inherit the same cap.

Tune or disable it with `[subagent_context_reduction]` in
`~/.codex/config.toml`: `enabled` (default `true`) and `threshold_tokens`
(default `272000`, must be positive). If a subagent is still over the limit
after shaking and compacting, its turn ends with a context-window-exceeded
error instead of looping, and the parent sees this as a failed child turn.
`list_agents` exposes a `context` field per agent (`active_tokens`, `basis`,
`last_reduction`) so a parent can inspect a child's context state directly.

See [shake.md](codex-rs/docs/shake.md#subagents) for the full behavior.
