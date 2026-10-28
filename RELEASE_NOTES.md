## Agents see and close every open subagent

`list_agents` now lists every open subagent, including idle ones whose runtime was unloaded (status `"unloaded"`), so the root no longer mistakes unloaded children for closed ones. `/subagents` reads the same lifecycle from the agent tree, so an unloaded idle child shows as idle rather than closed. A new top-level `close_agents` tool lets an agent close named descendants, or every idle one with `["idle"]`; set `close_agents_tool = false` under `[features.multi_agent_v2]` to hide it. Press `c` in `/subagents` to close every idle child. See [closing subagents](README.md#closing-subagents).

## Typed subagent reports

Subagents now message their parent and other ancestors only through a `report_to_parent` tool: `blocked` (stops and waits for a follow-up), `answer` (replies to a question an ancestor sent with `send_message`), or `final` (done). Their `send_message` and `followup_task` to ancestors are refused, and a turn that ends without `report_to_parent` still delivers its last message as the final report. An agent type's `lifetime` sets how long its agents live: a `task` agent (the default) is closed after its final report, so new work goes to a fresh agent; a `persistent` agent stays resumable. A subagent turn that reaches 150 model calls is told once to wrap up. Set `typed_reports = false` under `[features.multi_agent_v2]` to restore free-form reports; `wrap_up_after_model_calls = 0` turns off the reminder. See [typed subagent reports](README.md#typed-subagent-reports).

## Closing subagents from `/subagents`

Closing a subagent in `/subagents` (or archiving/deleting it from the agents overview) now closes it and its whole descendant subtree in the agent tree: it leaves the status-bar count, `list_agents`, and cannot be cold-restored. Later messages or follow-ups to it tell the model the agent is closed and to spawn a fresh one; `/subagents` context sizes now use compact counts like `272K`.

## Only ancestors wake subagents

`followup_task` now starts a turn only when the sender is the target's parent, grandparent, or root; siblings and cousins get an error and use `send_message`, which queues without waking the recipient or its durable sleep. `spawn_agent` without `fork_turns` now starts the child without parent history. Set `sibling_wake = true` under `[features.multi_agent_v2]` to restore sibling wakes.

## Automatic process and cell completion

A command or Code Mode script can now declare `work_kind: "service"` for
persistent work. Finite tasks remain the default and hold exec's drain through
completion delivery. Services allow the drain to finish while retaining owned
session shutdown and bounded terminal diagnostics. Code Mode uses its existing
`// @exec: {"work_kind":"service"}` directive. See
[Task and Service behavior](README.md#automatic-process-and-cell-completion)
([#93](https://github.com/epsalmond/codex/issues/93)).

A background command or running Code Mode script now reports its result to the agent once it finishes, so the model stops spending requests on checks. Code Mode's `wait` is replaced by `exec_terminate`, and empty `write_stdin` polls of non-TTY sessions are refused. See [automatic process and cell completion](README.md#automatic-process-and-cell-completion) and the [release notes](releases/2026-10-14-automatic-cell-completion.md) ([#79](https://github.com/epsalmond/codex/issues/79)).

## Recorded child reports after compaction

Wake-mode parents now get a fresh delivery when compaction drops a recorded trigger report, preventing repeated automatic turns on a report the model can no longer see. [Details](releases/2026-10-14-recorded-report-compaction.md) ([#175](https://github.com/epsalmond/codex/issues/175)).

## Automatic and manual Shake

`/shake` removes older tool output without asking a model to summarize it, and saves removed text in local artifact files. Auto-shake is on by default and runs before compaction would; see the [Shake documentation](codex-rs/docs/shake.md).

## Subagent context reduction

Spawned subagents have a 272,000-token active-context cap by default, alongside inherited limits. Every prepared ordinary child request, including retries, reduces before sampling when needed and stops if still at or above its limits, with failed and cancelled attempts reported separately; see [subagent context reduction](codex-rs/docs/shake.md#subagents).

## Subagent picker details

The `/subagents` picker shows each child’s status, captured context policy and freshness, reduction outcomes, and latest final response when idle. Highlight a row for details. Use numbered selection, `/` search and Esc to leave search, Ctrl+U to clear the query (Cmd+Delete in Ghostty on macOS), and `X` to archive a child while preserving the selected row. Option/Alt+Left/Right also switch visible agents with an empty prompt and no popup or overlay, including terminals that send Alt+b/f over SSH. `/status` shows Shake’s sealed-history-item watermark. See [monitoring subagents](README.md#monitoring-subagents).

## Wake mode for multi-agent orchestrators

Interactive MultiAgentV2 roots, `codex exec` roots, and eligible V2 child agents at every depth now wake on reports. Set `agent_polling = "enabled"` to retain polling throughout the tree. Esc pauses root wakeups and holds reports for the next user message. See [wake mode](README.md#wake-mode-for-multi-agent-orchestrators) and the [nested wake release notes](releases/2026-10-02-nested-wake-mode.md).

With `agent_polling = "enabled"`, `wait_agent` without `timeout_ms` in `codex exec` and in subagents now waits up to 300 seconds (interactive sessions keep 30 seconds), returns as soon as an agent reports, and returns at once when no child is running; a configured `default_wait_timeout_ms` applies everywhere. Wake-mode agents have no `wait_agent`, so the default does not apply to them.

## Durable sleep wakes while reports are paused

A `send_message` again wakes an agent in a durable sleep while Esc has paused its wakeups; held child reports still wait for the next message. A failed automatic start still waits for an explicit follow-up. A follow-up accepted while that start is failing resumes the queued work as soon as the failed start releases its slot. Only accepted follow-ups and user turns count as recovery; rejected deliveries keep the failure pause. See the [release notes](releases/2026-10-14-durable-sleep-paused-wake.md).

## Polling agents keep reports across interrupts

With `agent_polling = "enabled"`, reports and messages already accepted by a turn now survive Esc and arrive with the next turn; an interrupted `followup_task` waits for the next explicit follow-up. See the [release notes](releases/2026-10-14-polling-mail-retention.md).

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

## Hooks observe control-tool waits

`PreToolUse` hooks now see Code Mode `exec` and `wait` calls and unified exec `write_stdin` polls as observational events that cannot block or rewrite them, so loop detectors can spot repeated waits. See [hooks observe control-tool waits](README.md#hooks-observe-control-tool-waits) and the [release notes](releases/2026-10-14-control-tool-hook-observation.md).

## Getting the most out of codex-shake

Ask for delegation, hand off long waits, and turn repeated work into scripts:

- "Delegate the flaky-test investigation to a subagent and keep going." Its result arrives as a new turn, in an interactive session or a `codex-shake exec` run.
- "Give each failing package to its own subagent and summarize their reports as they arrive."
- For CI or a long build, ask for one waiting script and then end the turn: "Run a script that waits for CI to finish, then stop until it reports." The script's result arrives on its own when it finishes, so the wait costs no model requests.
- "Hand the CI wait to a subagent, then end your turn." The subagent's report starts your next turn. In a `codex-shake exec` run the root can end its turn too: exec drains the agent tree and wakes the root with the reports. With `agent_polling = "enabled"`, exec restores the old polling behaviour and exits after one root turn, so keep the wait in the root there.
- When a sequence of commands repeats, ask for a script: "Turn these steps into a script and use it from now on."
- Give each subagent one self-contained task and ask for a short report; it shakes and compacts within its own context budget.
- To steer a running turn, type your message and press Enter. Press Esc to interrupt the root turn; in wake mode this also pauses wakeups, and child results are held until your next message.
- Run `/shake` when a session grows heavy; the preview shows what it frees and how soon it pays back before you confirm.
- Ask codex-shake to search the file a `[shaken …]` placeholder names when you need a removed output again.

See the [quickstart](README.md#what-codex-shake-does-by-default) for the defaults behind these patterns.
