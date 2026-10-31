# Workflow Strategy

The workflows in this directory are split so that pull requests get fast, review-friendly signal while `main` still gets the full cross-platform verification pass.

## Fork PR gate

In `epsalmond/codex`, `fork-ci.yml` replaces the upstream checks that require
unavailable runner groups. It runs formatting, clippy, and dev builds, plus a
focused Linux nextest gate for the confirmed issue #238 regressions, including
the footer-selection snapshot also reported in issue #232, and one real-host
Code Mode readiness smoke from issue #279. Broader baseline test triage and
coverage remain tracked in issues #155 and #279.

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

The issue #238 selections remain separate (18 test cases):

- `codex-core --test all`: all eight
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
exact required test names and binary: eight residency, eight core unit, and two
TUI cases. A separate exact selection adds
`suite::async_cell_completion_tests::cell_polling_another_cells_process_delivers_its_completion`
from `codex-core --test all`. This real stdio-host smoke checks completion of a
process polled by another cell, exactly-once result delivery, and owner work
closure. It must select exactly one runnable case. Missing, ignored, filtered-out,
renamed, or extra selected cases fail the gate, including changes within either
parameterized family. Tests run without
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

## Exact upstream recovery

`fork-upstream-update.yml` prepares an exact upstream merge and validates its
immutable branch/SHA before landing it on `main` with compare-and-swap. The
candidate must have exactly the original fork parent followed by the peeled
upstream tag commit. Landing rechecks the remote branch tip and current `main`;
retrying an already landed exact candidate can resume release dispatch. A new
repair uses the same original two parents, never an extra repair parent.

Merge conflicts retain the existing schema v2 `merge_conflict` deployment.
Failed validation now emits schema v3 `validation_failure` through the same
`upstream-release` deployment environment and task. The deployment ref and SHA
are the original full fork SHA; its creator must be `github-actions[bot]`
(ID 41898282). The fixed repository is `epsalmond/codex` (ID 1365250630).
The feedback job has contents/actions read and deployments write permissions,
checks out the trusted workflow base (`github.sha`) without persisted credentials,
and never executes the failed candidate with its write token.

The v3 payload fields are:

```json
{
  "schema_version": 3,
  "recovery_kind": "validation_failure",
  "upstream_tag": "rust-v0.164.0",
  "upstream_sha": "<40 lowercase hex>",
  "fork_sha": "<40 lowercase hex>",
  "candidate_sha": "<exact failed prepare-output SHA>",
  "candidate_branch": "fork-recovery/<tag>/<fork_sha12>-attempt-1",
  "run_id": 40297840820,
  "run_attempt": 1,
  "run_url": "https://github.com/epsalmond/codex/actions/runs/40297840820",
  "validation_job_id": 121488520368,
  "failed_step": "Test scoped release behavior",
  "validation_result": "source_failure",
  "failure_reason": "rustc_diagnostics",
  "failure_context": "<sanitized bounded diagnostic windows>",
  "context_truncated": false
}
```

The collector verifies the fixed repository, updater workflow path/event, base
SHA, current run attempt, and exact attempt's prepare and reusable validation
jobs. It does not compare the run head to the candidate: the run executes the
base workflow. GitHub does not expose dispatch inputs in run metadata. It
requires terminal validation failure, not terminal completion of the containing
updater run, which is still delivering the feedback. It also rechecks current
`main`, exact candidate branch/SHA, original parents, and upstream tag identity.
The processor independently verifies API provenance and logs; payload context is
untrusted diagnostic data, never permission to change acceptance or budgets.

Only `Check formatting` with an actual formatter diff, or `Test scoped release
behavior` with rustc error codes or named test failures, can classify as
`source_failure`. Setup/identity failures, cancellation/timeouts, toolchain,
network/disk/resource diagnostics, and unknown output classify as
`infrastructure_failure`. Infrastructure diagnostics anywhere in the bounded
log take precedence over incidental source text. Failed helper tests remain
unclassified infrastructure outcomes. A source classification additionally
requires successful prepare and candidate identity steps. A killed workflow may
prevent any feedback job; Actions failure and the existing stalled-handoff
check remain the available evidence in that case.

Raw job logs use a 30-second request timeout and a 60-second streaming deadline,
with a 16 MiB download ceiling, at most 128 KiB diagnostic/infrastructure/tail storage, 2 KiB lines,
and at most 160 lines / 12 KiB final UTF-8 context. The collector scans the full
bounded stream and deduplicates diagnostic headers with their primary locations.
Compact distinct error/location pairs take priority over detail; source snippets
and expected/found/help lines are then added across diagnostics before a short
terminal tail. Per-block and aggregate limits prevent early verbose errors or
cleanup output from crowding out later locations. Excess unique diagnostics or
detail set `context_truncated`; bounded context cannot guarantee completeness.
The collector strips ANSI/OSC/control/workflow commands,
and redacts token patterns, authorization values, URL userinfo/query/fragment,
and the active workflow token. Signed log redirects receive no API authorization
header. Hitting the download ceiling prevents source classification. Full public
job logs remain the authoritative detailed evidence; collection/API/provenance
failure turns the handoff job red and still permits the failed-candidate draft PR.

Initial candidates use `fork-upstream-candidate/<tag>/<fork_sha12>` or the existing
`fork-recovery/<tag>/<fork_sha12>` root. Source repairs use only immutable
`fork-recovery/<tag>/<fork_sha12>-attempt-1` and `-attempt-2`. Sibling refs avoid
Git's file/directory collision with preserved historical root refs. The processor owns
the durable budget: at most two repair model invocations for the original tuple,
shared across reruns, redeliveries, restarts, and operator retry. Preflight before
a model call consumes no invocation. A prerequisite discovered during a model
call consumes that invocation; any later model call uses the next remaining
slot. Publication/dispatch retry never reruns the model. This supersedes the older
same-slot toolchain-continuation proposal. Toolchain provisioning uses an isolated
management-plane manifest draft PR, normal review/merge/GitOps promotion, and
installed sandbox verification; no candidate-controlled host setup is authorized.

The producer deduplicates candidate/run/attempt deployments. The processor also
deduplicates consumed failed candidates across different runs and owns persistent
attempt reservations and terminal exhaustion. Infrastructure failures spend no
source repair slot. A rerun supersedes old run-attempt notifications; advancing
`main` or moving the candidate makes the old tuple stale. The failed-candidate PR
records the exact prepare SHA, any changed branch tip, classification, and
deployment status link. It cannot predict future exhaustion. Terminal processor
status carries the final budget, last diagnostic, and supported retry/escalation.
A successful deployment status establishes local handoff completion only;
validation success, accepted landing, release dispatch, and published assets are
separate acceptance gates.

Deploy compatible management-plane processor/runtime support first, then this
producer and branch grammar, then exercise one new exact-base automatic attempt.
Preserve historical candidates, PRs, worktrees, and budgets. Roll back the v3
producer first; stop or forward-fix the processor rather than resetting upgraded
state or budgets. Component installation and full GitOps convergence are separate
gates; unrelated host capacity admission is not a reason to clean storage.
