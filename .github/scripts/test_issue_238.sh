#!/usr/bin/env bash
# Issue #238 regressions plus one Code Mode readiness smoke from issue #279.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root/codex-rs"

# CI reuses fork-ci's dev/debug artifacts. Local runs may reuse dev-small.
cargo_profile="${CODEX_ISSUE_238_CARGO_PROFILE:-dev}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-8}"
if [[ ! "$CARGO_BUILD_JOBS" =~ ^[1-9][0-9]*$ ]]; then
  echo 'CARGO_BUILD_JOBS must be a positive integer' >&2
  exit 2
fi
if (( CARGO_BUILD_JOBS > 8 )); then
  export CARGO_BUILD_JOBS=8
fi
# Match `just test` and require snapshot comparisons without updating files.
export RUST_MIN_STACK=16777216
export INSTA_UPDATE=no

# Preserve the caller's target directory and private temporary-directory defaults.
target_args=()
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
  target_args=(--target "$CARGO_BUILD_TARGET")
fi

# Tests resolve these from the same target/profile as their own executable.
# Build them from this checkout so stale binaries cannot invalidate the result.
cargo build --locked --profile "$cargo_profile" "${target_args[@]}" \
  -p codex-code-mode-host --bin codex-code-mode-host \
  -p codex-rmcp-client --bin test_stdio_server

nextest_args=(
  --locked --cargo-profile "$cargo_profile" "${target_args[@]}"
  --profile default
)

# Check the actual runnable names, not the listing's total (which includes skips).
# Use identical targets and filters for listing and running each selection.
run_selection() {
  local selection="$1"
  shift
  cargo nextest list "${nextest_args[@]}" "$@" --message-format json |
    python3 "$repo_root/.github/scripts/check_issue_238_selection.py" "$selection"
  cargo nextest run "${nextest_args[@]}" "$@" \
    --no-tests=fail --retries 0 --no-fail-fast --test-threads 4
}

# The prefix includes all eight parameterized residency cases.
run_selection residency -p codex-core --test all \
  -E 'test(/^suite::agent_execution::v2_residency_reload_preserves_inherited_environment_and_tools::/)'

# The completion prefix includes all three root-parent wake parameter cases.
# Select the four confirmed thread-manager regressions, not the whole module.
core_unit_filter='test(=agent::control::tests::cancelled_v2_send_finishes_reload_registration) |
  test(/^agent::control::tests::multi_agent_v2_completion_marks_only_root_parent_wake::/) |
  test(=thread_manager::tests::ignores_session_prefix_messages_when_truncating) |
  test(=thread_manager::tests::spawn_internal_session_preserves_parent_lineage_without_forking_history) |
  test(=thread_manager::tests::interrupted_fork_snapshot_does_not_synthesize_turn_id_for_legacy_history) |
  test(=thread_manager::tests::interrupted_fork_snapshot_uses_persisted_mid_turn_history_without_live_source)'
run_selection core-unit -p codex-core --lib -E "$core_unit_filter"

run_selection tui -p codex-tui --lib \
  -E 'test(=app::owned_transcript::tests::fullscreen_composer_mouse_copy_and_input_ownership) | test(=app::owned_transcript::tests::footer_selection_defers_warning_clicks_and_preserves_the_draft)'

# Keep this real-host readiness check separate from the issue #238 manifest.
run_selection code-mode-readiness -p codex-core --test all \
  -E 'test(=suite::async_cell_completion_tests::cell_polling_another_cells_process_delivers_its_completion)'
