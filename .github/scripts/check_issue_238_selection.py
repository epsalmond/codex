#!/usr/bin/env python3
"""Check the repaired cases in `cargo nextest list --message-format json`."""

from collections import Counter
import json
import sys


RESIDENCY = (
    "suite::agent_execution::"
    "v2_residency_reload_preserves_inherited_environment_and_tools::"
)
CONTROL = "agent::control::tests::"
WAKE = CONTROL + "multi_agent_v2_completion_marks_only_root_parent_wake::"
THREAD = "thread_manager::tests::"
TUI = "app::owned_transcript::tests::"

# Exact names from the reproduced baseline: 4 residency + 8 core + 2 TUI.
SELECTIONS = {
    "residency": (
        "codex-core::all",
        [
            RESIDENCY + "owner_revokes_cached_workspace_root",
            RESIDENCY + "owner_preserves_stricter_child",
            RESIDENCY + "owner_narrows_cached_permissions",
            RESIDENCY + "sender_preserves_stricter_child",
        ],
    ),
    "core-unit": (
        "codex-core",
        [
            CONTROL + "cancelled_v2_send_finishes_reload_registration",
            WAKE + "root_parent_with_wake_mode_child",
            WAKE + "non_root_parent",
            WAKE + "root_parent_with_polling_child",
            THREAD + "ignores_session_prefix_messages_when_truncating",
            THREAD
            + "spawn_internal_session_preserves_parent_lineage_without_forking_history",
            THREAD
            + "interrupted_fork_snapshot_does_not_synthesize_turn_id_for_legacy_history",
            THREAD
            + "interrupted_fork_snapshot_uses_persisted_mid_turn_history_without_live_source",
        ],
    ),
    "tui": (
        "codex-tui",
        [
            TUI + "fullscreen_composer_mouse_copy_and_input_ownership",
            TUI + "footer_selection_defers_warning_clicks_and_preserves_the_draft",
        ],
    ),
}


def main():
    selection = sys.argv[1]
    binary_id, names = SELECTIONS[selection]
    expected = Counter((binary_id, name) for name in names)
    listing = json.load(sys.stdin)
    # nextest 0.9.103's stable JSON API includes filtered/ignored cases too.
    selected = Counter(
        (suite_id, name)
        for suite_id, suite in listing["rust-suites"].items()
        for name, case in suite["testcases"].items()
        if case["filter-match"]["status"] == "matches" and not case["ignored"]
    )
    if selected != expected:
        print(f"{selection}: test selection drift", file=sys.stderr)
        for label, difference in (
            ("missing", expected - selected),
            ("unexpected", selected - expected),
        ):
            for suite_id, name in sorted(difference.elements()):
                print(f"  {label}: {suite_id} {name}", file=sys.stderr)
        raise SystemExit(1)
    print(f"{selection}: verified all {len(names)} required test cases")


if __name__ == "__main__":
    main()
