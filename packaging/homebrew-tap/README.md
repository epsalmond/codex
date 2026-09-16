# codex-shake Homebrew tap (bootstrap tree)

This directory is a placeholder for `epsalmond/homebrew-codex-shake`, the
Homebrew tap for `codex-shake` (the `eric/local-features` fork build of Codex
CLI, published alongside an official `codex` install).

`Formula/codex-shake.rb` in that tap repo is **generated**, not hand-edited.
It is (re)rendered on every published `eric/local-features` release by the
"Update Homebrew tap" step in
[`.github/workflows/fork-release.yml`](../../.github/workflows/fork-release.yml),
via [`.github/scripts/render-brew-formula.sh`](../../.github/scripts/render-brew-formula.sh):

```sh
.github/scripts/render-brew-formula.sh \
  <release-tag> <source-version> <darwin-arm64-sha256> <linux-x86_64-sha256>
```

CI extracts the two sha256 values from the release's `SHA256SUMS` asset,
renders the formula, and pushes it to the tap's default branch as commit
`codex-shake <release-tag>` (skipping the push when the rendered formula is
byte-identical to what's already there). This requires a
`HOMEBREW_TAP_TOKEN` repository secret with push access to
`epsalmond/homebrew-codex-shake`; when that secret is unset the step emits a
`::notice::` and skips, so publishing the release itself is never blocked.

## `Formula/codex-shake.rb` in this directory

The formula checked into this directory was rendered for the latest
published release at the time this tree was created:

- Release tag: `local-features-v0.154.0-main-r20260914110830.00000000000028b2ec76d3de4ab8`
- Source version: `0.154.0`
- sha256 values taken from that release's `SHA256SUMS` asset

Use it to bootstrap `epsalmond/homebrew-codex-shake`: create that repo, copy
this `Formula/codex-shake.rb` into it, commit, and push. From then on CI
keeps it up to date automatically; this checked-in copy is not refreshed by
CI and will drift -- treat it as a one-time seed, not a mirror.

## Usage once the tap exists

```sh
brew install epsalmond/codex-shake/codex-shake
brew upgrade epsalmond/codex-shake/codex-shake
```
