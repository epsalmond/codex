<p align="center"><strong>Codex CLI</strong> is a coding agent from OpenAI that runs locally on your computer.
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>
</br>
If you want Codex in your code editor (VS Code, Cursor, Windsurf), <a href="https://developers.openai.com/codex/ide">install in your IDE.</a>
</br>If you want the desktop app experience, run <code>codex app</code> or visit <a href="https://chatgpt.com/codex?app-landing-page=true">the Codex App page</a>.
</br>If you are looking for the <em>cloud-based agent</em> from OpenAI, <strong>Codex Web</strong>, go to <a href="https://chatgpt.com/codex">chatgpt.com/codex</a>.</p>

---

## Quickstart

This fork ships Codex CLI as `codex-shake`, installed beside the official
`codex`. Fork releases cover Apple Silicon macOS and x86_64 Linux. Windows
x86_64 has a [development build](#windows-development-build); for other
platforms, use [upstream Codex CLI](#installing-upstream-codex-cli).

### Installing codex-shake

Install the latest fork release with:

```sh
curl -fsSL https://raw.githubusercontent.com/epsalmond/codex/main/install.sh | sh
```

The script installs releases under `~/.local/share/codex-shake` and adds
`codex-shake` and `codex-shake-update` to `~/.local/bin`. Homebrew is also
available:

```sh
brew install epsalmond/codex-shake/codex-shake
```

On Debian or Ubuntu x86_64, download the latest `.deb` from the
[GitHub releases](https://github.com/epsalmond/codex/releases) and install it
with `sudo dpkg -i ./codex-shake_<version>_amd64.deb`.

To update, run `codex-shake-update` (script install),
`brew upgrade epsalmond/codex-shake/codex-shake` (Homebrew), or install the
newest `.deb`. The [fork release notes](https://github.com/epsalmond/codex/releases)
list the changes in each build.

Run `codex-shake` and sign in as described in
[Using Codex with your ChatGPT plan](#using-codex-with-your-chatgpt-plan).
`codex-shake` shares `~/.codex` (config, sign-in, and sessions) with the
official `codex`.

### Building codex-shake from a commit

To try a commit before its release is published, build and install it on the
machine that will run it (Linux x86_64 or macOS arm64):

```sh
scripts/shake-local-install.sh --dry-run <commit>   # print version, tag, and plan
scripts/shake-local-install.sh <commit>
scripts/shake-local-install.sh --host mbp <commit>  # run it on another machine over ssh
```

The script builds in a fresh worktree with the same steps as the release
workflow and installs the result next to the published releases under
`~/.local/share/codex-shake/<tag>`. It then points `current` at it and records
the switch in `last-local-deploy.json`. The tag sorts just below the official
release for the same commit, so `codex-shake-update` moves to that release
once it is published. It needs Rust and Python 3.11 or newer. On Linux it also
needs `pkg-config` and `libcap-dev` to build `bwrap`. Without them, it reuses
the installed release's `bwrap` when the `bwrap` sources haven't changed since
that release. The build worktree is removed on success unless you pass
`--keep-worktree`.

To roll back, point `current` at an earlier release directory. The previous
target is `oldTarget` in `last-local-deploy.json`:

```sh
ln -sfn <older-tag> ~/.local/share/codex-shake/current
```

The first build of each commit gets few sccache hits. Development builds keep
the workspace version at `0.0.0`, and stamping the release version changes
every workspace crate, so only third-party dependencies come from the cache.

### Windows development build

Install [GitHub CLI](https://cli.github.com) and run `gh auth login` first.
Download [codex-shake-update.bat](https://raw.githubusercontent.com/epsalmond/codex/main/codex-shake-update.bat)
and [codex-shake-update.ps1](https://raw.githubusercontent.com/epsalmond/codex/main/codex-shake-update.ps1)
into the folder where you want Codex installed. In PowerShell, run:

```powershell
cd 'C:\Tools\codex-shake'
.\codex-shake-update.bat
.\bin\codex.exe --version
```

For later updates, close any running Codex processes and run the batch file
from that folder again. It downloads the latest successful `fork-ci.yml`
push build on `main`, verifies the ZIP's SHA-256 checksum, and
extracts into your current folder, overwriting matching files. The ZIP
includes both updater scripts. These are development builds; GitHub Actions
artifacts expire after 14 days, so an expired artifact needs a new successful
push build.

### What codex-shake does by default

- **Auto-shake** frees context before compaction is needed. When a session
  reaches its model's threshold, or resumes after the prompt cache has
  expired, it replaces older tool outputs with short placeholders, without
  asking a model to summarize. Each removed output is saved under
  `~/.codex/artifacts/<thread-id>/`, and its placeholder names that file so
  the model can search it. To turn auto-shake off for every model, add this to
  `~/.codex/config.toml`:

  ```toml
  [auto_shake]
  threshold = "off"

  [auto_shake.models."gpt-5.6"]
  threshold = "inherit"

  [auto_shake.models."gpt-6-astra"]
  threshold = "inherit"
  ```

  See [Shake](codex-rs/docs/shake.md#auto-shake) for triggers, per-model
  thresholds, and manual `/shake`.
- **Subagent context reduction** keeps each subagent under a context cap: a
  child shakes, then compacts, when it reaches the cap. See
  [subagents](codex-rs/docs/shake.md#subagents).
- **Wake mode** lets an agent wait for its subagents without polling: each
  child report starts a new turn on its parent. This covers interactive roots,
  `codex-shake exec` roots, and MultiAgentV2 child agents at every depth; exec
  stays open until the whole agent tree is idle. See
  [wake mode](#wake-mode-for-multi-agent-orchestrators).
- **Automatic process and cell completion** delivers the result of a
  background command or a running Code Mode script once it finishes, so the
  model never polls for it. See
  [automatic process and cell completion](#automatic-process-and-cell-completion).

For prompts that put these to work, see
[Getting the most out of codex-shake](RELEASE_NOTES.md#getting-the-most-out-of-codex-shake).

### Statusline items

The default TUI status line uses `model` (a short model label when the catalog
provides one), `current-dir`, `thread-name`, `weekly-reset`, and
`last-response-clock`. The bundled model catalog includes short labels for
common Codex models, including `luna`, `sol`, and `astra`. A custom catalog can
override a label with `short_name`; when it is absent, the full `display_name`
is shown.

`weekly-reset` shows the local time your weekly usage window resets, and
`last-response-clock` shows when the latest live response in this session
completed. These items stay hidden until they have data; `weekly-reset` never
shows for API-key logins, which have no rate-limit windows. The optional
`context-window-usage` item shows current context tokens over the model's
context window (for example, `143K/823K`), while `active-subagents` shows the
number of working descendants of the displayed thread. Add optional items via
`/statusline` or `tui.status_line`. To remove default items, uncheck them in
`/statusline`, or set your own list in `tui.status_line` in
`~/.codex/config.toml`.

### Identifying Shake feature support

Run `codex-shake --version` to see both the upstream
Codex version and the Shake feature version and exact fork release tag. `-V`
continues to show only the upstream Codex version. Binaries reporting Shake
feature version `0.3.0` include event-driven wakeups for interactive CLI and
app-server MultiAgentV2 roots by default. This change extends wakeups to
eligible MultiAgentV2 child agents at every depth; check the exact release tag
to confirm whether a binary includes the extension. In those binaries Exec
roots still poll for their direct children; later fork releases wake and drain
Exec roots by default too (see wake mode below). The unpublished `0.2`
milestone covered subagent shaking and compaction plus intra-turn shaking. Version `0.3.0` is the
honorary bump that recognizes both milestones.

The release tag is the reliable feature identity. Older binaries do not
recognize the `agent_polling` string setting.

### Wake mode for multi-agent orchestrators

Wake mode lets an orchestrator delegate work and spend no requests while it
waits: each child report starts a new turn on its parent. MultiAgentV2 is enabled
by default, and `agent_polling` defaults to `"disabled"`, so interactive CLI
and app-server roots use wake mode without extra configuration. A V2 child
created with `ThreadSpawn` also wakes when its own children report, at any
depth, including when it was spawned under an Exec root. A `codex exec` root
wakes too: it ends its turn while children work, each child report starts its
next turn, and exec stays open until the whole agent tree is idle. Exec then
prints the last completed root turn's answer and exits 1 if any root turn failed
or was interrupted; `--json` emits a `turn.started` event per root turn,
followed by `turn.completed`, or `turn.failed` for a failed turn. The first
Ctrl-C interrupts the running root turn; a Ctrl-C between turns, or a second
one, stops waiting and exits 1.

To keep polling throughout the agent tree, set:

```toml
[features.multi_agent_v2]
agent_polling = "enabled"
```

or for a single run, `codex-shake -c 'features.multi_agent_v2.agent_polling="enabled"'`.
This also turns off the `codex exec` drain: the exec root polls with `wait_agent`
and exec exits after one root turn. With polling enabled, `wait_agent` without
`timeout_ms` waits up to 300 seconds in `codex exec` and in subagents
(interactive sessions keep 30 seconds), returns as soon as an agent reports, and
returns at once when no child is running; a configured `default_wait_timeout_ms`
applies everywhere. In the default wake mode agents have no `wait_agent`, so this
default does not apply. Polling agents keep reports and messages across Esc: the
next turn takes them, and an interrupted `followup_task` waits for the next
explicit follow-up rather than restarting on its own. A report to an evicted
parent reloads that parent while the Codex process runs.

Esc pauses automatic wakeups and holds child results arriving at the root; the TUI
shows "N child results queued — delivered with your next message". A child
with unfinished delegated work waits without reporting completion, then
resumes when a descendant reports. `send_message` only queues a message;
`followup_task` resumes a waiting child. Only an ancestor (parent, grandparent,
or root) can start a turn in an agent: `followup_task` from a sibling, cousin,
or descendant is refused, and their `send_message` mail waits for the
recipient's next turn, even in a durable sleep. Set `sibling_wake = true` under
`[features.multi_agent_v2]` to let any agent wake another. An interrupted child
attempt reports its interruption to the parent; automatic wakeups stay paused
until an explicit follow-up. While paused, a `send_message` from an ancestor or
descendant still wakes an agent in a durable sleep, and that turn leaves held
child results for the next message.
When an automatic turn fails to start, its accepted messages stay queued and no
turn starts automatically, durable sleep included, until an explicit follow-up
or user turn.

Wake-mode agents keep `clock.curr_time`, while `clock.sleep` and
`collaboration.wait_agent` are omitted. Code Mode's `wait` is replaced by
`exec_terminate`, because a running script's result is delivered
automatically; see
[automatic process and cell completion](#automatic-process-and-cell-completion). Set
`agent_polling = "enabled"` to keep polling tools throughout the tree.
Pending reports are retried when capacity frees, and evicted agents can reload
while the Codex process stays running; delivery across a process restart is
not guaranteed.

For the separate Shake and compaction cap on child context, see [subagent
context reduction](codex-rs/docs/shake.md#subagents). See the [nested wake
release notes](releases/2026-10-02-nested-wake-mode.md) for behavior and
boundaries, and the [original root wake rollout](releases/2026-09-26-wake-mode.md)
for its measurements.

### Automatic process and cell completion

When a background command started with `exec_command` or a Code Mode `exec`
script is still running at the end of its tool call, its result is delivered
to the agent automatically once it finishes. An idle agent wakes with a new
turn; a busy one receives the result with its next model request. The model
does not poll:

- Code Mode exposes `exec_terminate({cell_id})` instead of `wait`. Call it to
  stop a script you no longer need; it returns the output so far, and no
  automatic result follows. If the script already finished, it reports that
  the result was delivered automatically.
- An empty `write_stdin` from the model to a running non-TTY session returns
  "session N is running; its exit status and output tail will be delivered
  automatically." Writing input still works, and so do empty polls of
  interactive TTY sessions, which get no automatic delivery. A Code Mode script
  can still poll its own sessions with `tools.write_stdin`.

Each result is a single bounded message: a header with the call, the
`kind=process` or `kind=cell` producer, its status (`exited(N)`, `completed`,
`error`, `cancelled`, `timedOut`), and `truncated=<bytes>`, followed by the
output's tail. The whole message, including the tail and its metadata, is at
most 3072 bytes; `truncated` counts the tail bytes omitted. Write large results to a file or `store()` them for a later `exec`. A
script's images and audio are not delivered; the message notes how many were
omitted.

Esc pauses automatic delivery at the root: results that arrive afterwards are
held for your next message, like child reports in
[wake mode](#wake-mode-for-multi-agent-orchestrators). Interrupting a turn
reports its running scripts as `cancelled`.

The root agent and wake-mode subagents get automatic delivery without polling
tools. Subagents with `agent_polling = "enabled"` keep `wait` and empty polls.
Code Mode keeps `wait` when its host cannot report that a script finished.

To keep `wait` and empty `write_stdin` polls while still delivering results
automatically, add this to `~/.codex/config.toml`:

```toml
[features.async_process_completion]
polling_tools = "keep"
```

To turn automatic delivery off entirely and restore polling:

```toml
[features]
async_process_completion = false
```

### Typed subagent reports

A MultiAgentV2 subagent reports to its parent and other ancestors only through
its `report` tool, for one of three reasons:

- `blocked`: it hit something unexpected and needs instructions. Its turn ends,
  the parent receives a `BLOCKED` report, and a `followup_task` resumes it.
- `answer`: it replies to a question an ancestor sent it with `send_message`.
  Each such message allows one answer; without one the call is refused. The
  answer reaches a running ancestor at its next step and wakes an idle one.
- `final`: it is done. Its turn ends and the parent receives the result as a
  `FINAL_ANSWER`, exactly as when a turn ends without `report`.

A subagent's `send_message` or `followup_task` to an ancestor is refused;
sibling, cousin, and descendant messaging is unchanged. `spawn_agent` takes
`lifetime = "task" | "persistent"`; without it the agent type's `lifetime`
applies (`[agents.<name>] lifetime = "persistent"` or `lifetime` in the role
file), then `task`. A `task` agent is closed after its final report reaches the
parent, so later messages to it are refused and new work goes to a fresh agent.
A `persistent` agent stays idle and resumable. A subagent turn that reaches 150
model calls is told once to finish the current step and report.

```toml
[features.multi_agent_v2]
typed_reports = false           # free-form reports, finished agents stay resumable
wrap_up_after_model_calls = 0   # no wrap-up reminder (default 150)
```

### Monitoring subagents

Run `/subagents` to monitor child runs. Each row shows whether a child is mid-turn, idle, closed, or in error, along with its latest reported context tokens against the model window when available. Idle children also show a short preview of their latest final response.

Press `1`–`9` to select a numbered row (`1` is Main), or use arrows and Enter. Press `/` to search names, paths, or IDs; Esc clears search before closing the picker. Ctrl+U clears the query while keeping search active (Cmd+Delete in Ghostty on macOS). Press `X` to archive the selected child and its descendants; the picker keeps the same row selected, clamped to the last remaining row.

With an empty prompt and no popup or overlay, Option/Alt+Left and Option/Alt+Right switch to the previous or next visible agent, including Main. Terminals that send Alt+b/f for these keys are supported, including macOS terminals connected over SSH; with text in the prompt, the keys retain word editing.

### Subagents on a different model provider

An agent role can point its children at a different model provider than the
one the parent session is using. The root config declares the provider under
`[model_providers.<id>]` (self-hosted or otherwise), and the role's
`config_file` sets `model_provider = "<id>"` plus `model`:

```toml
# ~/.codex/config.toml (root)

[model_providers.self_hosted]
name = "Self-hosted vLLM"
base_url = "https://vllm.internal.example.com/v1"
wire_api = "responses"
env_key = "SELF_HOSTED_API_KEY"
requires_openai_auth = false

[agents.local]
config_file = "agents/local.toml"
```

```toml
# ~/.codex/agents/local.toml

model_provider = "self_hosted"
model = "my-org/local-coder-7b"
model_context_window = 32000
```

`spawn_agent(agent_type="local")` then runs that child against `self_hosted`,
while the parent keeps its own ChatGPT login and provider.

A role may only reference a provider id already defined in the root config's
`[model_providers]`; it cannot define a new `[model_providers.*]` table
inline. If a role names an id that isn't declared at the root, `spawn_agent`
fails with a clear error naming the role and the missing provider.
`wire_api = "responses"` is required for any provider a role targets; Chat
Completions is removed from this fork.

Model names for an alternate provider aren't checked against a live catalog,
so a role that switches providers should declare `model_context_window` (and
optionally `model_auto_compact_token_limit`) so context tracking still works.
Auth for the alternate provider follows its own `env_key` or `auth` setting,
independent of the parent's ChatGPT login. See the
[release notes](releases/2026-09-26-subagent-provider.md) for resolution
rules and current limitations.
A provider-switching role cannot fork the parent's history; spawn it with
`fork_turns = "none"` (the default).

### Automatic goal continuation

When Goals is enabled, the host checks admitted automatic goal turns for empty
output, short literal waiting messages, and repeated action/result digests.
The default `defer` mode holds continuation after two consecutive suspicious
turns. Repeated actions first establish a baseline, so that pattern needs three
turns. A hold persists across restart and keeps the goal Active; descendants and
budget accounting continue.

New user input, explicit goal activation, a changed objective, or a newly accepted
agent result can recover a hold. Plain turn starts, maintenance, and generic
process/cell completion do not release it. This does not add a process-completion
wake mechanism.

Tune policy in `~/.codex/config.toml`:

```toml
[goals]
continuation_guard_mode = "defer" # off, observe, or defer
stall_after_no_progress_turns = 2
stall_waiting_text_max_chars = 512 # positive; at most 512 normalized Unicode characters
stall_unlinked_timer_recognition = true # fixed literal-timer grammar; no script evaluation
# stall_waiting_prefixes = ["waiting", "awaiting"] # replaces the packaged vocabulary
```

Waiting prefixes apply to final and commentary messages. Unset uses the versioned
[packaged profile](codex-rs/ext/goal/config/continuation-defaults.v1.json); `[]`
disables the prefix rule. Replacements allow at most 32 nonblank literals, each
at most 64 Unicode characters as written. Matching lowercases, collapses
whitespace, and normalizes curly apostrophes. Invalid packaged policy disables
stall assessment and retains the legacy empty-response guard; it has no duplicate
Rust vocabulary fallback.

Settings are frozen when a new runtime starts or resumes. Restart and resume to
apply edits. `observe` warns without holding; `off` skips assessment. Both retain
the legacy three-empty-response Blocked guard, and loading either releases only
detector holds, preserving fork deferrals. Disabling literal-timer recognition
leaves unlinked Code Mode cells Unknown.

Each new runtime treats prior suspicion history as Unknown because older binaries
may have done unobserved work. Pre-threshold streaks restart; persisted holds and
accepted-result ID deduplication remain until authorized recovery. Deduplication
retains the 32 most recent accepted identities. Loading changed guard settings
invalidates compatible prior detector history and releases detector holds,
including when the new mode remains `defer`; fork deferrals remain separate.

This is a conservative heuristic, not a proof of progress. Short prefix matches
can hold productive messages, and changing outputs can hide a stall. Unknown or
incomplete observations break suspicion. Hard resource caps remain fixed (256
calls, 512-byte IDs, 256-byte tool names, and 4096-byte persisted state). Calibration
used two bad windows from one incident family, with no independent-family
validation; manual controls were Unknown and one exceeded the call cap. The
automatic returned-results control did not hold. Tune with the same effective
profile in live execution and replay rather than assuming general accuracy.

### Hooks observe control-tool waits

`PreToolUse` hooks also receive Code Mode `exec` cells, Code Mode
`exec_terminate` and `wait`, and unified exec `write_stdin`, including empty
polls. These events carry
`"observational": true`: hooks run and their `additionalContext` reaches the
model, but deny, exit code 2, and `updatedInput` are ignored, so the call runs
with its original input and result. Observational hooks run only when a hook
matches, and they get 1 second before they are stopped. `tool_use_id` is the control call's own
call id, which matches its output in the rollout. Payloads carry only bounded
metadata:

| `tool_name` | `tool_input` |
|---|---|
| `write_stdin` | `session_id`, `stdin_empty`, `stdin_bytes`, `yield_time_ms`, optional `max_output_tokens`, `exec_call_id` (the originating `exec_command` call), and `"chars": ""` only for empty polls. Stdin text is never sent. |
| `exec_terminate` | `cell_id`. |
| `wait` | `cell_id`, `yield_time_ms`, `terminate`, optional `max_tokens`. Only with `polling_tools = "keep"`, `async_process_completion = false`, or a polling subagent. |
| `exec` | `source_bytes`, `source_sha1`, and optional pragma `yield_time_ms`/`max_output_tokens`. Source text is never sent. |

The command a `write_stdin` call continues keeps its single `Bash`
`PostToolUse`. To stop these events, add this to `~/.codex/config.toml`:

```toml
[features]
control_tool_hook_observation = false
```

### Installing upstream Codex CLI

The official OpenAI release installs as `codex`.

Run the following on Mac or Linux to install Codex CLI:

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

Run the following on Windows to install Codex CLI:

```shell
powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"
```

The standalone installers download from `https://releases.openai.com/codex` by default and fall back to GitHub Releases if a metadata or asset download is unavailable. To force GitHub Releases, set `CODEX_INSTALLER_USE_RELEASES_OPENAI_COM` to `false` (`0` and `no` are also accepted):

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_INSTALLER_USE_RELEASES_OPENAI_COM=false sh
```

```powershell
$env:CODEX_INSTALLER_USE_RELEASES_OPENAI_COM='false'; irm https://chatgpt.com/codex/install.ps1 | iex
```

Codex CLI can also be installed via the following package managers:

```shell
# Install using npm
npm install -g @openai/codex
```

```shell
# Install using Homebrew
brew install --cask codex
```

Then simply run `codex` to get started.

<details>
<summary>You can also go to the <a href="https://github.com/openai/codex/releases/latest">latest GitHub Release</a> and download the appropriate binary for your platform.</summary>

Each GitHub Release contains many executables, but in practice, you likely want one of these:

- macOS
  - Apple Silicon/arm64: `codex-aarch64-apple-darwin.tar.gz`
  - x86_64 (older Mac hardware): `codex-x86_64-apple-darwin.tar.gz`
- Linux
  - x86_64: `codex-x86_64-unknown-linux-musl.tar.gz`
  - arm64: `codex-aarch64-unknown-linux-musl.tar.gz`

Each archive contains a single entry with the platform baked into the name (e.g., `codex-x86_64-unknown-linux-musl`), so you likely want to rename it to `codex` after extracting it.

</details>

### Using Codex with your ChatGPT plan

Run `codex-shake` (or `codex` for the upstream CLI) and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Business, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Maintainer notes

The maintainer's local build workflow (`codex-update`, `codex-official`, and a
`codex` command that launches a local build) is described in
[Maintainer-local build](./docs/maintainer-local-build.md). Using codex-shake
requires none of it.

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
