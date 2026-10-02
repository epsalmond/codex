## Automatic and manual Shake

`/shake` removes older tool output without asking a model to summarize it, and saves removed text in local artifact files. Automatic Shake can run at the pre-sampling point before compaction; see the [Shake documentation](codex-rs/docs/shake.md).

## Subagent context reduction

Spawned subagents inherit a 272,000-token Shake and compaction cap by default, tightened further when the parent has a lower limit. Within a turn, a child shakes first and compacts if needed; see [subagent context reduction](codex-rs/docs/shake.md#subagents).

## Wake mode for multi-agent orchestrators

Interactive MultiAgentV2 roots and eligible V2 child agents at every depth now wake on reports. Exec roots themselves continue polling their direct children. Set `agent_polling = "enabled"` to retain polling throughout the tree. See [wake mode](README.md#wake-mode-for-multi-agent-orchestrators) and the [nested wake release notes](releases/2026-10-02-nested-wake-mode.md).

## Subagents on a different model provider

An agent role's `config_file` can now set `model_provider`, so `spawn_agent` runs that role's children against a self-hosted or otherwise alternate provider while the parent keeps its own login. See [subagents on a different model provider](README.md#subagents-on-a-different-model-provider) and its [detailed notes](releases/2026-09-26-subagent-provider.md).

## Statusline entries

The TUI statusline can show the weekly usage reset time and the completion time of the latest live response. See the [statusline documentation](README.md#local-statusline-build).

## Installation and self-update

The fork installs as `codex-shake` beside the official `codex` binary and can update to the latest fork release. See the [installation instructions](README.md#installing-codex-shake).

## Offline savings estimate

`codex-shake-estimate` estimates Shake savings from local rollout files without sending them to a service. See the [estimator documentation](codex-rs/docs/shake-bench/README.md#offline-savings-estimate).
