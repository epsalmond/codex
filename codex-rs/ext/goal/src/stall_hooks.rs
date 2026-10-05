//! Lifecycle adapter for accepted host observations, independent of replay files.

use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolOutputInput;
use codex_extension_api::ToolStartInput;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;

use crate::runtime::GoalRuntimeHandle;
use crate::stall::digest;

pub(crate) struct GuardContributor;

impl TurnLifecycleContributor for GuardContributor {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            // Idle admission already owns the permit. This callback only seeds
            // turn-local state; it must not recursively acquire the goal permit.
            if input.token_usage_at_turn_start.is_some()
                && let Some(runtime) = input.thread_store.get::<GoalRuntimeHandle>()
                && runtime.is_enabled()
                && let Ok(Some(goal)) = runtime
                    .goal_store()
                    .get_thread_goal(runtime.thread_id())
                    .await
            {
                runtime.guard().begin(input.turn_id, &goal);
            }
        })
    }

    fn on_item_completed<'a>(
        &'a self,
        thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
        item: &'a TurnItem,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = thread_store
                .get::<GoalRuntimeHandle>()
                .filter(|runtime| runtime.is_enabled())
            else {
                return;
            };
            if let TurnItem::UserMessage(message) = item
                && message.content.iter().any(|input| !matches!(input, UserInput::Text { text, .. } if text.trim().is_empty()))
            {
                runtime.guard().with_turn(turn_store.level_id(), |turn| turn.observer.fresh_input());
                if let Err(error) = runtime.recover_guard(/*report*/ None).await {
                    tracing::warn!(%error, "failed to recover goal continuation after user input");
                }
            }
        })
    }

    fn on_item_recorded<'a>(
        &'a self,
        thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
        item: &'a ResponseItem,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = thread_store
                .get::<GoalRuntimeHandle>()
                .filter(|runtime| runtime.is_enabled())
            else {
                return;
            };
            match item {
                ResponseItem::Message {
                    role,
                    content,
                    phase,
                    ..
                } if role == "assistant" => {
                    let text = content
                        .iter()
                        .map(|part| match part {
                            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                                Some(text.as_str())
                            }
                            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
                        })
                        .collect::<Option<Vec<_>>>();
                    runtime.guard().with_turn(turn_store.level_id(), |turn| {
                        let Some(text) = text else {
                            turn.observer.unknown();
                            return;
                        };
                        let text = text.join("\n");
                        if *phase == Some(MessagePhase::Commentary) {
                            turn.observer.commentary_text(&text);
                        } else {
                            turn.observer.final_text(&text);
                        }
                    });
                }
                ResponseItem::AgentMessage {
                    id: Some(id),
                    author,
                    recipient,
                    content,
                    ..
                } => {
                    match crate::stall_observation::agent_content_kind(content) {
                        crate::stall_observation::AgentTextKind::TaskAdmission => return,
                        crate::stall_observation::AgentTextKind::Empty => {
                            runtime
                                .guard()
                                .with_turn(turn_store.level_id(), |turn| turn.observer.unknown());
                            return;
                        }
                        crate::stall_observation::AgentTextKind::Result => {}
                    }
                    runtime.guard().with_turn(turn_store.level_id(), |turn| {
                        if let Ok(content) = serde_json::to_value(content) {
                            turn.observer.external_result(&content);
                        } else {
                            turn.observer.unknown();
                        }
                    });
                    if let Err(error) = runtime
                        .recover_guard(Some(digest((author, recipient, id.as_str()))))
                        .await
                    {
                        tracing::warn!(%error, "failed to recover goal continuation after accepted agent result");
                    }
                }
                // These items are internal context or observed through tool lifecycle hooks.
                ResponseItem::Message { .. }
                | ResponseItem::Reasoning { .. }
                | ResponseItem::AdditionalTools { .. }
                | ResponseItem::ConfigurationUpdate { .. }
                | ResponseItem::FunctionCall { .. }
                | ResponseItem::CustomToolCall { .. }
                | ResponseItem::FunctionCallOutput { .. }
                | ResponseItem::CustomToolCallOutput { .. } => {}
                // Provider-native work has no corresponding accepted tool output here.
                ResponseItem::WebSearchCall { .. }
                | ResponseItem::ImageGenerationCall { .. }
                | ResponseItem::Other
                | ResponseItem::LocalShellCall { .. }
                | ResponseItem::ToolSearchCall { .. }
                | ResponseItem::ToolSearchOutput { .. }
                | ResponseItem::AgentMessage { id: None, .. }
                | ResponseItem::Compaction { .. }
                | ResponseItem::ContextCompaction { .. }
                | ResponseItem::CompactionTrigger {} => {
                    runtime
                        .guard()
                        .with_turn(turn_store.level_id(), |turn| turn.observer.unknown());
                }
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(runtime) = input
                .thread_store
                .get::<GoalRuntimeHandle>()
                .filter(|runtime| runtime.is_enabled())
                && let Err(error) = runtime.finish_guard(input.turn_store.level_id()).await
            {
                tracing::warn!(%error, "failed to assess automatic goal continuation");
            }
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(runtime) = input
                .thread_store
                .get::<GoalRuntimeHandle>()
                .filter(|runtime| runtime.is_enabled())
            {
                runtime
                    .guard()
                    .with_turn(input.turn_store.level_id(), |turn| turn.observer.unknown());
                if let Err(error) = runtime.finish_guard(input.turn_store.level_id()).await {
                    tracing::warn!(%error, "failed to reset goal suspicion after an aborted turn");
                }
            }
        })
    }
}

impl ToolLifecycleContributor for GuardContributor {
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if let Some(runtime) = input
                .thread_store
                .get::<GoalRuntimeHandle>()
                .filter(|runtime| runtime.is_enabled())
            {
                runtime
                    .guard()
                    .with_turn(input.turn_id, |turn| turn.start(&input));
            }
        })
    }
    fn on_tool_output<'a>(&'a self, input: ToolOutputInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if let Some(runtime) = input
                .thread_store
                .get::<GoalRuntimeHandle>()
                .filter(|runtime| runtime.is_enabled())
            {
                runtime
                    .guard()
                    .with_turn(input.turn_id, |turn| turn.output(&input));
            }
        })
    }
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if !matches!(input.outcome, ToolCallOutcome::Completed { .. })
                && let Some(runtime) = input
                    .thread_store
                    .get::<GoalRuntimeHandle>()
                    .filter(|runtime| runtime.is_enabled())
            {
                runtime
                    .guard()
                    .with_turn(input.turn_id, |turn| turn.observer.unknown());
            }
        })
    }
}
