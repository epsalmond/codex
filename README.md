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

### Installing and running Codex CLI

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

### Local statusline build

This local fork carries two TUI-only status-line items: `weekly-reset` (an
absolute local reset time) and `last-response-clock` (the latest successful
live response in the current TUI session). `codex-update` updates the official
npm install, fetches the matching `rust-v<version>` tag, rebases this branch
onto that exact tag, and only then builds and atomically switches the local
binary. It deliberately stops on a missing tag or rebase conflict.

`codex` launches the managed local build; `codex-official` launches the npm
installation directly as an escape hatch.

Fork test builds are published as `codex-shake` beside the official `codex`.
See [Installing codex-shake](#installing-codex-shake) for install and update
commands, and the [fork release notes](https://github.com/epsalmond/codex/releases)
for changes in each build.

### Installing codex-shake

Install the latest fork build on macOS or Linux with:

```sh
curl -fsSL https://raw.githubusercontent.com/epsalmond/codex/eric/local-features/install.sh | sh
```

Homebrew is also available on macOS and Linux x86_64:

```sh
brew install epsalmond/codex-shake/codex-shake
```

Debian and Ubuntu x86_64 users can download the latest `.deb` from the
[GitHub releases](https://github.com/epsalmond/codex/releases) and install it
with `sudo dpkg -i ./codex-shake_<version>_amd64.deb`. The script installer
adds `codex-shake` and `codex-shake-update` alongside the official `codex`.
Run `codex-shake-update` to install a newer fork release. Homebrew users can
run `brew upgrade epsalmond/codex-shake/codex-shake`. Debian and Ubuntu users
can download and reinstall the newest `.deb`.

### Identifying Shake feature support

Run `codex --version` (or `codex-shake --version`) to see both the upstream
Codex version and the Shake feature version and exact fork release tag. `-V`
continues to show only the upstream Codex version. Binaries reporting Shake
feature version `0.3.0` include opt-in event-driven root wakeups when subagents
finish. The unpublished `0.2` milestone covered subagent shaking and compaction
plus intra-turn shaking. Version `0.3.0` is the honorary bump that recognizes
both milestones.

The release tag is the reliable feature identity. An older binary can accept
`wait_agent_enabled = false` and remove `wait_agent` from the root instructions
without waking the root when a child finishes.

### Wake mode for multi-agent orchestrators

In wake mode, a MultiAgentV2 root sleeps until a child subagent reports back,
instead of polling `wait_agent` in a loop. Turn it on with:

```toml
[features.multi_agent_v2]
wait_agent_enabled = false
```

or for a single run, `codex -c features.multi_agent_v2.wait_agent_enabled=false`.

Esc pauses wakeups and holds any child results that arrive; the TUI shows "N
child results queued — delivered with your next message", and they are
delivered together with the next user message. Subagents keep `wait_agent`
regardless of this setting, and a nested parent (a subagent with its own
children) does not wake yet; that is a later stage.

This is off by default. With it off, behavior matches upstream. See the
[release notes](releases/2026-09-26-wake-mode.md) for details and measurements.

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

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Business, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
