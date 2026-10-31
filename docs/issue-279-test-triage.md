# Issue 279 test triage

The original ledger contains 70 named scenarios and one retained hooks guard. At base `b70d4b571fff35644b609de45cf9e909b10ace1f`, the 70 scenarios ran: 3 passed and 67 failed; the hooks guard was not part of that baseline run. This change repairs fixture assumptions and two production paths without removing cases, increasing timeouts, enabling snapshot updates, or weakening authorization and delivery assertions.

## Dispositions and changed paths

Paths below are relative to `codex-rs/core/` unless stated otherwise. The disposition keys in the ledger identify the actual cause and repair.

- **header:** `tests/suite/code_mode.rs` expected an obsolete output header. Its anchored helper now checks required exec `work_kind`, exact status/cell id, configured timing format, and automatic-delivery notice. Original result, image, failure and termination checks remain.
- **catalog:** the same test file omitted the default top-level `close_agents` entry. Add that exact entry and retain deferred discovery, dispatch and result checks.
- **polling:** `tests/suite/multi_agent_tool_descriptions.rs` and six `pending_input.rs` interruption variants requested polling-only tools under wake defaults. Pin agent polling locally; keep the dedicated wake omission control and original interruption/delivery checks.
- **lifetime:** `tests/suite/guardian_subagent_authorization.rs`, `agent_wake.rs`, and `subagent_context_reduction.rs` followed or inspected a default task after its terminal report had closed it. Give only those fixtures a persistent role. H01 retains its original CLI session source. Unchanged task-close and native-reduction controls cover the boundary.
- **snapshot:** eight scenario and three pending-input snapshots drifted with current prompt, catalog and request metadata. Review each actual failure diff individually; update only those snapshots. C03 additionally used a path-dependent estimated token count: `tests/suite/scenarios.rs` now supplies provider usage 20000 and asserts that value before comparison. Its snapshot records 20000 and the resulting payload hash. No global normalization changed; stale tracked/untracked `.snap.new` files were not used as evidence of a current result.
- **idle (production):** spawned futures in `src/unified_exec/process_manager.rs` and `oneshot.rs` lost the `CODE_MODE_NESTED_CALL` task-local context before reaching approval. Capture and scope it across both spawn boundaries; expose the existing crate-private helper in `src/tools/mod.rs`. Idle callbacks now refuse user/Guardian escalation without an extra request. All eleven hosted callback variants and three interruption/retained-authority controls are selected.
- **wire:** `tests/suite/code_mode.rs` calibrated truncation against a different request than the wire sample. Measure first, yielded and terminal serialized request sizes; keep later complete metadata and output/history checks. Metadata shedding intentionally clears the continuation baseline, so assert the actual full-resend contract.
- **search:** the MCP exposure fixture assumed tool search existed without a deferred source. Retain all 64 original combinations plus two explicit deferred-positive controls. Original G03 maps to eight omission cases below. A newly added control initially waited twice for the single global MCP startup summary; consume it once and check both ready servers plus empty failures/cancellations. Splitting the cases does not raise the timeout.
- **shutdown (production):** G04 captured two old-session model requests during shutdown before the genuine resumed Start B request; `call-b` returned `code mode session is shutting down`. Finite owned-work hints can request a synthetic turn even with automatic completion disabled. `src/session/handlers.rs`, `session/session.rs` and `tasks/mod.rs` now close admission under the shared active-turn then state lock order, check reservation and final installation, and clean up only the caller's reservation. Unstarted mail is requeued, installed work remains cancellable, and hints are retained. Four deterministic regressions plus finite-hint, unread-mail, reservation-loss and ordinary wake controls cover both admission orderings. G04 keeps its exact 2/4/6 request totals and stale-certification checks.
- **apps:** `tests/suite/mcp_tool_exposure.rs` retains explicit polling visibility and accepts either the inline recovered Apps result or its collected-result pointer only when the same automatic completion carries the exact successful payload. Exactly-one app invocation remains required.
- **explicit:** `tests/suite/agent_message_wake.rs` expected a report wake after Esc despite the root being paused. Submit an explicit user turn to resume it even when WakeOnAgentMessages is disabled, then verify the exact turn and preserved reports. Plan/enabled siblings remain selected.
- **evidence:** `tests/suite/guardian_review.rs` searched only the newest delta after compaction, although pending evidence was already admitted and retained. Assert the actual diagnostic tool-result text exactly once across retained reviewer input; keep checkpoint/new-authorization checks and run all three recovery variants.
- **boundary:** three `tests/suite/pending_input.rs` final-answer fixtures injected mail before the intended initial-sample boundary. Gate acceptance after that sample; retain visible/empty/whitespace final-answer and exactly-once next-turn delivery checks.
- **websocket:** those fixtures used a reasoning id without the protocol prefix. Client history sanitization cleared it and forced a full resend. Use a consistent `ResponseItemId::with_suffix` id; retain all interrupt acknowledgement, drain, continuation and pending-input assertions. The discarded-reasoning control remains selected.
- **pause:** terminal root failure intentionally pauses report wakeups. Triggering mail stays queued rather than starting a second failing turn. The two fixtures now pin wake mode and verify one error/completion, paused state plus queued result, no retry, held history, explicit recovery, and exactly-once model/history delivery. All six compaction variants are selected.
- **guard:** I09/I18/I19 already passed at baseline. Retain their original running-cell, input-preemption and completion behavior checks.
- **hook:** J01 is the retained observer guard, not an additional historical failure. It checks exec/wait observation under current completion behavior.

## Original ledger

These are exact original names. G03 is deliberately replaced by all eight names in the next table; no other original name is replaced, ignored or omitted.

| ID | Original exact name | Disposition |
| --- | --- | --- |
| A01 | `suite::code_mode::code_mode_background_keeps_running_on_later_turn_without_wait` | header |
| A02 | `suite::code_mode::code_mode_can_apply_patch_via_nested_tool` | header |
| A03 | `suite::code_mode::code_mode_can_return_exec_command_output` | header |
| A04 | `suite::code_mode::code_mode_can_use_mcp_image_result_with_image_helper` | header |
| A05 | `suite::code_mode::code_mode_can_use_view_image_result_with_image_helper::legacy_detail` | header |
| A06 | `suite::code_mode::code_mode_can_use_view_image_result_with_image_helper::unified_image_budget` | header |
| A07 | `suite::code_mode::code_mode_can_yield_and_resume_with_wait` | header |
| A08 | `suite::code_mode::code_mode_exit_stops_script_immediately` | header |
| A09 | `suite::code_mode::code_mode_returns_accumulated_output_when_script_fails` | header |
| A10 | `suite::code_mode::code_mode_terminate_tool_can_terminate_and_continue` | header |
| A11 | `suite::code_mode::code_mode_wait_can_terminate_and_continue` | header |
| A12 | `suite::code_mode::code_mode_yield_and_termination_are_not_starved_by_runtime_output` | header |
| B01 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_spoofed_project_mcp_send` | lifetime |
| B02 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_spoofed_raw_apps_tool_name` | lifetime |
| B03 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_user_message_connector` | lifetime |
| B04 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_user_message_connector_blocked_post_hook` | lifetime |
| B05 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_user_message_connector_cancelled_post_hook` | lifetime |
| B06 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_user_message_connector_mcp_error` | lifetime |
| B07 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_user_message_flat` | lifetime |
| B08 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_user_message_flat_batch` | lifetime |
| B09 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_user_message_flat_cancelled_before_confirmation` | lifetime |
| B10 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::code_mode_user_reply_before_delivery` | lifetime |
| C01 | `suite::scenarios::astra_continues_after_input_yields_a_code_mode_cell` | snapshot |
| C02 | `suite::scenarios::astra_reads_code_mode_call_timing` | snapshot |
| C03 | `suite::scenarios::astra_settings_release_check_with_direct_and_code_mode_tools` | snapshot |
| C04 | `suite::scenarios::code_mode_mcp_schema_limits_preserve_explicit_overrides` | snapshot |
| C05 | `suite::scenarios::code_mode_settled_helpers::code_mode_settled_helpers_report_successes_and_failures` | snapshot |
| C06 | `suite::scenarios::guardian_code_mode_messaging_request_history` | snapshot |
| C07 | `suite::scenarios::indirect_namespace_prefixes::catalog_namespace_prefixes_follow_the_selected_model::code_mode_only` | snapshot |
| C08 | `suite::scenarios::mcp_resource_messages::mcp_resource_messages::code_mode` | snapshot |
| D01 | `suite::code_mode::code_mode_only_searches_and_calls_deferred_app_tools::control` | catalog |
| D02 | `suite::code_mode::code_mode_only_searches_and_calls_deferred_app_tools::ranked_search` | catalog |
| D03 | `suite::code_mode::code_mode_only_searches_and_calls_deferred_app_tools::ranked_search_expanded_schema` | catalog |
| D04 | `suite::code_mode::code_mode_only_searches_and_calls_deferred_app_tools::ranked_search_tool_description_first` | catalog |
| D05 | `suite::code_mode::code_mode_only_searches_and_calls_deferred_app_tools::tool_description_first` | catalog |
| E01 | `suite::multi_agent_tool_descriptions::multi_agent_catalog_messages_change_only_selected_tool_fields::code_mode_declarations` | polling |
| E02 | `suite::multi_agent_tool_descriptions::multi_agent_catalog_messages_change_only_selected_tool_fields::code_mode_parameters` | polling |
| E03 | `suite::multi_agent_tool_descriptions::multi_agent_catalog_messages_change_only_selected_tool_fields::empty_code_mode_descriptions` | polling |
| F01 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::guardian_approval_refused_while_idle` | idle |
| F02 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::user_approval_refusal_delivered_automatically` | idle |
| F03 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::user_approval_refused_while_idle` | idle |
| G01 | `suite::code_mode::code_mode_argument_truncation_preserves_later_wait_completeness` | wire |
| G02 | `suite::code_mode::code_mode_only_restricts_prompt_tools` | catalog |
| G03 | `suite::code_mode::mcp_code_mode_exclusion_does_not_change_direct_mode_tool_exposure` | search |
| G04 | `suite::code_mode::code_mode_resumed_wait_does_not_certify_a_reused_runtime_cell` | shutdown |
| G05 | `suite::mcp_tool_exposure::code_mode_discovered_apps_tool_survives_startup_failure_and_recovers` | apps |
| G06 | `suite::subagent_context_reduction::tool_output_crossing_cap_compacts_before_the_next_sample::code_mode` | lifetime |
| H01 | `suite::agent_wake::wake_mode_help_round_trip_wakes_root_twice` | lifetime |
| I01 | `suite::agent_message_wake::pending_agent_message_wake_respects_control_changes::disabled_interrupt_preserves_status_and_explicit_followup` | explicit |
| I02 | `suite::guardian_review::guardian_review_recovers_after_compaction::failed_compaction_preserves_pending_evidence` | evidence |
| I03 | `suite::pending_input::any_new_input_interrupts_sleep::already_pending_injection` | polling |
| I04 | `suite::pending_input::any_new_input_interrupts_sleep::injection_while_waiting` | polling |
| I05 | `suite::pending_input::any_new_input_interrupts_sleep::user_steer` | polling |
| I06 | `suite::pending_input::input_interrupts_wait_agent_and_is_sent_in_follow_up_request::already_pending_injection` | polling |
| I07 | `suite::pending_input::input_interrupts_wait_agent_and_is_sent_in_follow_up_request::injection_while_waiting` | polling |
| I08 | `suite::pending_input::input_interrupts_wait_agent_and_is_sent_in_follow_up_request::user_steer` | polling |
| I09 | `suite::pending_input::input_preempts_response_and_yields_running_code_mode_call` | guard |
| I10 | `suite::pending_input::queued_inter_agent_mail_does_not_restart_after_final_answer::empty_final` | boundary |
| I11 | `suite::pending_input::queued_inter_agent_mail_does_not_restart_after_final_answer::visible_final` | boundary |
| I12 | `suite::pending_input::queued_inter_agent_mail_does_not_restart_after_final_answer::whitespace_final` | boundary |
| I13 | `suite::pending_input::queued_inter_agent_mail_triggers_follow_up_after_commentary_message_item` | snapshot |
| I14 | `suite::pending_input::queued_inter_agent_mail_triggers_follow_up_after_reasoning_item` | snapshot |
| I15 | `suite::pending_input::steer_interrupts_and_drains_websocket::completed_reasoning` | websocket |
| I16 | `suite::pending_input::steer_interrupts_and_drains_websocket::missing_acknowledgement` | websocket |
| I17 | `suite::pending_input::steer_interrupts_and_drains_websocket::rejected_interrupt` | websocket |
| I18 | `suite::pending_input::steers_yield_exec_and_wait_without_stopping_the_cell::disabled` | guard |
| I19 | `suite::pending_input::steers_yield_exec_and_wait_without_stopping_the_cell::enabled` | guard |
| I20 | `suite::pending_input::terminal_compaction_error_does_not_retry_pending_input::mid_turn_triggering_mail` | pause |
| I21 | `suite::pending_input::terminal_compaction_error_does_not_retry_pending_input::pre_turn_triggering_mail` | pause |
| I22 | `suite::pending_input::user_input_does_not_preempt_after_reasoning_item` | snapshot |
| J01 | `suite::hooks::pre_tool_use_observes_code_mode_exec_and_wait_without_gating_them` | hook |

## Runnable G03 replacements and readiness smoke

| ID | Exact runnable name | Binary |
| --- | --- | --- |
| G03.01 | `suite::code_mode::mcp_code_mode_exclusion_does_not_change_direct_mode_tool_exposure::omit_code_mode` | `codex-core::all` |
| G03.02 | `suite::code_mode::mcp_code_mode_exclusion_does_not_change_direct_mode_tool_exposure::omit_deferred` | `codex-core::all` |
| G03.03 | `suite::code_mode::mcp_code_mode_exclusion_does_not_change_direct_mode_tool_exposure::omit_direct` | `codex-core::all` |
| G03.04 | `suite::code_mode::mcp_code_mode_exclusion_does_not_change_direct_mode_tool_exposure::omit_code_mode_and_deferred` | `codex-core::all` |
| G03.05 | `suite::code_mode::mcp_code_mode_exclusion_does_not_change_direct_mode_tool_exposure::omit_code_mode_and_direct` | `codex-core::all` |
| G03.06 | `suite::code_mode::mcp_code_mode_exclusion_does_not_change_direct_mode_tool_exposure::omit_deferred_and_direct` | `codex-core::all` |
| G03.07 | `suite::code_mode::mcp_code_mode_exclusion_does_not_change_direct_mode_tool_exposure::omit_all_exposures` | `codex-core::all` |
| G03.08 | `suite::code_mode::mcp_code_mode_exclusion_does_not_change_direct_mode_tool_exposure::omit_no_exposures` | `codex-core::all` |
| SMOKE | `suite::async_cell_completion_tests::cell_polling_another_cells_process_delivers_its_completion` | `codex-core::all` |

## Focused safety controls

These controls exercise changed shared fixtures and the two production fixes; original-ledger cases are deduplicated.

| ID | Exact runnable name | Binary |
| --- | --- | --- |
| SAFETY-01 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::native_callback_after_idle` | `codex-core::all` |
| SAFETY-02 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::notify_after_idle` | `codex-core::all` |
| SAFETY-03 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::shutdown_stops_idle_callback` | `codex-core::all` |
| SAFETY-04 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::native_callback_after_idle_delivered_automatically` | `codex-core::all` |
| SAFETY-05 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::one_shot_native_callback_after_idle` | `codex-core::all` |
| SAFETY-06 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::one_shot_user_approval_refused_while_idle` | `codex-core::all` |
| SAFETY-07 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::one_shot_guardian_approval_refused_while_idle` | `codex-core::all` |
| SAFETY-08 | `suite::code_mode::idle_callbacks::hosted_callback_settles_after_original_owner_becomes_idle::one_shot_user_approval_refusal_delivered_automatically` | `codex-core::all` |
| SAFETY-09 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::retained_user_message_connector` | `codex-core::all` |
| SAFETY-10 | `suite::guardian_subagent_authorization::guardian_subagent_review_preserves_late_root_user_authorization::migrating_complete_answer` | `codex-core::all` |
| SAFETY-11 | `suite::unified_exec::managed_one_shot_command_is_terminated_when_the_turn_is_interrupted` | `codex-core::all` |
| SAFETY-12 | `suite::unified_exec::managed_unified_exec_disable_runs_commands_without_retained_authority` | `codex-core::all` |
| SAFETY-13 | `suite::unified_exec::unified_exec_emits_terminal_interaction_for_write_stdin::with_stdin_approval` | `codex-core::all` |
| SAFETY-14 | `tasks::tests::shutdown_rejects_reserved_task_before_run_and_preserves_mail` | `codex-core` |
| SAFETY-15 | `tasks::tests::shutdown_rejects_inflight_wake_and_requeues_mail` | `codex-core` |
| SAFETY-16 | `tasks::tests::shutdown_cancels_task_installed_before_admission_closes` | `codex-core` |
| SAFETY-17 | `tasks::owned_completion::tests::shutdown_rejects_finite_hint_without_payload_and_preserves_pending_work` | `codex-core` |
| SAFETY-18 | `tasks::owned_completion::tests::finite_hint_without_payload_passes_selector_and_bind_and_survives_paused_unstarted_turn` | `codex-core` |
| SAFETY-19 | `tasks::tests::finished_task_returns_unread_mail_only_in_wake_mode_when_not_read` | `codex-core` |
| SAFETY-20 | `suite::async_cell_completion_tests::idle_root_wakes_once_on_cell_completion` | `codex-core::all` |
| SAFETY-21 | `suite::wake_reservation::interrupt_during_wake_context_construction_does_not_start_turn` | `codex-core::all` |
| SAFETY-22 | `suite::agent_wake::wake_mode_root_sleeps_until_child_reports` | `codex-core::all` |
| SAFETY-23 | `suite::subagent_notifications::typed_reports_tests::task_agent_final_report_reaches_root_once_then_agent_closes::explicit_final_report` | `codex-core::all` |
| SAFETY-24 | `suite::subagent_notifications::typed_reports_tests::task_agent_final_report_reaches_root_once_then_agent_closes::turn_ends_without_report` | `codex-core::all` |
| SAFETY-25 | `suite::multi_agent_tool_descriptions::multi_agent_catalog_messages_change_only_selected_tool_fields::catalog_description` | `codex-core::all` |
| SAFETY-26 | `suite::subagent_context_reduction::tool_output_crossing_cap_compacts_before_the_next_sample::native_tools` | `codex-core::all` |
| SAFETY-27 | `suite::agent_message_wake::pending_agent_message_wake_respects_control_changes::disabled_plan_allows_explicit_followup` | `codex-core::all` |
| SAFETY-28 | `suite::agent_message_wake::pending_agent_message_wake_respects_control_changes::interrupt_during_discovery` | `codex-core::all` |
| SAFETY-29 | `suite::agent_message_wake::pending_agent_message_wake_respects_control_changes::plan_mode_during_discovery` | `codex-core::all` |
| SAFETY-30 | `suite::guardian_review::guardian_review_recovers_after_compaction::reuse_parent_checkpoint_and_newer_authorization` | `codex-core::all` |
| SAFETY-31 | `suite::guardian_review::guardian_review_recovers_after_compaction::without_parent_checkpoint` | `codex-core::all` |
| SAFETY-32 | `suite::pending_input::steer_interrupts_and_drains_websocket::discarded_reasoning` | `codex-core::all` |
| SAFETY-33 | `suite::pending_input::terminal_compaction_error_does_not_retry_pending_input::mid_turn_mail` | `codex-core::all` |
| SAFETY-34 | `suite::pending_input::terminal_compaction_error_does_not_retry_pending_input::mid_turn_steer` | `codex-core::all` |
| SAFETY-35 | `suite::pending_input::terminal_compaction_error_does_not_retry_pending_input::pre_turn_mail` | `codex-core::all` |
| SAFETY-36 | `suite::pending_input::terminal_compaction_error_does_not_retry_pending_input::pre_turn_steer` | `codex-core::all` |
| SAFETY-37 | `tools::spec_plan::tests::exec_root_wake_mode_drops_wait_agent_unless_opted_out` | `codex-core` |

## Validation and reproduction

The complete source before this documentation commit is `38c4a6ab4f6d0253151a730c402670f921b9339a`. Its focused pass9 ran 20 cases (6 core unit, 14 integration): all passed, with fresh helper binaries, exact nonignored discovery, matching JUnit/human outcomes, unchanged source and preserved snapshot artifacts. This establishes the shutdown patch, strict G04, all eight G03 replacements and G01/H01 review repairs. It is not a final-head result for the whole ledger.

The final selection is 78 runnable ledger cases plus one real-host readiness smoke (79), and 37 distinct safety controls: **116 total**, 109 in `codex-core::all` and 7 in `codex-core`. The committed tables are the durable selection. On the NAS the final supervisor writes `/tmp/codex-279-final-result.md`, `/tmp/codex-279-final.json`, per-stage/per-case files under `/tmp/codex-279-final-artifacts/`, and `/tmp/codex-279-final.done` only at terminal completion. Final success is pending when this document is committed; the delivery report must cite the final source and actual receipts.

Initialize the checkout's worktree environment and private TMPDIR. From `codex-rs`, use dev/debug with the native target, `CARGO_BUILD_JOBS=8`, `RUST_MIN_STACK=16777216`, `RUST_BACKTRACE=1` and `INSTA_UPDATE=no`. Build `codex-code-mode-host` and `test_stdio_server` from this same checkout/profile before discovering and running cases. The following derives the exact filter from this document, excluding only the replaced original G03 name:

```bash
cargo build -p codex-code-mode-host --bin codex-code-mode-host \
  -p codex-rmcp-client --bin test_stdio_server
issue_279_filter="$(python3 - <<'PYFILTER'
from pathlib import Path
import re
text = Path('../docs/issue-279-test-triage.md').read_text()
rows = re.findall(r'^\| ([A-Z0-9.-]+) \| `(suite::[^`]+|tasks::[^`]+|tools::[^`]+)`', text, re.M)
names = [name for key, name in rows if key != 'G03']
assert len(names) == len(set(names)) == 116
print(' | '.join('test(=' + name + ')' for name in names))
PYFILTER
)"
cargo nextest list -p codex-core --lib --test all \
  --message-format json -E "$issue_279_filter"
cargo nextest run -p codex-core --lib --test all \
  --no-tests=fail --retries 0 --no-fail-fast --test-threads 4 \
  -E "$issue_279_filter"
```

Before accepting a result, compare the fresh listing's matching `(binary, name)` pairs against all 116 table entries and require none ignored; a nonzero listing count alone is insufficient. Preserve source, helper hashes, profile, environment, per-case outcomes and the real JUnit report. Run repository formatting checks, focused core clippy (`--lib --test all`, warnings denied), diff checks and `.github/scripts/test_check_issue_238_selection.py` separately from runtime acceptance.

CI adds only `suite::async_cell_completion_tests::cell_polling_another_cells_process_delivers_its_completion` to the existing focused runner as a separate exactly-one selection. The original 18-case issue238 manifest stays intact. `.github/scripts/test_issue_238.sh` and `.github/workflows/README.md` describe its fresh stdio-host setup. Local focused evidence does not establish remote CI, other platforms, an installed binary or deployment status.
