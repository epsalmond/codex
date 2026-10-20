//! Pending background jobs shown for idle agents in `/subagents`.
//!
//! A job is pending while its result will still be delivered to the agent: a running background
//! process or code cell registered for automatic completion, or a finished result the agent has
//! not accepted yet. The app server lists them per thread with `thread/pendingCompletions/list`.

use super::agent_context_display::snapshot_description;
use super::*;
use crate::exec_command::split_command_string;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::multi_agents::AgentPendingJob;
use crate::render::renderable::Renderable;
use crate::status_indicator_widget::fmt_elapsed_compact;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadContextUsage;
use codex_app_server_protocol::ThreadPendingCompletion;
use codex_app_server_protocol::ThreadPendingCompletionStatus;
use codex_app_server_protocol::ThreadPendingCompletionsListParams;
use codex_app_server_protocol::ThreadPendingCompletionsListResponse;
use futures::StreamExt;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

/// Core retains at most 64 completion records per thread, so one page lists them all.
const PENDING_JOBS_PAGE_SIZE: u32 = 64;

impl App {
    pub(super) fn refresh_agent_picker_pending_jobs(
        &mut self,
        app_server: &AppServerSession,
        root: ThreadId,
        thread_ids: Vec<ThreadId>,
    ) {
        if self.primary_thread_id != Some(root) {
            return;
        }
        let thread_ids: Vec<_> = thread_ids
            .into_iter()
            .filter(|thread_id| {
                self.agent_navigation
                    .get(thread_id)
                    .is_some_and(|entry| !entry.is_closed)
            })
            .collect();
        if thread_ids.is_empty() {
            return;
        }
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let results = futures::stream::iter(thread_ids.into_iter().map(|thread_id| {
                let request_handle = request_handle.clone();
                async move {
                    let result = request_handle
                        .request_typed::<ThreadPendingCompletionsListResponse>(
                            ClientRequest::ThreadPendingCompletionsList {
                                request_id: RequestId::String(Uuid::new_v4().to_string()),
                                params: ThreadPendingCompletionsListParams {
                                    thread_id: thread_id.to_string(),
                                    cursor: None,
                                    limit: Some(PENDING_JOBS_PAGE_SIZE),
                                },
                            },
                        )
                        .await
                        .map(|response| response.data.into_iter().map(pending_job).collect())
                        .map_err(|error| error.to_string());
                    (thread_id, result)
                }
            }))
            .buffer_unordered(16)
            .collect::<Vec<_>>()
            .await;
            app_event_tx.send(AppEvent::AgentPickerPendingJobsLoaded {
                primary_thread_id: root,
                results,
            });
        });
    }

    pub(super) fn apply_agent_picker_pending_jobs(
        &mut self,
        root: ThreadId,
        results: Vec<(ThreadId, Result<Vec<AgentPendingJob>, String>)>,
    ) {
        if self.primary_thread_id != Some(root) {
            return;
        }
        for (thread_id, result) in results {
            // A list requested before the agent's turn started is stale: the turn consumes
            // delivered results, and the jobs are relisted once it completes.
            if self
                .agent_navigation
                .get(&thread_id)
                .is_some_and(|entry| entry.is_running)
            {
                continue;
            }
            let jobs = result
                .inspect_err(|error| {
                    tracing::debug!(%error, %thread_id, "failed to list subagent pending jobs");
                })
                .ok();
            self.agent_navigation.set_pending_jobs(thread_id, jobs);
        }
        self.update_agent_picker_rows_if_present();
    }
}

fn pending_job(completion: ThreadPendingCompletion) -> AgentPendingJob {
    let command = match (
        &completion.command,
        &completion.process_id,
        &completion.cell_id,
    ) {
        (Some(command), _, _) => strip_bash_lc_and_escape(&split_command_string(command)),
        (None, Some(process_id), _) => format!("process {process_id}"),
        (None, None, Some(cell_id)) => format!("code cell {cell_id}"),
        (None, None, None) => "code cell".to_string(),
    };
    AgentPendingJob {
        command,
        started_at: completion.created_at,
        finished: completion.status == ThreadPendingCompletionStatus::Ready,
        wakes_owner: completion.wakes_owner,
    }
}

/// The idle row status, extended with pending work once it has been listed. Only jobs whose
/// result starts the agent's turn count as waiting; the rest are marked "no wake".
pub(super) fn idle_status_label(jobs: Option<&[AgentPendingJob]>) -> String {
    let Some(jobs) = jobs else {
        return "idle".to_string();
    };
    let plural = |count: usize| if count == 1 { "job" } else { "jobs" };
    let waking = jobs.iter().filter(|job| job.wakes_owner).count();
    match (waking, jobs.len()) {
        (_, 0) => "idle · nothing pending".to_string(),
        (0, total) => format!("idle · {total} {}, no wake", plural(total)),
        (waking, _) => format!("idle · waiting on {waking} {}", plural(waking)),
    }
}

/// Highlighted-row details: the captured context snapshot, then each pending job.
pub(super) struct AgentPickerRowDetails {
    pub(super) context: Option<ThreadContextUsage>,
    pub(super) jobs: Vec<AgentPendingJob>,
}

impl AgentPickerRowDetails {
    /// Text lines for the details pane, recomputing ages against `now` (Unix seconds).
    pub(super) fn lines(&self, now: i64) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(context) = &self.context {
            lines.push(snapshot_description(context, now));
        }
        if !self.jobs.is_empty() {
            lines.push("Pending jobs:".to_string());
        }
        for job in &self.jobs {
            let state = if job.finished {
                "finished, awaiting delivery".to_string()
            } else {
                let elapsed = u64::try_from(now.saturating_sub(job.started_at)).unwrap_or(0);
                format!("running {}", fmt_elapsed_compact(elapsed))
            };
            let wake = if job.wakes_owner { "" } else { " · no wake" };
            lines.push(format!("  • {} · {state}{wake}", job.command));
        }
        lines
    }

    /// Wrapped lines for each entry from `lines`, flagging pending-job entries.
    fn wrapped_entries(&self, width: u16) -> Vec<(bool, Vec<Line<'static>>)> {
        let width = usize::from(width.max(/*other*/ 1));
        let lines = self.lines(chrono::Utc::now().timestamp());
        let first_job = lines.len() - self.jobs.len();
        lines
            .iter()
            .enumerate()
            .map(|(idx, line)| {
                let wrapped = textwrap::wrap(line, width)
                    .into_iter()
                    .map(|line| Line::from(line.into_owned()))
                    .collect();
                (idx >= first_job, wrapped)
            })
            .collect()
    }
}

impl Renderable for AgentPickerRowDetails {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let entries = self.wrapped_entries(area.width);
        let height = usize::from(area.height);
        let total: usize = entries.iter().map(|(_, lines)| lines.len()).sum();
        let lines = if height == 0 || total <= height {
            entries.into_iter().flat_map(|(_, lines)| lines).collect()
        } else {
            // The popup reserves a capped details area; show whole entries and
            // summarize the rest.
            let mut lines = Vec::new();
            let mut shown = 0;
            for (_, entry) in &entries {
                if lines.len() + entry.len() >= height {
                    break;
                }
                lines.extend(entry.iter().cloned());
                shown += 1;
            }
            let hidden = &entries[shown..];
            let summary = if hidden.iter().all(|(is_job, _)| *is_job) {
                let jobs = if hidden.len() == 1 { "job" } else { "jobs" };
                format!("  … +{} more {jobs}", hidden.len())
            } else {
                let hidden_lines: usize = hidden.iter().map(|(_, lines)| lines.len()).sum();
                format!("  … +{hidden_lines} more lines")
            };
            lines.push(Line::from(summary).dim());
            lines
        };
        Renderable::render(&Paragraph::new(lines), area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let lines: usize = self
            .wrapped_entries(width)
            .iter()
            .map(|(_, lines)| lines.len())
            .sum();
        lines.try_into().unwrap_or(u16::MAX)
    }
}

#[cfg(test)]
#[path = "agent_picker_pending_tests.rs"]
mod tests;
