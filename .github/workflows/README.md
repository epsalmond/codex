# Workflow Strategy

The workflows in this directory are split so that pull requests get fast, review-friendly signal while `main` still gets the full cross-platform verification pass.

## Fork PR gate

In `epsalmond/codex`, `fork-ci.yml` replaces the upstream checks that require
unavailable runner groups. It runs formatting, clippy, and dev builds, plus a
focused Linux nextest gate for the confirmed issue #238 regressions, including
the footer-selection snapshot also reported in issue #232. Broader baseline
test triage and coverage remain tracked in issue #155.

Run the same focused gate from the repository root:

```bash
bash .github/scripts/test_issue_238.sh
```

Install cargo-nextest first (CI pins 0.9.103). The runner needs Python 3, the normal
Linux build dependencies and verified rusty_v8 archive/bindings used by
`.github/actions/setup-rusty-v8`. On the maintainer's host, initialize the isolated
worktree's build environment with `codex-worktree env` before running it.
It builds `codex-code-mode-host` and `test_stdio_server` freshly from the current
checkout, then reuses that target directory, target triple, and Cargo profile for
the tests. The default profile is `dev`, matching the preceding CI build; set
`CODEX_ISSUE_238_CARGO_PROFILE=dev-small` to reuse a local `dev-small` build.
Compilation uses at most eight jobs and tests use four threads. The runner
sets `RUST_MIN_STACK=16777216`, matching `just test`, and preserves the caller's
temporary directory settings; fixtures keep their normal private directory
permissions.

Only these targets and selectors run (14 test cases):

- `codex-core --test all`: all four
  `suite::agent_execution::v2_residency_reload_preserves_inherited_environment_and_tools::`
  parameter cases.
- `codex-core --lib`: `cancelled_v2_send_finishes_reload_registration`, all three
  `multi_agent_v2_completion_marks_only_root_parent_wake::` parameter cases, and
  these four `thread_manager::tests::` cases:

  - `ignores_session_prefix_messages_when_truncating`
  - `spawn_internal_session_preserves_parent_lineage_without_forking_history`
  - `interrupted_fork_snapshot_does_not_synthesize_turn_id_for_legacy_history`
  - `interrupted_fork_snapshot_uses_persisted_mid_turn_history_without_live_source`
- `codex-tui --lib`:
  `app::owned_transcript::tests::fullscreen_composer_mouse_copy_and_input_ownership`
  and
  `app::owned_transcript::tests::footer_selection_defers_warning_clicks_and_preserves_the_draft`.

Before running each target, the runner checks nextest's JSON listing against the
exact required test names and binary: four residency, eight core unit, and two
TUI cases. Missing, ignored, filtered-out, renamed, or extra selected cases fail
the gate, including changes within either parameterized family. Tests run without
retries or snapshot updates. The gate reuses the current Linux job's cache and V8
setup; it does not build the full workspace test suite. macOS and Windows keep
their existing checks and builds.

## Pull Requests

- Required checks run against GitHub's synthetic merge commit, not the pull
  request head alone. This includes changes already on `main` and catches
  conflicts before they reach the branch.
- `bazel.yml` is the main pre-merge verification path for Rust code.
  It runs Bazel `test` and Bazel `clippy` on the supported Bazel targets,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

## Post-Merge On `main`

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path while still validating them after merge:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - Windows ARM64 nextest archives cross-compiled on Windows x64, then replayed on native Windows ARM64 shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel does not replace yet.
