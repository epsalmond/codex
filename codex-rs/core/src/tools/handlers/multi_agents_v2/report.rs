//! `report_to_parent`: the only way a subagent messages its ancestors under typed reports.
//!
//! `blocked` and `final` end the turn and reach the parent as its terminal report. `answer`
//! replies to an ancestor's question without ending the turn and spends one answer credit.
//! A terminal report is rejected while the agent still owns unfinished work, since its parent
//! would not receive it until that work settles.

use super::message_tool::message_content;
use super::*;
use crate::TurnStartOptions;
use crate::agent::child_config::build_agent_resume_config;
use crate::session::async_completion::PendingRecord;
use crate::session::typed_reports::PendingTerminalReport;
use crate::session::typed_reports::TerminalReportKind;
use crate::session::typed_reports::typed_report_agent_path;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::handlers::multi_agents_spec::REPORT_TOOL_NAME;
use crate::tools::handlers::multi_agents_spec::create_report_tool;
use codex_protocol::AgentPath;
use codex_tools::ToolSpec;

/// Pending work items named in a rejected terminal report before "and N more".
const MAX_LISTED_PENDING_WORK: usize = 5;
/// Bounds each command label quoted in a rejected terminal report.
const MAX_PENDING_COMMAND_BYTES: usize = 80;

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
    to: Option<String>,
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
    let ToolInvocation {
        session,
        turn,
        source,
        ..
    } = invocation;
    let Some(agent_path) = typed_report_agent_path(&turn) else {
        return Err(FunctionCallError::RespondToModel(format!(
            "{REPORT_TOOL_NAME} is only available to subagents with typed reports enabled."
        )));
    };
    let terminal_kind = match args.kind {
        ReportKind::Blocked => TerminalReportKind::Blocked,
        ReportKind::Final => TerminalReportKind::Final,
        ReportKind::Answer => {
            return send_answer(&session, &turn, &agent_path, args.to, message).await;
        }
    };
    if turn.extension_data.get::<PendingTerminalReport>().is_some() {
        return Err(FunctionCallError::RespondToModel(
            "You already reported this turn; your turn ends after this step.".to_string(),
        ));
    }
    if let Some(rejection) = pending_work_rejection(&session, &turn, terminal_kind, &source) {
        return Err(FunctionCallError::RespondToModel(rejection));
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

/// Rejects a terminal report while ending the turn would leave the wake assignment Waiting, as
/// turn-end classification decides, so the report would be held until that work settles. The
/// code cell making the call is not pending work of its own. The hold in
/// `maybe_notify_parent_of_terminal_turn` remains the backstop for work started after this check.
fn pending_work_rejection(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    kind: TerminalReportKind,
    source: &ToolCallSource,
) -> Option<String> {
    let runtime = &session.services.local_agent_runtime;
    if !runtime.wake_mode_enabled() {
        return None;
    }
    let assignment = turn.agent_assignment.get()?;
    let asking_cell = match source {
        ToolCallSource::CodeMode { cell_id, .. } => Some(cell_id.as_str()),
        ToolCallSource::Direct | ToolCallSource::DirectPlaintextMessage => None,
    };
    let asking = asking_cell.and_then(|cell_id| {
        session
            .services
            .code_mode_service
            .work_registration(&codex_code_mode::CellId::new(cell_id.to_owned()))
    });
    let children = runtime.pending_wake_turn_end(assignment, asking.as_ref())?;
    let completions = session
        .services
        .async_completions
        .pending()
        .into_iter()
        .filter(|record| {
            record.wakes_owner
                && !(record.process_id.is_none() && record.cell_id.as_deref() == asking_cell)
        })
        .collect::<Vec<_>>();
    Some(pending_work_message(kind, &completions, &children, turn))
}

/// Names the pending work, then offers only remedies whose tools this turn exposes.
fn pending_work_message(
    kind: TerminalReportKind,
    completions: &[PendingRecord],
    children: &[String],
    turn: &crate::session::turn_context::TurnContext,
) -> String {
    let kind = match kind {
        TerminalReportKind::Blocked => "blocked",
        TerminalReportKind::Final => "final",
    };
    let mut items = Vec::new();
    let (mut cells, mut processes) = (false, false);
    for record in completions {
        let mut item = match (record.process_id, record.cell_id.as_deref()) {
            (Some(process_id), _) => {
                processes = true;
                match record.command.as_deref() {
                    Some(command) => format!("process {process_id}: `{}`", command_label(command)),
                    None => format!("process {process_id}"),
                }
            }
            (None, Some(cell_id)) => {
                cells = true;
                format!("code cell {cell_id}")
            }
            (None, None) => continue,
        };
        if record.ready {
            item.push_str(" (finished; its result has not reached you yet)");
        }
        items.push(item);
    }
    items.extend(children.iter().map(|child| format!("agent `{child}`")));

    let mut message = format!(
        "report_to_parent(kind=\"{kind}\") was not recorded: you still own unfinished work, and your parent would not receive this report until it settles.\nPending:\n"
    );
    if items.is_empty() {
        message.push_str("- background work whose result has not reached you yet\n");
    }
    for item in items.iter().take(MAX_LISTED_PENDING_WORK) {
        message.push_str(&format!("- {item}\n"));
    }
    if items.len() > MAX_LISTED_PENDING_WORK {
        message.push_str(&format!(
            "- and {} more\n",
            items.len() - MAX_LISTED_PENDING_WORK
        ));
    }
    message.push_str(
        "To report, end your turn without calling report_to_parent; you will be woken when this work finishes, and can report then.",
    );
    // `wait` is visible unless Code Mode results are delivered automatically, and empty
    // `write_stdin` polls are refused only while automatic completion is active.
    let cell_wait = !turn.code_mode_automatic_completion();
    let process_poll = !turn.automatic_completion_active();
    let mut waits = Vec::new();
    if cells && cell_wait {
        waits.push("`wait` for code cells");
    }
    if processes && process_poll {
        waits.push("`write_stdin` with empty chars for processes");
    }
    if !waits.is_empty() {
        message.push_str(&format!(
            " Or wait for it now, then report: {}.",
            waits.join(", ")
        ));
    }
    let mut stops = Vec::new();
    if cells {
        stops.push(if cell_wait {
            "`wait` with `terminate: true` for code cells"
        } else {
            "`exec_terminate` for code cells"
        });
    }
    if processes {
        stops.push("`write_stdin` with chars \"\\u0003\" for processes");
    }
    if !children.is_empty() && turn.config.multi_agent_v2.close_agents_tool {
        stops.push("`close_agents` for agents");
    }
    if !stops.is_empty() {
        message.push_str(&format!(
            " Or stop it first, then report: {}.",
            stops.join(", ")
        ));
    }
    message
}

fn command_label(command: &str) -> String {
    let label = codex_utils_string::take_bytes_at_char_boundary(command, MAX_PENDING_COMMAND_BYTES);
    if label.len() < command.len() {
        format!("{label}…")
    } else {
        label.to_owned()
    }
}

async fn send_answer(
    session: &std::sync::Arc<crate::session::session::Session>,
    turn: &std::sync::Arc<crate::session::turn_context::TurnContext>,
    agent_path: &AgentPath,
    to: Option<String>,
    message: String,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let ancestor = to
        .as_deref()
        .map(|target| {
            AgentPath::try_from(target)
                .ok()
                .filter(|ancestor| ancestor.is_ancestor_of(agent_path))
                .ok_or_else(|| FunctionCallError::RespondToModel(format!(
                    "Answer recipient `{target}` must be an ancestor agent path of `{agent_path}`."
                )))
        })
        .transpose()?;
    let Some(credit) = session.take_answer_credit(ancestor.as_ref()) else {
        let message = match ancestor {
            Some(ancestor) => format!(
                "No unanswered ancestor message from `{ancestor}`; no answer credit was consumed."
            ),
            None => ANSWER_WITHOUT_QUESTION.to_string(),
        };
        return Err(FunctionCallError::RespondToModel(message));
    };
    let ancestor_path = credit.ancestor();
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
        Ok(_) => {
            let output = format!("Answer delivered to `{ancestor_path}`. Continue your task.");
            credit.commit();
            Ok(FunctionToolOutput::from_text(output, Some(true)))
        }
        Err(err) => Err(err),
    }
}
