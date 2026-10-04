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
`codex`. Fork builds cover Apple Silicon macOS and x86_64 Linux; for other
platforms, use [upstream Codex CLI](#installing-upstream-codex-cli).

### Installing codex-shake

Install the latest fork release with:

```sh
curl -fsSL https://raw.githubusercontent.com/epsalmond/codex/eric/local-features/install.sh | sh
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
  child report starts a new turn on its parent. This covers interactive roots
  and MultiAgentV2 child agents at every depth; a `codex-shake exec` root
  still polls its direct children. See
  [wake mode](#wake-mode-for-multi-agent-orchestrators).

For prompts that put these to work, see
[Getting the most out of codex-shake](RELEASE_NOTES.md#getting-the-most-out-of-codex-shake).

### Statusline items

codex-shake adds two TUI status-line items: `weekly-reset` (the local time
your weekly usage window resets) and `last-response-clock` (when the latest
live response in this session completed). Both are on by default, after the
upstream defaults (`model-with-reasoning`, `current-dir`, `thread-name`), unless
you've customized the status line. If you have, add them via `/statusline` or
`tui.status_line`. Each
stays hidden until it has data; `weekly-reset` never shows for API-key logins,
which have no rate-limit windows. To remove them, uncheck them in
`/statusline`, or set your own list in `tui.status_line` in
`~/.codex/config.toml`.

Two more items are available from `/statusline` or `tui.status_line`:
`context-window-usage` shows current context tokens over the model's context
window (for example, `143K/823K`), and `active-subagents` shows the number of
working descendants of the displayed thread. The existing `thread-name` item
shows the session name set by `/rename`.

To use a compact model label in the `model` item, add an optional `short_name`
to that model's entry in the configured `model_catalog_json`. For example,
`"short_name": "luna"` displays `luna` in the statusline while other model
labels continue to use `display_name`.

### Identifying Shake feature support

Run `codex-shake --version` to see both the upstream
Codex version and the Shake feature version and exact fork release tag. `-V`
continues to show only the upstream Codex version. Binaries reporting Shake
feature version `0.3.0` include event-driven wakeups for interactive CLI and
app-server MultiAgentV2 roots by default. This change extends wakeups to
eligible MultiAgentV2 child agents at every depth; check the exact release tag
to confirm whether a binary includes the extension. Exec roots themselves
still poll for their direct children. The unpublished `0.2` milestone covered
subagent shaking and compaction plus intra-turn shaking. Version `0.3.0` is the
honorary bump that recognizes both milestones.

The release tag is the reliable feature identity. Older binaries do not
recognize the `agent_polling` string setting.

### Wake mode for multi-agent orchestrators

Wake mode lets an orchestrator delegate work and spend no requests while it
waits: each child report starts a new turn on its parent. MultiAgentV2 is enabled
by default, and `agent_polling` defaults to `"disabled"`, so interactive CLI
and app-server roots use wake mode without extra configuration. A V2 child
created with `ThreadSpawn` also wakes when its own children report, at any
depth, including when it was spawned under an Exec root. The Exec root itself
continues polling its direct children with `wait_agent`.
In `codex exec` and in subagents, `wait_agent` without `timeout_ms` waits up to
300 seconds (interactive sessions keep 30 seconds), returns as soon as an agent
reports, and returns at once when no child is running; a configured
`default_wait_timeout_ms` applies everywhere.

To keep polling throughout the agent tree, set:

```toml
[features.multi_agent_v2]
agent_polling = "enabled"
```

or for a single run, `codex-shake -c 'features.multi_agent_v2.agent_polling="enabled"'`.

Esc pauses automatic wakeups and holds child results arriving at the root; the TUI
shows "N child results queued — delivered with your next message". A child
with unfinished delegated work waits without reporting completion, then
resumes when a descendant reports. `send_message` only queues a message;
`followup_task` resumes a waiting child. An interrupted child attempt reports
its interruption to the parent; automatic wakeups stay paused until an
explicit follow-up.

Wake-mode agents keep `clock.curr_time` and Code Mode's `wait` tool, while
`clock.sleep` and `collaboration.wait_agent` are omitted. Set
`agent_polling = "enabled"` to keep polling tools throughout the tree.
Pending reports are retried when capacity frees, and evicted agents can reload
while the Codex process stays running; delivery across a process restart is
not guaranteed.

For the separate Shake and compaction cap on child context, see [subagent
context reduction](codex-rs/docs/shake.md#subagents). See the [nested wake
release notes](releases/2026-10-02-nested-wake-mode.md) for behavior and
boundaries, and the [original root wake rollout](releases/2026-09-26-wake-mode.md)
for its measurements.

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
