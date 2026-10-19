//! `report_to_parent`: the only way a subagent messages its ancestors under typed reports.
//!
//! `blocked` and `final` end the turn and reach the parent as its terminal report. `answer`
//! replies to an ancestor's question without ending the turn and spends one answer credit.

use super::message_tool::message_content;
use super::*;
use crate::TurnStartOptions;
use crate::agent::child_config::build_agent_resume_config;
use crate::session::typed_reports::PendingTerminalReport;
use crate::session::typed_reports::TerminalReportKind;
use crate::session::typed_reports::typed_report_agent_path;
use crate::tools::context::FunctionToolOutput;
use crate::tools::handlers::multi_agents_spec::REPORT_TOOL_NAME;
use crate::tools::handlers::multi_agents_spec::create_report_tool;
use codex_tools::ToolSpec;

/// Model-visible rejection for `report_to_parent(answer)` without an unanswered ancestor message.
pub(crate) const ANSWER_WITHOUT_QUESTION: &str = "report_to_parent(kind=\"answer\") only replies to a message an ancestor sent you, and no ancestor message is waiting for an answer. Do not send progress reports: use report_to_parent(kind=\"blocked\") if you need instructions to continue, or report_to_parent(kind=\"final\") when your task is done.";

pub(crate) struct Handler;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReportKind {
    Blocked,
    Answer,
    Final,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    kind: ReportKind,
    message: String,
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(REPORT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_report_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move { handle_report(invocation).await.map(boxed_tool_output) })
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

async fn handle_report(
    invocation: ToolInvocation,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let arguments = function_arguments(invocation.payload.clone())?;
    let args: ReportArgs = parse_arguments(&arguments)?;
    let message = message_content(args.message)?;
    let ToolInvocation { session, turn, .. } = invocation;
    if typed_report_agent_path(&turn).is_none() {
        return Err(FunctionCallError::RespondToModel(format!(
            "{REPORT_TOOL_NAME} is only available to subagents with typed reports enabled."
        )));
    }
    let terminal_kind = match args.kind {
        ReportKind::Blocked => TerminalReportKind::Blocked,
        ReportKind::Final => TerminalReportKind::Final,
        ReportKind::Answer => {
            return send_answer(&session, &turn, message).await;
        }
    };
    if turn.extension_data.get::<PendingTerminalReport>().is_some() {
        return Err(FunctionCallError::RespondToModel(
            "You already reported this turn; your turn ends after this step.".to_string(),
        ));
    }
    turn.extension_data.insert(PendingTerminalReport {
        kind: terminal_kind,
        message,
    });
    let output = match terminal_kind {
        TerminalReportKind::Blocked => {
            "Blocked report recorded. Your turn ends now; your parent will send instructions."
        }
        TerminalReportKind::Final => "Final report recorded. Your turn ends now.",
    };
    Ok(FunctionToolOutput::from_text(
        output.to_string(),
        Some(true),
    ))
}

async fn send_answer(
    session: &std::sync::Arc<crate::session::session::Session>,
    turn: &std::sync::Arc<crate::session::turn_context::TurnContext>,
    message: String,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let Some(ancestor_path) = session.take_answer_credit() else {
        return Err(FunctionCallError::RespondToModel(
            ANSWER_WITHOUT_QUESTION.to_string(),
        ));
    };
    let delivered = async {
        let ancestor = resolve_agent_target(session, turn, ancestor_path.as_str()).await?;
        let resume_config =
            build_agent_resume_config(turn).map_err(FunctionCallError::RespondToModel)?;
        session
            .services
            .agent_control
            .send_report_answer(
                session.thread_id,
                ancestor,
                resume_config,
                // The tool is fork-owned and its arguments are never encrypted.
                AgentMessage::Plaintext(message),
                TurnStartOptions {
                    parent_turn_id: Some(turn.sub_id.clone()),
                    root_turn_id: turn.turn_metadata_state.root_turn_id(),
                    turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
                    cyber_access_program: turn.cyber_access_program,
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| collab_v2_agent_error(ancestor, err))
    }
    .await;
    match delivered {
        Ok(_) => Ok(FunctionToolOutput::from_text(
            format!("Answer delivered to `{ancestor_path}`. Continue your task."),
            Some(true),
        )),
        Err(err) => {
            session.restore_answer_credit(ancestor_path);
            Err(err)
        }
    }
}
