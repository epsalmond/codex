use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;

use super::common;
use crate::engine::ClaudeHooksEngine;
use crate::engine::ConfiguredHandler;
use crate::engine::HandlerRunResult;
use crate::engine::dispatcher;
use crate::engine::output_parser;
use crate::output_spill::AdditionalContext;
use crate::schema::PreToolUseCommandInput;
use crate::schema::SubagentCommandInputFields;

#[derive(Debug, Clone)]
pub struct PreToolUseRequest {
    pub session_id: ThreadId,
    pub turn_id: String,
    pub subagent: Option<common::SubagentHookContext>,
    pub cwd: AbsolutePathBuf,
    pub transcript_path: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub tool_name: String,
    pub matcher_aliases: Vec<String>,
    pub tool_use_id: String,
    pub tool_input: Value,
}

#[derive(Debug)]
pub struct PreToolUseOutcome {
    pub hook_events: Vec<HookCompletedEvent>,
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub additional_contexts: Vec<crate::HookContext>,
    pub updated_input: Option<Value>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PreToolUseHandlerData {
    should_block: bool,
    block_reason: Option<String>,
    additional_contexts_for_model: Vec<AdditionalContext>,
    updated_input: Option<Value>,
}

pub(crate) fn preview(
    handlers: &[ConfiguredHandler],
    request: &PreToolUseRequest,
) -> Vec<HookRunSummary> {
    let matcher_inputs = common::matcher_inputs(&request.tool_name, &request.matcher_aliases);
    dispatcher::select_handlers_for_matcher_inputs(
        handlers,
        HookEventName::PreToolUse,
        &matcher_inputs,
    )
    .into_iter()
    .map(|handler| {
        common::hook_run_for_tool_use(dispatcher::running_summary(&handler), &request.tool_use_id)
    })
    .collect()
}

pub(crate) async fn run(
    engine: &ClaudeHooksEngine,
    request: PreToolUseRequest,
) -> PreToolUseOutcome {
    run_with_mode(engine, request, DispatchMode::Control).await
}

/// Whether any configured `PreToolUse` handler matches this tool name.
pub(crate) fn has_handlers(
    handlers: &[ConfiguredHandler],
    tool_name: &str,
    matcher_aliases: &[String],
) -> bool {
    let matcher_inputs = common::matcher_inputs(tool_name, matcher_aliases);
    !dispatcher::select_handlers_for_matcher_inputs(
        handlers,
        HookEventName::PreToolUse,
        &matcher_inputs,
    )
    .is_empty()
}

/// Observed calls are on the tool's hot path, so their hooks get a short,
/// fixed budget. Handlers still running at the deadline are killed.
const OBSERVATIONAL_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Runs `PreToolUse` hooks for a call that hooks may observe but not gate.
///
/// Hook stdin carries `"observational": true`. Matching hooks run exactly as
/// they do for gated tools, and their `additionalContext` still reaches the
/// model, but block decisions, exit code 2, and `updatedInput` are reported as
/// ignored and never reach the caller.
pub(crate) async fn run_observation(
    engine: &ClaudeHooksEngine,
    request: PreToolUseRequest,
) -> PreToolUseOutcome {
    run_with_mode(engine, request, DispatchMode::Observational).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchMode {
    Control,
    Observational,
}

async fn run_with_mode(
    engine: &ClaudeHooksEngine,
    request: PreToolUseRequest,
    mode: DispatchMode,
) -> PreToolUseOutcome {
    let matcher_inputs = common::matcher_inputs(&request.tool_name, &request.matcher_aliases);
    let matched = dispatcher::select_handlers_for_matcher_inputs(
        &engine.handlers,
        HookEventName::PreToolUse,
        &matcher_inputs,
    );
    if matched.is_empty() {
        return PreToolUseOutcome {
            hook_events: Vec::new(),
            should_block: false,
            block_reason: None,
            additional_contexts: Vec::new(),
            updated_input: None,
        };
    }

    let input_json = match command_input_json_with_mode(&request, mode) {
        Ok(input_json) => input_json,
        Err(error) => {
            let hook_events = common::serialization_failure_hook_events_for_tool_use(
                matched,
                Some(request.turn_id.clone()),
                format!("failed to serialize pre tool use hook input: {error}"),
                &request.tool_use_id,
            );
            return serialization_failure_outcome(hook_events);
        }
    };

    let parse = match mode {
        DispatchMode::Control => parse_completed,
        DispatchMode::Observational => parse_completed_observational,
    };
    let observed_handlers = (mode == DispatchMode::Observational).then(|| matched.clone());
    let execution = dispatcher::execute_handlers(
        engine,
        matched,
        input_json,
        request.cwd.as_path(),
        Some(request.turn_id.clone()),
        parse,
    );
    let results = match observed_handlers {
        None => execution.await,
        Some(handlers) => match tokio::time::timeout(OBSERVATIONAL_HOOK_TIMEOUT, execution).await {
            Ok(results) => results,
            Err(_) => {
                return serialization_failure_outcome(
                    common::serialization_failure_hook_events_for_tool_use(
                        handlers,
                        Some(request.turn_id.clone()),
                        format!(
                            "observational PreToolUse hook timed out after {}ms",
                            OBSERVATIONAL_HOOK_TIMEOUT.as_millis()
                        ),
                        &request.tool_use_id,
                    ),
                );
            }
        },
    };

    let should_block = results.iter().any(|result| result.data.should_block);
    let block_reason = results
        .iter()
        .find_map(|result| result.data.block_reason.clone());
    let additional_contexts = common::flatten_additional_contexts(
        results
            .iter()
            .map(|result| result.data.additional_contexts_for_model.as_slice()),
    );
    let additional_contexts = engine
        .command_runtime
        .output_spiller()
        .maybe_spill_additional_contexts(additional_contexts)
        .await;
    let updated_input = if should_block {
        None
    } else {
        latest_updated_input(&results)
    };

    PreToolUseOutcome {
        hook_events: results
            .into_iter()
            .map(|result| {
                common::hook_completed_for_tool_use(result.completed, &request.tool_use_id)
            })
            .collect(),
        should_block,
        block_reason,
        additional_contexts,
        updated_input,
    }
}

/// Chooses the rewrite from the hook that actually finished last.
///
/// Hook results stay in configured order for stable reporting, but the
/// `PreToolUse` contract resolves competing rewrites by completion order.
fn latest_updated_input(
    results: &[dispatcher::ParsedHandler<PreToolUseHandlerData>],
) -> Option<Value> {
    results
        .iter()
        .filter_map(|result| {
            result
                .data
                .updated_input
                .clone()
                .map(|updated_input| (result.completion_order, updated_input))
        })
        .max_by_key(|(completion_order, _)| *completion_order)
        .map(|(_, updated_input)| updated_input)
}

/// Serializes command stdin for a selected `PreToolUse` hook.
///
/// Handler selection may include internal matcher aliases, but hook stdin keeps
/// the canonical `tool_name` so audit logs and downstream policy decisions stay
/// stable. Shell-like tools pass `{ "command": ... }` as `tool_input`; MCP
/// tools pass their resolved JSON arguments.
#[cfg(test)]
fn command_input_json(request: &PreToolUseRequest) -> Result<String, serde_json::Error> {
    command_input_json_with_mode(request, DispatchMode::Control)
}

fn command_input_json_with_mode(
    request: &PreToolUseRequest,
    mode: DispatchMode,
) -> Result<String, serde_json::Error> {
    let subagent = SubagentCommandInputFields::from(request.subagent.as_ref());
    serde_json::to_string(&PreToolUseCommandInput {
        session_id: request.session_id.to_string(),
        turn_id: request.turn_id.clone(),
        agent_id: subagent.agent_id,
        agent_type: subagent.agent_type,
        transcript_path: crate::schema::NullableString::from_path(request.transcript_path.clone()),
        cwd: request.cwd.display().to_string(),
        hook_event_name: "PreToolUse".to_string(),
        model: request.model.clone(),
        permission_mode: request.permission_mode.clone(),
        tool_name: request.tool_name.clone(),
        tool_input: request.tool_input.clone(),
        tool_use_id: request.tool_use_id.clone(),
        observational: (mode == DispatchMode::Observational).then_some(true),
    })
}

fn parse_completed(
    handler: &ConfiguredHandler,
    run_result: HandlerRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<PreToolUseHandlerData> {
    parse_completed_with_mode(handler, run_result, turn_id, DispatchMode::Control)
}

fn parse_completed_observational(
    handler: &ConfiguredHandler,
    run_result: HandlerRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<PreToolUseHandlerData> {
    parse_completed_with_mode(handler, run_result, turn_id, DispatchMode::Observational)
}

const OBSERVATIONAL_DECISION_IGNORED: &str =
    "PreToolUse decision ignored: this tool call is observed by hooks, not gated";

fn parse_completed_with_mode(
    handler: &ConfiguredHandler,
    run_result: HandlerRunResult,
    turn_id: Option<String>,
    mode: DispatchMode,
) -> dispatcher::ParsedHandler<PreToolUseHandlerData> {
    // Observed calls treat every hook like an async hook: context is kept, but
    // no decision can block or rewrite the call.
    let observational = mode == DispatchMode::Observational;
    let can_apply_control_effects = handler.can_apply_control_effects() && !observational;
    let mut entries = Vec::new();
    let mut status = HookRunStatus::Completed;
    let mut should_block = false;
    let mut block_reason = None;
    let mut additional_contexts_for_model = Vec::new();
    let mut updated_input = None;

    match run_result.error.as_deref() {
        Some(error) => {
            status = HookRunStatus::Failed;
            entries.push(HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: error.to_string(),
            });
        }
        None => match run_result.exit_code {
            Some(0) => {
                let trimmed_stdout = run_result.stdout.trim();
                if trimmed_stdout.is_empty() {
                } else if let Some(parsed) = output_parser::parse_pre_tool_use(&run_result.stdout) {
                    if let Some(system_message) = parsed.universal.system_message {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: system_message,
                        });
                    }
                    if (!can_apply_control_effects || parsed.invalid_reason.is_none())
                        && let Some(additional_context) = parsed.additional_context
                    {
                        common::append_additional_context(
                            &mut entries,
                            &mut additional_contexts_for_model,
                            handler,
                            additional_context,
                            &run_result.context_metadata,
                        );
                    }
                    if observational
                        && handler.can_apply_control_effects()
                        && (parsed.block_reason.is_some() || parsed.updated_input.is_some())
                    {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: OBSERVATIONAL_DECISION_IGNORED.to_string(),
                        });
                    }
                    if can_apply_control_effects {
                        if let Some(invalid_reason) = parsed.invalid_reason {
                            status = HookRunStatus::Failed;
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Error,
                                text: invalid_reason,
                            });
                        } else if let Some(reason) = parsed.block_reason {
                            status = HookRunStatus::Blocked;
                            should_block = true;
                            block_reason = Some(reason.clone());
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Feedback,
                                text: reason,
                            });
                        } else {
                            updated_input = parsed.updated_input;
                        }
                    }
                } else if output_parser::looks_like_json(&run_result.stdout) {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "hook returned invalid pre-tool-use JSON output".to_string(),
                    });
                }
            }
            Some(2) if observational && handler.can_apply_control_effects() => {
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Warning,
                    text: OBSERVATIONAL_DECISION_IGNORED.to_string(),
                });
            }
            Some(2) if can_apply_control_effects => {
                if let Some(reason) = common::trimmed_non_empty(&run_result.stderr) {
                    status = HookRunStatus::Blocked;
                    should_block = true;
                    block_reason = Some(reason.clone());
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Feedback,
                        text: reason,
                    });
                } else {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "PreToolUse hook exited with code 2 but did not write a blocking reason to stderr".to_string(),
                    });
                }
            }
            Some(exit_code) => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: format!("hook exited with code {exit_code}"),
                });
            }
            None => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: "hook exited without a status code".to_string(),
                });
            }
        },
    }

    let completed = HookCompletedEvent {
        turn_id,
        run: dispatcher::completed_summary(handler, &run_result, status, entries),
    };

    dispatcher::ParsedHandler {
        completed,
        data: PreToolUseHandlerData {
            should_block,
            block_reason,
            additional_contexts_for_model,
            updated_input,
        },
        completion_order: 0,
    }
}

fn serialization_failure_outcome(hook_events: Vec<HookCompletedEvent>) -> PreToolUseOutcome {
    PreToolUseOutcome {
        hook_events,
        should_block: false,
        block_reason: None,
        additional_contexts: Vec::new(),
        updated_input: None,
    }
}

#[cfg(test)]
mod tests {
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::HookEventName;
    use codex_protocol::protocol::HookOutputEntry;
    use codex_protocol::protocol::HookOutputEntryKind;
    use codex_protocol::protocol::HookRunStatus;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;

    use super::DispatchMode;
    use super::OBSERVATIONAL_DECISION_IGNORED;
    use super::PreToolUseHandlerData;
    use super::command_input_json;
    use super::command_input_json_with_mode;
    use super::latest_updated_input;
    use super::parse_completed;
    use super::parse_completed_observational;
    use super::preview;
    use crate::engine::ConfiguredHandler;
    use crate::engine::HandlerRunResult;
    use crate::events::common;
    use crate::output_spill::AdditionalContext;
    use crate::output_spill::AdditionalContextLimit;

    #[test]
    fn command_input_uses_request_tool_name() {
        let mut request = request_for_tool_use("call-apply-patch");
        request.tool_name = "apply_patch".to_string();

        let input_json = command_input_json(&request).expect("serialize command input");
        let input: serde_json::Value =
            serde_json::from_str(&input_json).expect("parse command input");

        assert_eq!(input["tool_name"], "apply_patch");
    }

    #[test]
    fn permission_decision_deny_blocks_processing() {
        let parsed = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"do not run that"}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: true,
                block_reason: Some("do not run that".to_string()),
                additional_contexts_for_model: Vec::new(),
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Blocked);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Feedback,
                text: "do not run that".to_string(),
            }]
        );
    }

    #[test]
    fn permission_decision_allow_can_update_input() {
        let parsed = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"echo rewritten"}}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: false,
                block_reason: None,
                additional_contexts_for_model: Vec::new(),
                updated_input: Some(serde_json::json!({ "command": "echo rewritten" })),
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
        assert_eq!(parsed.completed.run.entries, vec![]);
    }

    #[test]
    fn last_completed_updated_input_wins() {
        let mut later_configured = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"echo configured later"}}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );
        later_configured.completion_order = 0;
        let mut earlier_configured = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"echo finished later"}}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );
        earlier_configured.completion_order = 1;

        assert_eq!(
            latest_updated_input(&[later_configured, earlier_configured]),
            Some(serde_json::json!({ "command": "echo finished later" }))
        );
    }

    #[test]
    fn permission_decision_allow_without_updated_input_fails_open() {
        let stdout = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","additionalContext":"preserved"}}"#;
        let parsed = parse_completed(
            &handler(),
            run_result(Some(0), stdout, ""),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: false,
                block_reason: None,
                additional_contexts_for_model: Vec::new(),
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Failed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: "PreToolUse hook returned unsupported permissionDecision:allow".to_string(),
            }]
        );

        let async_handler = handler_with_async(/*async*/ true);
        let parsed = parse_completed(
            &async_handler,
            run_result(Some(0), stdout, ""),
            Some("turn-1".to_string()),
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Context,
                text: "preserved".to_string(),
            }],
        );
    }

    #[test]
    fn deprecated_block_decision_blocks_processing() {
        let parsed = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"decision":"block","reason":"do not run that"}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: true,
                block_reason: Some("do not run that".to_string()),
                additional_contexts_for_model: Vec::new(),
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Blocked);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Feedback,
                text: "do not run that".to_string(),
            }]
        );
    }

    #[test]
    fn deprecated_block_decision_with_additional_context_blocks_processing() {
        let parsed = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"decision":"block","reason":"do not run that","hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"remember this"}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: true,
                block_reason: Some("do not run that".to_string()),
                additional_contexts_for_model: vec![AdditionalContext {
                    context: crate::HookContext::harness("remember this".to_string()),
                    limit: Default::default(),
                }],
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Blocked);
        assert_eq!(
            parsed.completed.run.entries,
            vec![
                HookOutputEntry {
                    kind: HookOutputEntryKind::Context,
                    text: "remember this".to_string(),
                },
                HookOutputEntry {
                    kind: HookOutputEntryKind::Feedback,
                    text: "do not run that".to_string(),
                },
            ]
        );
    }

    #[test]
    fn unsupported_permission_decision_fails_open() {
        let parsed = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"please confirm"}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: false,
                block_reason: None,
                additional_contexts_for_model: Vec::new(),
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Failed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: "PreToolUse hook returned unsupported permissionDecision:ask".to_string(),
            }]
        );
    }

    #[test]
    fn deprecated_approve_decision_fails_open() {
        let parsed = parse_completed(
            &handler(),
            run_result(Some(0), r#"{"decision":"approve"}"#, ""),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: false,
                block_reason: None,
                additional_contexts_for_model: Vec::new(),
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Failed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: "PreToolUse hook returned unsupported decision:approve".to_string(),
            }]
        );
    }

    #[test]
    fn additional_context_is_recorded() {
        let mut handler = handler();
        handler.additional_context_limit = AdditionalContextLimit::from_config(Some(13));
        let parsed = parse_completed(
            &handler,
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"do not run that","additionalContext":"nope"}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: true,
                block_reason: Some("do not run that".to_string()),
                additional_contexts_for_model: vec![AdditionalContext {
                    context: crate::HookContext::harness("nope".to_string()),
                    limit: AdditionalContextLimit::from_config(Some(13)),
                }],
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Blocked);
        assert_eq!(
            parsed.completed.run.entries,
            vec![
                HookOutputEntry {
                    kind: HookOutputEntryKind::Context,
                    text: "nope".to_string(),
                },
                HookOutputEntry {
                    kind: HookOutputEntryKind::Feedback,
                    text: "do not run that".to_string(),
                },
            ]
        );
    }

    #[test]
    fn observational_input_marks_payload_and_control_input_omits_marker() {
        let request = request_for_tool_use("call-wait");
        let control: serde_json::Value = serde_json::from_str(
            &command_input_json(&request).expect("serialize control command input"),
        )
        .expect("parse control command input");
        let observed: serde_json::Value = serde_json::from_str(
            &command_input_json_with_mode(&request, DispatchMode::Observational)
                .expect("serialize observational command input"),
        )
        .expect("parse observational command input");

        assert_eq!(control.get("observational"), None);
        assert_eq!(observed["observational"], true);
        assert_eq!(observed["hook_event_name"], "PreToolUse");
    }

    #[test]
    fn observational_deny_is_ignored_but_context_is_kept() {
        let parsed = parse_completed_observational(
            &handler(),
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"stop polling","additionalContext":"poll advisory"}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: false,
                block_reason: None,
                additional_contexts_for_model: vec![AdditionalContext {
                    text: "poll advisory".to_string(),
                    limit: AdditionalContextLimit::default(),
                }],
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![
                HookOutputEntry {
                    kind: HookOutputEntryKind::Context,
                    text: "poll advisory".to_string(),
                },
                HookOutputEntry {
                    kind: HookOutputEntryKind::Warning,
                    text: OBSERVATIONAL_DECISION_IGNORED.to_string(),
                },
            ]
        );
    }

    #[test]
    fn observational_allow_rewrite_is_ignored() {
        let parsed = parse_completed_observational(
            &handler(),
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"session_id":7}}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(parsed.data, PreToolUseHandlerData::default());
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
    }

    #[test]
    fn observational_exit_code_two_does_not_block() {
        let parsed = parse_completed_observational(
            &handler(),
            run_result(Some(2), "", "stop polling"),
            Some("turn-1".to_string()),
        );

        assert_eq!(parsed.data, PreToolUseHandlerData::default());
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Warning,
                text: OBSERVATIONAL_DECISION_IGNORED.to_string(),
            }]
        );
    }

    #[test]
    fn plain_stdout_is_ignored() {
        let parsed = parse_completed(
            &handler(),
            run_result(Some(0), "hook ran successfully\n", ""),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: false,
                block_reason: None,
                additional_contexts_for_model: Vec::new(),
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
        assert_eq!(parsed.completed.run.entries, vec![]);
    }

    #[test]
    fn invalid_json_like_stdout_fails_instead_of_becoming_noop() {
        let parsed = parse_completed(
            &handler(),
            run_result(Some(0), "{\"decision\":\n", ""),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: false,
                block_reason: None,
                additional_contexts_for_model: Vec::new(),
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Failed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: "hook returned invalid pre-tool-use JSON output".to_string(),
            }]
        );
    }

    #[test]
    fn exit_code_two_blocks_processing() {
        let parsed = parse_completed(
            &handler(),
            run_result(Some(2), "", "blocked by policy\n"),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PreToolUseHandlerData {
                should_block: true,
                block_reason: Some("blocked by policy".to_string()),
                additional_contexts_for_model: Vec::new(),
                updated_input: None,
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Blocked);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Feedback,
                text: "blocked by policy".to_string(),
            }]
        );
    }

    #[test]
    fn preview_and_completed_run_ids_include_tool_use_id() {
        let request = request_for_tool_use("tool-call-123");
        let runs = preview(&[handler()], &request);

        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].id,
            format!(
                "pre-tool-use:0:{}:tool-call-123",
                test_path_buf("/tmp/hooks.json").display()
            )
        );

        let parsed = parse_completed(
            &handler(),
            run_result(Some(0), "", ""),
            Some("turn-1".to_string()),
        );
        let completed = common::hook_completed_for_tool_use(parsed.completed, &request.tool_use_id);

        assert_eq!(completed.run.id, runs[0].id);
    }

    #[test]
    fn serialization_failure_run_ids_include_tool_use_id() {
        let request = request_for_tool_use("tool-call-123");
        let runs = preview(&[handler()], &request);

        let completed = common::serialization_failure_hook_events_for_tool_use(
            vec![handler()],
            Some(request.turn_id.clone()),
            "serialize failed".into(),
            &request.tool_use_id,
        );

        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].run.id, runs[0].id);
    }

    fn handler() -> ConfiguredHandler {
        handler_with_async(/*async*/ false)
    }

    fn handler_with_async(r#async: bool) -> ConfiguredHandler {
        ConfiguredHandler {
            builtin: false,
            plugin_trust: None,
            event_name: HookEventName::PreToolUse,
            matcher: Some(crate::engine::HookMatcher::new("^Bash$").expect("valid matcher")),
            timeout_sec: 5,
            status_message: None,
            additional_context_limit: Default::default(),
            source_path: test_path_buf("/tmp/hooks.json").abs().into(),
            source: codex_protocol::protocol::HookSource::User,
            display_order: 0,
            kind: crate::engine::ConfiguredHandlerKind::Command {
                command: "echo hook".to_string(),
                r#async,
                env: std::collections::HashMap::new(),
            },
        }
    }

    fn run_result(exit_code: Option<i32>, stdout: &str, stderr: &str) -> HandlerRunResult {
        HandlerRunResult {
            context_metadata: codex_protocol::models::ContentItemMetadata::harness(),
            started_at: 1,
            completed_at: 2,
            duration_ms: 1,
            exit_code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            error: None,
        }
    }

    fn request_for_tool_use(tool_use_id: &str) -> super::PreToolUseRequest {
        super::PreToolUseRequest {
            session_id: ThreadId::new(),
            turn_id: "turn-1".to_string(),
            subagent: None,
            cwd: test_path_buf("/tmp").abs(),
            transcript_path: None,
            model: "gpt-test".to_string(),
            permission_mode: "default".to_string(),
            tool_name: "Bash".to_string(),
            matcher_aliases: Vec::new(),
            tool_use_id: tool_use_id.to_string(),
            tool_input: serde_json::json!({ "command": "echo hello" }),
        }
    }
}
