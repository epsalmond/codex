//! Assembles multi-agent role instructions from selected text and runtime capabilities.
//! The segment owns rendering and attribution; consumers select and capture its inputs.

use crate::without_update_plan_instructions;
use codex_context_fragments::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

const DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT: &str = "Full-history forks (`fork_turns` set to `\"all\"`) inherit the parent model and reasoning effort and do not accept overrides. Only set `model` or `reasoning_effort` when explicitly requested by the user, applicable `AGENTS.md` instructions, or skill instructions; when doing so, leave `fork_turns` unset or `\"none\"`.";
const DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT: &str = "When calling `wait_agent`, use long timeouts (minutes); it returns as soon as an agent reports.";
const ROOT_WAKE_ON_REPORT_USAGE_HINT_TEXT: &str = "Child results arrive as new turns; when you're waiting only on children, end your turn with a short status.";
pub const SUBAGENT_WAKE_ON_REPORT_USAGE_HINT_TEXT: &str = "When delegated work remains and you have no independent task, end your turn. A child report resumes your assignment in a new turn.";
const SUBAGENT_BLOCKED_USAGE_HINT_TEXT: &str = "If you are blocked, end your turn with your question; your parent will reply with a follow-up.";
const SUBAGENT_TYPED_REPORTS_USAGE_HINT_TEXT: &str = "Message your parent and other ancestors only with `report_to_parent`, and never send progress reports. Use `report_to_parent(kind=\"blocked\")` when something unexpected stops you and you need instructions: your turn ends and you wait for a follow-up. Use `report_to_parent(kind=\"answer\")` only to reply to a question an ancestor sent you. Use `report_to_parent(kind=\"final\")` when your task is done: it means done, so include everything your parent needs. Ending your turn without `report_to_parent` delivers your last message as your final report. Unless you were spawned as `persistent`, you are closed after your final report.";
const TYPED_REPORTS_DELEGATION_USAGE_HINT_TEXT: &str = "Agents you spawn report to you only through `report_to_parent`: `FINAL_ANSWER` when done, `BLOCKED` when they wait for your followup_task with instructions, and `ANSWER` when they reply to a question you sent with send_message. An agent's lifetime comes from its agent type and defaults to `task`: a `task` agent is closed after its final report, so spawn a fresh agent with a self-contained brief for new work. Use an agent type whose lifetime is `persistent` only for an agent that genuinely needs to keep its context across tasks.";
const DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT: &str = r#"Note that collaboration tools cannot be called from inside `functions.exec`. Call `spawn_agent`, `send_message`, `followup_task`, `wait_agent`, `interrupt_agent`, and `list_agents` only as direct tool calls using the recipient shown in their tool definitions, such as `to=functions.collaboration.spawn_agent`, since they are intentionally absent from the `functions.exec` `tools.*` namespace. Available tools in `functions.exec` are explicitly described with a `tools` namespace in the developer message.

All agents share the same directory. In detail:
- All agents have access to the same container and filesystem as you.
- All agents use the same current working directory.
- As a result, edits made by one agent are immediately visible to all other agents.
"#;

/// Multi-agent role text and the captured capabilities used to render its context segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiAgentRoleInstructions {
    /// Complete configured instructions, emitted verbatim without catalog markers.
    Configured(String),
    /// Selected catalog or bundled text, composed with captured runtime guidance.
    Composed {
        base: String,
        marked: bool,
        omit_update_plan_instructions: bool,
        max_concurrency: usize,
        agent_polling_enabled: bool,
        expose_model_overrides: bool,
        /// True for the root's role text, false for a subagent's.
        is_root: bool,
        /// Token count at which a subagent's context compacts, stated in its role text.
        subagent_context_token_cap: Option<u64>,
        /// Subagents report only through `report_to_parent`, and task agents close after reporting.
        typed_reports: bool,
    },
}

impl ContextualUserFragment for MultiAgentRoleInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.role_instructions".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        match self {
            Self::Composed { marked: true, .. } => Self::type_markers(),
            Self::Configured(_) | Self::Composed { marked: false, .. } => ("", ""),
        }
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<multi_agent_role>", "</multi_agent_role>")
    }

    fn body(&self) -> String {
        match self {
            Self::Configured(text) => text.clone(),
            Self::Composed {
                base,
                omit_update_plan_instructions,
                max_concurrency,
                agent_polling_enabled,
                expose_model_overrides,
                is_root,
                subagent_context_token_cap,
                typed_reports,
                ..
            } => {
                let base = if *omit_update_plan_instructions {
                    without_update_plan_instructions(base)
                } else {
                    base.clone()
                };
                let wait_agent_guidance =
                    format!("{DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT}\n\n");
                let shared = if *agent_polling_enabled {
                    DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT.to_string()
                } else {
                    DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT.replace("`wait_agent`, ", "")
                };
                let context_budget_guidance =
                    subagent_context_budget_guidance(*subagent_context_token_cap);
                let (blocked_guidance, delegation_guidance) = if *typed_reports {
                    (
                        SUBAGENT_TYPED_REPORTS_USAGE_HINT_TEXT,
                        format!("{TYPED_REPORTS_DELEGATION_USAGE_HINT_TEXT}\n\n"),
                    )
                } else {
                    (SUBAGENT_BLOCKED_USAGE_HINT_TEXT, String::new())
                };
                let role_guidance = match (*is_root, *agent_polling_enabled) {
                    (true, true) => format!("{wait_agent_guidance}{delegation_guidance}"),
                    (true, false) => {
                        format!("{ROOT_WAKE_ON_REPORT_USAGE_HINT_TEXT}\n\n{delegation_guidance}")
                    }
                    (false, true) => format!(
                        "{wait_agent_guidance}{blocked_guidance}\n\n{delegation_guidance}{context_budget_guidance}"
                    ),
                    (false, false) => format!(
                        "{SUBAGENT_WAKE_ON_REPORT_USAGE_HINT_TEXT}\n\n{blocked_guidance}\n\n{delegation_guidance}{context_budget_guidance}"
                    ),
                };
                let mut text = format!(
                    "{base}\n{shared}\n{role_guidance}There are {max_concurrency} available concurrency slots, meaning that up to {max_concurrency} agents can be active at once, including you."
                );
                if *expose_model_overrides {
                    text.push_str("\n\n");
                    text.push_str(DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT);
                }
                text
            }
        }
    }
}

fn subagent_context_budget_guidance(cap: Option<u64>) -> String {
    cap.map(|cap| {
        format!(
            "Your context budget is {cap} tokens; keep tool output focused and report partial results before you exceed it.\n\n"
        )
    })
    .unwrap_or_default()
}

#[cfg(test)]
#[path = "multi_agent_instructions_tests.rs"]
mod tests;
