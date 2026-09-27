## Automatic and manual Shake

`/shake` removes older tool output without asking a model to summarize it, and saves removed text in local artifact files. Automatic Shake can run at the pre-sampling point before compaction; see the [Shake documentation](codex-rs/docs/shake.md).

## Subagent context reduction

Spawned subagents inherit a 272,000-token Shake and compaction cap by default, tightened further when the parent has a lower limit. Within a turn, a child shakes first and compacts if needed; see [subagent context reduction](codex-rs/docs/shake.md#subagents).

## Wake mode for multi-agent orchestrators

With wake mode enabled, a MultiAgentV2 root sleeps until a child reports back instead of polling `wait_agent`; Esc holds results for the next user message. See [wake mode](README.md#wake-mode-for-multi-agent-orchestrators) and its [detailed notes](releases/2026-09-26-wake-mode.md).

## Statusline entries

The TUI statusline can show the weekly usage reset time and the completion time of the latest live response. See the [statusline documentation](README.md#local-statusline-build).

## Installation and self-update

The fork installs as `codex-shake` beside the official `codex` binary and can update to the latest fork release. See the [installation instructions](README.md#installing-codex-shake).

## Offline savings estimate

`codex-shake-estimate` estimates Shake savings from local rollout files without sending them to a service. See the [estimator documentation](codex-rs/docs/shake-bench/README.md#offline-savings-estimate).
