//! Archive visibility follows backend lifecycle notifications and descendant discovery.
use super::agent_navigation::AgentPickerThreadVisibility;
use super::app_server_event_targets::ServerNotificationThreadTarget;
use super::app_server_event_targets::server_notification_thread_target;
use super::*;

impl App {
    pub(super) fn set_agent_picker_thread_visibility(
        &mut self,
        thread_id: ThreadId,
        visibility: AgentPickerThreadVisibility,
    ) {
        self.update_agent_picker_thread_visibility(thread_id, visibility);
        self.update_agent_picker_rows_if_present();
    }

    pub(super) fn update_agent_picker_thread_visibility(
        &mut self,
        thread_id: ThreadId,
        visibility: AgentPickerThreadVisibility,
    ) {
        if matches!(visibility, AgentPickerThreadVisibility::Hidden) {
            self.agent_navigation.mark_stopped(thread_id);
            if let Some(channel) = self.thread_event_channels.get_mut(&thread_id) {
                channel.mark_replay_only();
            }
        }
        self.agent_navigation
            .set_picker_thread_visibility(thread_id, visibility);
    }

    pub(super) fn handle_agent_picker_visibility_notification(
        &mut self,
        app_server: &AppServerSession,
        notification: &ServerNotification,
    ) {
        let visibility = match notification {
            ServerNotification::ThreadArchived(_) => AgentPickerThreadVisibility::Hidden,
            ServerNotification::ThreadUnarchived(_) => AgentPickerThreadVisibility::Visible,
            _ => return,
        };
        let ServerNotificationThreadTarget::Thread(thread_id) =
            server_notification_thread_target(notification)
        else {
            return;
        };
        let refresh_uncached_thread = matches!(visibility, AgentPickerThreadVisibility::Visible)
            && self.agent_navigation.get(&thread_id).is_none();
        self.set_agent_picker_thread_visibility(thread_id, visibility);
        if refresh_uncached_thread
            && let Some(primary_thread_id) = self.primary_thread_id
            && !self
                .agent_navigation
                .queue_picker_refresh(primary_thread_id)
        {
            self.refresh_agent_picker_threads(app_server, primary_thread_id);
        }
    }
}
