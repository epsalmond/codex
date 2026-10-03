//! Root-scoped background refresh for the agent picker.

use super::agent_picker_status::context_description;
use super::agent_picker_status::picker_status_label;
use super::*;
use crate::bottom_pane::SelectionDescriptionLayout;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadSourceKind;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnItemsView;
use futures::StreamExt;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

pub(super) const AGENT_PICKER_VIEW_ID: &str = "agent-picker";
const AGENT_PICKER_PAGE_SIZE: u32 = 100;
const AGENT_PICKER_MAX_THREADS: usize = 1_000;

impl App {
    pub(super) fn refresh_agent_picker_threads(
        &mut self,
        app_server: &AppServerSession,
        root: ThreadId,
    ) {
        let Some(request_id) = self.agent_navigation.begin_picker_refresh(root) else {
            return;
        };
        let status_revisions = self.agent_navigation.status_revision_snapshot();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let mut threads = Vec::new();
                let mut cursor = None;
                let mut seen_cursors = HashSet::new();
                while threads.len() < AGENT_PICKER_MAX_THREADS
                    && seen_cursors.insert(cursor.clone())
                {
                    let page = match request_handle
                        .request_typed::<ThreadListResponse>(ClientRequest::ThreadList {
                            request_id: RequestId::String(Uuid::new_v4().to_string()),
                            params: ThreadListParams {
                                originators: None,
                                cursor,
                                limit: Some(AGENT_PICKER_PAGE_SIZE),
                                sort_key: None,
                                sort_direction: Some(SortDirection::Desc),
                                model_providers: Some(vec![]),
                                source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                                archived: None,
                                section_id: None,
                                project_id: None,
                                cwd: None,
                                use_state_db_only: true,
                                search_term: None,
                                parent_thread_id: None,
                                ancestor_thread_id: Some(root.to_string()),
                            },
                        })
                        .await
                    {
                        Ok(page) => page,
                        Err(err) if threads.is_empty() => return Err(err.to_string()),
                        Err(err) => {
                            tracing::warn!(%err, "failed to refresh remaining agent picker descendants");
                            break;
                        }
                    };
                    threads.extend(
                        page.data
                            .into_iter()
                            .take(AGENT_PICKER_MAX_THREADS - threads.len()),
                    );
                    let Some(next_cursor) = page.next_cursor else {
                        break;
                    };
                    cursor = Some(next_cursor);
                }
                threads.reverse();
                Ok(threads)
            }
            .await;

            app_event_tx.send(AppEvent::AgentPickerThreadsLoaded {
                primary_thread_id: root,
                request_id,
                status_revisions,
                result,
            });
        });
    }

    pub(super) fn refresh_agent_picker_previews(
        &mut self,
        app_server: &AppServerSession,
        root: ThreadId,
        thread_ids: Vec<ThreadId>,
    ) {
        if self.primary_thread_id != Some(root) {
            return;
        }
        let generation = self.agent_navigation.begin_picker_preview_generation(root);
        let mut candidate_count = 0;
        let mut requests = Vec::new();
        for thread_id in thread_ids {
            if thread_id == root || self.agent_navigation.get(&thread_id).is_none() {
                continue;
            }
            if candidate_count >= AGENT_PICKER_MAX_THREADS {
                break;
            }
            candidate_count += 1;
            if let Some(revision) = self.agent_navigation.begin_preview_backfill(thread_id) {
                requests.push((thread_id, revision));
            }
        }
        if requests.is_empty() {
            return;
        }

        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let semaphore = Arc::clone(
            &self
                .agent_navigation
                .picker_state
                .preview_backfill_semaphore,
        );
        tokio::spawn(async move {
            let results =
                futures::stream::iter(requests.into_iter().map(|(thread_id, revision)| {
                    let request_handle = request_handle.clone();
                    let semaphore = Arc::clone(&semaphore);
                    async move {
                        let result = match semaphore.acquire_owned().await {
                            Ok(_permit) => request_handle
                                .request_typed::<ThreadTurnsListResponse>(
                                    ClientRequest::ThreadTurnsList {
                                        request_id: RequestId::String(Uuid::new_v4().to_string()),
                                        params: ThreadTurnsListParams {
                                            thread_id: thread_id.to_string(),
                                            cursor: None,
                                            limit: Some(/*value*/ 20),
                                            sort_direction: Some(SortDirection::Desc),
                                            items_view: Some(TurnItemsView::Full),
                                        },
                                    },
                                )
                                .await
                                .map(|response| {
                                    super::agent_picker_status::latest_completed_response_preview(
                                        &response.data,
                                    )
                                })
                                .map_err(|error| error.to_string()),
                            Err(_) => Err("agent preview request limit closed".to_string()),
                        };
                        (thread_id, revision, result)
                    }
                }))
                .buffer_unordered(16)
                .collect::<Vec<_>>()
                .await;
            app_event_tx.send(AppEvent::AgentPickerPreviewsLoaded {
                primary_thread_id: root,
                generation,
                results,
            });
        });
    }

    pub(super) fn apply_agent_picker_thread_refresh(
        &mut self,
        app_server: &AppServerSession,
        root: ThreadId,
        request_id: Uuid,
        status_revisions: HashMap<ThreadId, u64>,
        result: Result<Vec<Thread>, String>,
    ) {
        if !self
            .agent_navigation
            .finish_picker_refresh(root, request_id)
            || self.primary_thread_id != Some(root)
        {
            return;
        }
        let threads = match result {
            Ok(threads) => threads,
            Err(err) => {
                tracing::warn!(%err, "failed to refresh agent picker descendants");
                return;
            }
        };
        for thread in threads {
            let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
                continue;
            };
            let live = self
                .thread_event_channels
                .get(&thread_id)
                .is_some_and(|channel| channel.attachment() == ThreadEventAttachment::Live);
            let previous = self.agent_navigation.get(&thread_id);
            let is_running = matches!(thread.status, ThreadStatus::Active { .. });
            let status_unchanged = self
                .agent_navigation
                .status_revision_matches(thread_id, &status_revisions);
            let update_liveness = status_unchanged && (previous.is_none() || !is_running);
            let is_closed = if status_unchanged {
                !live && matches!(thread.status, ThreadStatus::NotLoaded)
            } else {
                previous.is_some_and(|entry| entry.is_closed)
            };
            if !is_closed && previous.is_some_and(|entry| entry.is_closed) {
                continue;
            }
            let agent_path = crate::app_server_session::source_agent_path(&thread.source);
            let agent_nickname = thread
                .agent_nickname
                .or_else(|| previous.and_then(|entry| entry.agent_nickname.clone()));
            let agent_role = thread
                .agent_role
                .or_else(|| previous.and_then(|entry| entry.agent_role.clone()));
            if thread.can_accept_direct_input == Some(false) {
                self.agent_navigation.mark_parent_owned(thread_id);
            }
            self.upsert_agent_picker_thread(thread_id, agent_nickname, agent_role, is_closed);
            self.agent_navigation.set_agent_path(thread_id, agent_path);
            if status_unchanged {
                if matches!(thread.status, ThreadStatus::SystemError) {
                    self.agent_navigation
                        .set_error_from_discovery(thread_id, /*is_error*/ true);
                } else if is_running {
                    self.agent_navigation
                        .set_error_from_discovery(thread_id, /*is_error*/ false);
                }
            }
            if !live && update_liveness {
                self.agent_navigation
                    .set_running_from_discovery(thread_id, is_running);
            }
        }

        if self.update_agent_picker_rows_if_present()
            && self.chat_widget.active_view_id() == Some(AGENT_PICKER_VIEW_ID)
        {
            self.refresh_agent_picker_previews(
                app_server,
                root,
                self.agent_navigation.tracked_thread_ids(),
            );
        }
    }

    pub(super) fn apply_agent_picker_previews(
        &mut self,
        root: ThreadId,
        generation: Uuid,
        results: Vec<(ThreadId, u64, Result<Option<String>, String>)>,
    ) {
        if self.primary_thread_id != Some(root)
            || !self
                .agent_navigation
                .picker_state
                .preview_generation_matches(root, generation)
        {
            return;
        }

        for (thread_id, revision, mut result) in results {
            if let Err(error) = &result {
                if error.contains("ephemeral")
                    || error.contains("unavailable before first user message")
                    || error.contains("thread/turns/list is unavailable")
                {
                    result = Ok(None);
                } else {
                    tracing::warn!(%error, %thread_id, "failed to load subagent response preview");
                }
            }
            self.agent_navigation
                .finish_preview_backfill(root, generation, thread_id, revision, result);
        }
        self.update_agent_picker_rows_if_present();
    }

    pub(super) fn agent_picker_selection_view_params(
        &self,
        selected: Option<usize>,
    ) -> SelectionViewParams {
        let mut initial_selected_idx = selected;
        let items = self.agent_picker_selection_items(|idx, thread_id| {
            if initial_selected_idx.is_none() && self.active_thread_id == Some(thread_id) {
                initial_selected_idx = Some(idx);
            }
        });

        SelectionViewParams {
            view_id: Some(AGENT_PICKER_VIEW_ID),
            title: Some("Subagents".to_string()),
            subtitle: Some(AgentNavigationState::picker_subtitle()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx,
            is_searchable: true,
            search_placeholder: Some("Search subagents".to_string()),
            description_layout: SelectionDescriptionLayout::Columns,
            ..SelectionViewParams::picker()
        }
    }

    fn agent_picker_selection_items(
        &self,
        mut on_row: impl FnMut(usize, ThreadId),
    ) -> Vec<SelectionItem> {
        self.agent_navigation
            .ordered_threads()
            .into_iter()
            .enumerate()
            .map(|(idx, (thread_id, entry))| {
                on_row(idx, thread_id);
                let id = thread_id;
                let is_primary = self.primary_thread_id == Some(thread_id);
                let name = entry
                    .agent_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|agent_path| !is_primary && !agent_path.is_empty())
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| {
                        format_agent_picker_item_name(
                            entry.agent_nickname.as_deref(),
                            entry.agent_role.as_deref(),
                            is_primary,
                        )
                    });
                let details = self.agent_navigation.picker_details(&thread_id);
                let is_error = details.is_some_and(|details| details.is_error);
                let status = picker_status_label(entry.is_closed, entry.is_running, is_error);
                let mut description = vec![context_description(
                    details.and_then(|details| details.context_usage.as_ref()),
                )];
                if status == "idle"
                    && let Some(preview) =
                        details.and_then(|details| details.response_preview.as_ref())
                {
                    description.push(preview.clone());
                }
                let uuid = thread_id.to_string();
                SelectionItem {
                    selection_key: Some(uuid.clone()),
                    name: name.clone(),
                    name_prefix_spans: agent_picker_status_dot_spans(entry.is_closed, is_error),
                    description: Some(description.join(" · ")),
                    category_tag: Some(status.to_string()),
                    is_current: self.active_thread_id == Some(thread_id),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::SelectAgentThread(id));
                    })],
                    dismiss_on_select: true,
                    search_value: Some(format!("{name} {uuid}")),
                    ..Default::default()
                }
            })
            .collect()
    }

    pub(super) fn update_agent_picker_rows_if_present(&mut self) -> bool {
        let items = self.agent_picker_selection_items(|_, _| {});
        self.chat_widget
            .update_selection_items_if_present(AGENT_PICKER_VIEW_ID, items)
    }

    pub(super) fn cache_agent_picker_notification(
        &mut self,
        thread_id: ThreadId,
        notification: &ServerNotification,
    ) {
        if self.agent_navigation.get(&thread_id).is_none() {
            return;
        }

        match notification {
            ServerNotification::TurnStarted(_) => {
                self.agent_navigation
                    .set_error(thread_id, /*is_error*/ false);
            }
            ServerNotification::TurnCompleted(completed) => {
                self.agent_navigation.set_error(
                    thread_id,
                    matches!(completed.turn.status, TurnStatus::Failed),
                );
                let should_backfill = self.agent_navigation.note_completed_preview_turn(
                    thread_id,
                    &completed.turn.id,
                    super::agent_picker_status::response_preview_for_turn(&completed.turn),
                );
                if should_backfill
                    && self.primary_thread_id.is_some()
                    && self.chat_widget.active_view_id() == Some(AGENT_PICKER_VIEW_ID)
                {
                    self.app_event_tx
                        .send(AppEvent::AgentPickerPreviewNeeded(thread_id));
                }
            }
            ServerNotification::Error(error) if !error.will_retry => {
                self.agent_navigation
                    .set_error(thread_id, /*is_error*/ true);
            }
            ServerNotification::ThreadClosed(_) => {
                self.agent_navigation
                    .set_error(thread_id, /*is_error*/ false);
            }
            ServerNotification::ThreadStatusChanged(status) => match &status.status {
                ThreadStatus::Active { .. } => self.agent_navigation.mark_running(thread_id),
                ThreadStatus::Idle => self.agent_navigation.mark_stopped(thread_id),
                ThreadStatus::SystemError => {
                    self.agent_navigation.mark_stopped(thread_id);
                    self.agent_navigation
                        .set_error(thread_id, /*is_error*/ true);
                }
                ThreadStatus::NotLoaded
                    if !self
                        .thread_event_channels
                        .get(&thread_id)
                        .is_some_and(|channel| {
                            channel.attachment() == ThreadEventAttachment::Live
                        }) =>
                {
                    self.agent_navigation.mark_closed(thread_id);
                }
                ThreadStatus::NotLoaded => {}
            },
            ServerNotification::ThreadTokenUsageUpdated(usage) => {
                let context_usage = (usage.token_usage.last.total_tokens >= 0).then_some(
                    crate::multi_agents::AgentPickerContextUsage {
                        last_tokens: usage.token_usage.last.total_tokens,
                        model_context_window: usage
                            .token_usage
                            .model_context_window
                            .filter(|window| *window > 0),
                    },
                );
                self.agent_navigation
                    .set_context_usage(thread_id, context_usage);
            }
            ServerNotification::ThreadReverted(_) => {
                self.agent_navigation.clear_picker_details(thread_id);
                self.agent_navigation
                    .set_error(thread_id, /*is_error*/ false);
            }
            _ => {}
        }
    }
}
