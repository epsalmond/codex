## Automatic and manual Shake

`/shake` removes older tool output without asking a model to summarize it, and saves removed text in local artifact files. Auto-shake is on by default and runs before compaction would; see the [Shake documentation](codex-rs/docs/shake.md).

## Subagent context reduction

Spawned subagents have a 272,000-token active-context cap by default, alongside inherited limits. Every prepared ordinary child request, including retries, reduces before sampling when needed and stops if still at or above its limits, with failed and cancelled attempts reported separately; see [subagent context reduction](codex-rs/docs/shake.md#subagents).

## Subagent picker details

The `/subagents` picker shows each child’s status, captured context policy and freshness, reduction outcomes, and latest final response when idle. Highlight a row for details. Use numbered selection, `/` search and Esc to leave search, Ctrl+U to clear the query (Cmd+Delete in Ghostty on macOS), and `X` to archive a child while preserving the selected row. Option/Alt+Left/Right also switch visible agents with an empty prompt and no popup or overlay, including terminals that send Alt+b/f over SSH. `/status` shows Shake’s sealed-history-item watermark. See [monitoring subagents](README.md#monitoring-subagents).

## Wake mode for multi-agent orchestrators

Interactive MultiAgentV2 roots, `codex exec` roots, and eligible V2 child agents at every depth now wake on reports. Set `agent_polling = "enabled"` to retain polling throughout the tree. Esc pauses root wakeups and holds reports for the next user message. See [wake mode](README.md#wake-mode-for-multi-agent-orchestrators) and the [nested wake release notes](releases/2026-10-02-nested-wake-mode.md).

With `agent_polling = "enabled"`, `wait_agent` without `timeout_ms` in `codex exec` and in subagents now waits up to 300 seconds (interactive sessions keep 30 seconds), returns as soon as an agent reports, and returns at once when no child is running; a configured `default_wait_timeout_ms` applies everywhere. Wake-mode agents have no `wait_agent`, so the default does not apply to them.

## Exec drains its agent tree by default

`codex exec` roots now wake on child reports, and exec stays open until the agent tree is idle, including after `exec resume` and `exec fork`. It prints the last completed root turn's answer and exits 1 if any root turn failed or was interrupted. `--json` emits a `turn.started` event for every root turn, followed by `turn.completed`, or `turn.failed` for a failed turn. The first Ctrl-C interrupts the running root turn; a Ctrl-C between turns, or a second one, stops waiting and exits 1.

To turn it off, set `features.multi_agent_v2.agent_polling = "enabled"` (for a single run, `codex-shake exec -c 'features.multi_agent_v2.agent_polling="enabled"'`). That restores `wait_agent` polling for the whole agent tree, exec included, and exec exits after one root turn, as before. An interactive session resumed with `codex exec resume` keeps polling, because exec cannot observe its work.

Errors without an error code and permission path-conversion failures are now reported as failed turns. Exec stops the failed turn's child work before reporting completion. See [#97](https://github.com/epsalmond/codex/issues/97).

## Subagents on a different model provider

An agent role's `config_file` can now set `model_provider`, so `spawn_agent` runs that role's children against a self-hosted or otherwise alternate provider while the parent keeps its own login. See [subagents on a different model provider](README.md#subagents-on-a-different-model-provider) and its [detailed notes](releases/2026-09-26-subagent-provider.md).

## Statusline entries

The TUI statusline shows the weekly usage reset time and the completion time of the latest live response, and both are now on by default unless you've customized the status line. If you have, add them via `/statusline` or `tui.status_line` in `~/.codex/config.toml`. Otherwise, remove them the same way. See [statusline items](README.md#statusline-items).

## Installation and self-update

The fork installs as `codex-shake` beside the official `codex` binary and can update to the latest fork release. See the [installation instructions](README.md#installing-codex-shake).

## Offline savings estimate

`codex-shake-estimate` estimates Shake savings from local rollout files without sending them to a service. See the [estimator documentation](codex-rs/docs/shake-bench/README.md#offline-savings-estimate).

## `codex exec` output failures

`codex exec` now exits 1 when it cannot write the `--output-last-message` file, after normal shutdown, so CI no longer treats a missing output file as success.

## Getting the most out of codex-shake

Ask for delegation, hand off long waits, and turn repeated work into scripts:

- "Delegate the flaky-test investigation to a subagent and keep going." Its result arrives as a new turn, in an interactive session or a `codex-shake exec` run.
- "Give each failing package to its own subagent and summarize their reports as they arrive."
- For CI or a long build, ask for one waiting script in place of repeated status checks: "Run a script that waits for CI to finish, with the longest yield." Each check on the running script is still a model request, spaced up to 300 seconds apart by default, so one long wait keeps requests to a minimum.
- "Hand the CI wait to a subagent, then end your turn." The subagent's report starts your next turn. In a `codex-shake exec` run the root can end its turn too: exec drains the agent tree and wakes the root with the reports. With `agent_polling = "enabled"`, exec restores the old polling behaviour and exits after one root turn, so keep the wait in the root there.
- When a sequence of commands repeats, ask for a script: "Turn these steps into a script and use it from now on."
- Give each subagent one self-contained task and ask for a short report; it shakes and compacts within its own context budget.
- To steer a running turn, type your message and press Enter. Press Esc to interrupt the root turn; in wake mode this also pauses wakeups, and child results are held until your next message.
- Run `/shake` when a session grows heavy; the preview shows what it frees and how soon it pays back before you confirm.
- Ask codex-shake to search the file a `[shaken …]` placeholder names when you need a removed output again.

See the [quickstart](README.md#what-codex-shake-does-by-default) for the defaults behind these patterns.
