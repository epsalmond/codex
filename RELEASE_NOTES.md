<!-- This file is the "What this fork adds" section of every GitHub release for
     this branch. codex-fork-release adds the changelog before it and the
     install block, provenance, and commit list after it. Plain markdown. -->

**Shake keeps long coding sessions smaller by removing older tool output from
active context.** In one real-session replay benchmark, shaken runs used
2.3–2.7× fewer standard Codex credits than the same session run in full.
Surviving conversation content stays byte-identical, and removed content
remains available in local artifact files; auto-shake runs by default and
`/shake` previews or triggers it manually. Full benchmark table and
methodology: [RESULTS.md](codex-rs/docs/shake-bench/RESULTS.md). Config and
internals: [shake.md](codex-rs/docs/shake.md). Benchmark scripts and reports:
[shake-bench](codex-rs/docs/shake-bench/README.md).

**Subagent context reduction** caps how large a spawned or resumed child's
context can grow: by default, a child shakes and then compacts once active
context reaches 272,000 tokens or the limit it would otherwise inherit from
the parent, whichever is lower, via the `[subagent_context_reduction]` config
table. See [shake.md](codex-rs/docs/shake.md#subagents) for the full behavior.

**Statusline items** add two TUI-only `/statusline` entries: `weekly-reset`
(compact local wall-clock time the weekly usage window resets) and
`last-response-clock` (local time of the most recently completed live
response in the session).

**Self-update and packaging** installs this branch's snapshots as
`codex-shake`, beside the official `codex`, via `curl | sh`
([install.sh](install.sh)), Homebrew
(`brew install epsalmond/codex-shake/codex-shake`), or a `.deb` for
Debian/Ubuntu x86_64 attached to each release. `codex-shake-update` checks and
reinstalls the latest tag; `codex-shake-estimate` runs the offline savings
estimator against local rollouts.

**Release automation** builds, validates, and publishes a prerelease whenever
a merge lands on `eric/local-features` (a fast-forward push whose new tip is a
two-parent merge commit) or a canonical `local-features-v*` tag is pushed as a
manual recovery path: two-platform binaries with attached debug symbols, a
`.deb`, and a Homebrew tap update, gated on the fork's own test suite. See
[fork-release.yml](.github/workflows/fork-release.yml).
