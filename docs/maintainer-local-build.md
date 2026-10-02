# Maintainer-local build

This page describes the fork maintainer's personal setup on their own
machines. To install and use the fork, follow the
[codex-shake quickstart](../README.md#quickstart) instead.

## Local statusline build

This local fork carries two TUI-only status-line items: `weekly-reset` (an
absolute local reset time) and `last-response-clock` (the latest successful
live response in the current TUI session). `codex-update` updates the official
npm install, fetches the matching `rust-v<version>` tag, rebases this branch
onto that exact tag, and only then builds and atomically switches the local
binary. It deliberately stops on a missing tag or rebase conflict.

`codex` launches the managed local build; `codex-official` launches the npm
installation directly as an escape hatch.
