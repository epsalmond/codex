## Automatic and manual Shake

`/shake` removes older tool output without asking a model to summarize it, and saves removed text in local artifact files. Auto-shake is on by default and runs before compaction would; see the [Shake documentation](codex-rs/docs/shake.md).

## Subagent context reduction

Spawned subagents inherit a 272,000-token cap by default, tightened further when the parent has a lower limit. Prepared child requests reduce before sampling when needed and stop if still over their limits, with failed and cancelled attempts reported separately; see [subagent context reduction](codex-rs/docs/shake.md#subagents).

## Subagent picker details

The `/subagents` picker shows each child’s status, latest reported context usage, and a short preview of its latest final response when idle. See [monitoring subagents](README.md#monitoring-subagents).

## Wake mode for multi-agent orchestrators

Interactive MultiAgentV2 roots and eligible V2 child agents at every depth now wake on reports. Exec roots themselves continue polling their direct children with `wait_agent`. Set `agent_polling = "enabled"` to retain polling throughout the tree. Esc pauses root wakeups and holds reports for the next user message. See [wake mode](README.md#wake-mode-for-multi-agent-orchestrators) and the [nested wake release notes](releases/2026-10-02-nested-wake-mode.md).

In `codex exec` and in subagents, `wait_agent` without `timeout_ms` now waits up to 300 seconds (interactive sessions keep 30 seconds), returns as soon as an agent reports, and returns at once when no child is running; a configured `default_wait_timeout_ms` applies everywhere.

## Subagents on a different model provider

An agent role's `config_file` can now set `model_provider`, so `spawn_agent` runs that role's children against a self-hosted or otherwise alternate provider while the parent keeps its own login. See [subagents on a different model provider](README.md#subagents-on-a-different-model-provider) and its [detailed notes](releases/2026-09-26-subagent-provider.md).

## Statusline entries

The TUI statusline can show the weekly usage reset time and the completion time of the latest live response. See [statusline items](README.md#statusline-items).

## Installation and self-update

The fork installs as `codex-shake` beside the official `codex` binary and can update to the latest fork release. See the [installation instructions](README.md#installing-codex-shake).

## Offline savings estimate

`codex-shake-estimate` estimates Shake savings from local rollout files without sending them to a service. See the [estimator documentation](codex-rs/docs/shake-bench/README.md#offline-savings-estimate).

## Getting the most out of codex-shake

Ask for delegation, hand off long waits, and turn repeated work into scripts:

- "Delegate the flaky-test investigation to a subagent and keep going." In an interactive session, its result arrives as a new turn; a `codex-shake exec` root collects it with `wait_agent`.
- "Give each failing package to its own subagent and summarize their reports as they arrive."
- For CI or a long build, ask for one waiting script in place of repeated status checks: "Run a script that waits for CI to finish, with the longest yield." Each check on the running script is still a model request, spaced up to 300 seconds apart by default, so one long wait keeps requests to a minimum.
- Interactive sessions only: "Hand the CI wait to a subagent, then end your turn." The subagent's report starts your next turn. A `codex-shake exec` run ends with its root turn, so keep the wait in the root there.
- When a sequence of commands repeats, ask for a script: "Turn these steps into a script and use it from now on."
- Give each subagent one self-contained task and ask for a short report; it shakes and compacts within its own context budget.
- To steer a running turn, type your message and press Enter. Press Esc to interrupt the root turn; in wake mode this also pauses wakeups, and child results are held until your next message.
- Run `/shake` when a session grows heavy; the preview shows what it frees and how soon it pays back before you confirm.
- Ask codex-shake to search the file a `[shaken …]` placeholder names when you need a removed output again.

See the [quickstart](README.md#what-codex-shake-does-by-default) for the defaults behind these patterns.
