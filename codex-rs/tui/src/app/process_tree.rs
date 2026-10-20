//! Tree-wide `/ps`: background terminals of the root and every open subagent.
//!
//! Without subagents, `/ps` keeps its single-thread history summary. With subagents, it lists
//! each thread's terminals through `thread/backgroundTerminals/list` and opens a picker whose
//! rows show the owning agent, elapsed time, command, and whether the exit wakes the owner.

use super::agent_picker::agent_picker_row_name;
use super::process_tree_view::PROCESS_TREE_VIEW_ID;
use super::process_tree_view::process_tree_view_params;
use super::process_tree_view::recent_output_lines;
use super::*;
use crate::app_event::ProcessTreeEntry;
use crate::exec_command::split_command_string;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::history_cell::UnifiedExecProcessDetails;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadBackgroundTerminalsListParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsListResponse;
use codex_app_server_protocol::ThreadBackgroundTerminalsTerminateParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsTerminateResponse;
use futures::StreamExt;

impl App {
    pub(super) fn open_process_tree(&mut self, app_server: &AppServerSession) {
        let Some(root) = self.primary_thread_id else {
            self.chat_widget.add_ps_output();
            return;
        };
        let root_name = crate::multi_agents::format_agent_picker_item_name(
            /*agent_nickname*/ None, /*agent_role*/ None, /*is_primary*/ true,
        );
        let mut owners = vec![(root, root_name)];
        owners.extend(
            self.agent_navigation
                .ordered_threads()
                .into_iter()
                .filter(|(thread_id, entry)| *thread_id != root && !entry.is_closed)
                .map(|(thread_id, entry)| {
                    (
                        thread_id,
                        agent_picker_row_name(thread_id, entry, /*is_primary*/ false),
                    )
                }),
        );
        if owners.len() == 1 {
            self.chat_widget.add_ps_output();
            return;
        }

        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let lists = futures::stream::iter(owners.into_iter().map(|(thread_id, owner)| {
                let request_handle = request_handle.clone();
                async move {
                    let result = request_handle
                        .request_typed::<ThreadBackgroundTerminalsListResponse>(
                            ClientRequest::ThreadBackgroundTerminalsList {
                                request_id: RequestId::String(Uuid::new_v4().to_string()),
                                params: ThreadBackgroundTerminalsListParams {
                                    thread_id: thread_id.to_string(),
                                    cursor: None,
                                    limit: None,
                                    include_recent_output: true,
                                },
                            },
                        )
                        .await;
                    (thread_id, owner, result)
                }
            }))
            // Ordered, so rows stay grouped root first, then subagents in picker order.
            .buffered(16)
            .collect::<Vec<_>>()
            .await;
            let mut entries = Vec::new();
            let mut failed_owners = Vec::new();
            for (thread_id, owner, result) in lists {
                let response = match result {
                    Ok(response) => response,
                    Err(error) => {
                        tracing::debug!(%error, %thread_id, "failed to list background terminals");
                        failed_owners.push(owner);
                        continue;
                    }
                };
                entries.extend(response.data.into_iter().map(|terminal| {
                    ProcessTreeEntry {
                        thread_id,
                        owner: owner.clone(),
                        process_id: terminal.process_id,
                        command: strip_bash_lc_and_escape(&split_command_string(&terminal.command)),
                        started_at: terminal.started_at,
                        wakes_owner: terminal.wakes_owner,
                        recent_output: terminal
                            .recent_output
                            .as_deref()
                            .map(recent_output_lines)
                            .unwrap_or_default(),
                    }
                }));
            }
            app_event_tx.send(AppEvent::ProcessTreeLoaded {
                primary_thread_id: root,
                entries,
                failed_owners,
            });
        });
    }

    pub(super) fn show_process_tree(
        &mut self,
        root: ThreadId,
        entries: Vec<ProcessTreeEntry>,
        failed_owners: Vec<String>,
    ) {
        if self.primary_thread_id != Some(root) {
            return;
        }
        if !failed_owners.is_empty() {
            self.chat_widget.add_error_message(format!(
                "Failed to list background terminals for: {}",
                failed_owners.join(", ")
            ));
        }
        if entries.is_empty() {
            if failed_owners.is_empty() {
                self.chat_widget
                    .add_to_history(history_cell::new_unified_exec_processes_output(Vec::new()));
            }
            return;
        }
        let params = process_tree_view_params(&entries, chrono::Utc::now().timestamp());
        if !self
            .chat_widget
            .replace_selection_view_if_present(PROCESS_TREE_VIEW_ID, params)
            // The listing is async; never push the picker over a view opened in the meantime.
            && self.chat_widget.no_modal_or_popup_active()
        {
            self.chat_widget
                .show_selection_view(process_tree_view_params(
                    &entries,
                    chrono::Utc::now().timestamp(),
                ));
        }
    }

    pub(super) fn show_background_terminal_output(&mut self, entry: ProcessTreeEntry) {
        self.chat_widget
            .add_to_history(history_cell::new_unified_exec_processes_output(vec![
                UnifiedExecProcessDetails {
                    command_display: format!("{}: {}", entry.owner, entry.command),
                    recent_chunks: entry.recent_output,
                },
            ]));
    }

    /// Terminates one terminal, as `/stop` does for all of the current thread's terminals.
    pub(super) fn stop_background_terminal(
        &mut self,
        app_server: &AppServerSession,
        entry: ProcessTreeEntry,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<ThreadBackgroundTerminalsTerminateResponse>(
                    ClientRequest::ThreadBackgroundTerminalsTerminate {
                        request_id: RequestId::String(Uuid::new_v4().to_string()),
                        params: ThreadBackgroundTerminalsTerminateParams {
                            thread_id: entry.thread_id.to_string(),
                            process_id: entry.process_id.clone(),
                        },
                    },
                )
                .await;
            let message = match result {
                Ok(response) if response.terminated => Ok(format!(
                    "Stopped background terminal for {}: {}",
                    entry.owner, entry.command
                )),
                Ok(_) => Err(format!(
                    "Background terminal for {} already exited: {}",
                    entry.owner, entry.command
                )),
                Err(error) => Err(format!("Failed to stop background terminal: {error}")),
            };
            app_event_tx.send(AppEvent::BackgroundTerminalStopped(message));
        });
    }
}
