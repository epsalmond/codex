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
    }
}

/// The idle row status, extended with pending work once it has been listed.
pub(super) fn idle_status_label(jobs: Option<&[AgentPendingJob]>) -> String {
    match jobs {
        None => "idle".to_string(),
        Some([]) => "idle · nothing pending".to_string(),
        Some([_]) => "idle · waiting on 1 job".to_string(),
        Some(jobs) => format!("idle · waiting on {} jobs", jobs.len()),
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
            lines.push(format!("  • {} · {state}", job.command));
        }
        lines
    }

    fn wrapped(&self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width.max(/*other*/ 1));
        self.lines(chrono::Utc::now().timestamp())
            .iter()
            .flat_map(|line| textwrap::wrap(line, width))
            .map(|line| Line::from(line.into_owned()))
            .collect()
    }
}

impl Renderable for AgentPickerRowDetails {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Renderable::render(&Paragraph::new(self.wrapped(area.width)), area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.wrapped(width).len().try_into().unwrap_or(u16::MAX)
    }
}

#[cfg(test)]
#[path = "agent_picker_pending_tests.rs"]
mod tests;
