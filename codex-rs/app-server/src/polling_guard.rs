//! Turn-local intervention for completed, serial, unchanged status checks.
//! This observes exact nested calls, never JavaScript source repetition or generic reads.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ExtensionWarning;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolDispatchInput;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolOutputInput;
use codex_extension_api::ToolStartInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;

mod observation;
mod reducer;

use reducer::Episode;

pub(crate) const POLLING_WARNING: &str = "Repeated status checks returned no new information. Requesting a pause; use a completion notification or one bounded wait helper before continuing.";

pub(crate) fn install(
    builder: &mut ExtensionRegistryBuilder<Config>,
    event_sink: Arc<dyn ExtensionEventSink>,
    thread_manager: Weak<ThreadManager>,
) {
    let guard = Arc::new(PollingGuard {
        event_sink,
        thread_manager,
    });
    builder.tool_lifecycle_contributor(guard.clone());
    builder.turn_lifecycle_contributor(guard);
}

struct PollingGuard {
    event_sink: Arc<dyn ExtensionEventSink>,
    thread_manager: Weak<ThreadManager>,
}

impl ToolLifecycleContributor for PollingGuard {
    fn on_tool_dispatch(&self, input: ToolDispatchInput<'_>) {
        let state = input
            .turn_store
            .get_or_init(|| Mutex::new(Episode::default()));
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dispatch(
                observation::call_key(input.call_id, &input.source),
                observation::intent(&input),
            );
    }

    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            let state = input
                .turn_store
                .get_or_init(|| Mutex::new(Episode::default()));
            let intervene = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .start(observation::start(&input));
            if !intervene {
                return;
            }
            // Claim is already turn-local and no mutex is held across cancellation.
            let Some(manager) = self.thread_manager.upgrade() else {
                return;
            };
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            let Ok(thread) = manager.get_thread(thread_id).await else {
                return;
            };
            self.event_sink.emit_warning(ExtensionWarning {
                thread_id: thread_id.to_string(),
                turn_id: Some(input.turn_id.to_owned()),
                message: POLLING_WARNING.to_owned(),
            });
            // This seam preserves pending user/mailbox input. It requests interruption,
            // not tool rejection: at most this final read-only check can still execute.
            if let Err(error) = thread.interrupt_if_no_pending_input(input.turn_id).await {
                tracing::warn!(%error, "failed to interrupt repeated status checks");
            }
        })
    }

    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if let Some(state) = input.turn_store.get::<Mutex<Episode>>() {
                state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .finish(
                        observation::call_key(input.call_id, &input.source),
                        input.outcome == ToolCallOutcome::Completed { success: true },
                    );
            }
        })
    }

    fn on_tool_output<'a>(&'a self, input: ToolOutputInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if let Some(state) = input.turn_store.get::<Mutex<Episode>>() {
                state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .output(
                        observation::call_key(input.call_id, &input.source),
                        input.output,
                    );
            }
        })
    }
}

impl TurnLifecycleContributor for PollingGuard {
    fn on_item_completed<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
        item: &'a TurnItem,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if matches!(item, TurnItem::UserMessage(_))
                && let Some(state) = turn_store.get::<Mutex<Episode>>()
            {
                state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .reset();
            }
        })
    }

    fn on_item_recorded<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
        item: &'a ResponseItem,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if matches!(
                item,
                ResponseItem::AgentMessage { .. }
                    | ResponseItem::WebSearchCall { .. }
                    | ResponseItem::ImageGenerationCall { .. }
                    | ResponseItem::ToolSearchCall { .. }
                    | ResponseItem::ToolSearchOutput { .. }
                    | ResponseItem::LocalShellCall { .. }
                    | ResponseItem::Other
            ) && let Some(state) = turn_store.get::<Mutex<Episode>>()
            {
                state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .reset();
            }
        })
    }
}
