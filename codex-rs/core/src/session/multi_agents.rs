use crate::agent::types::ResolvedMultiAgentV2UsageHints;
use crate::config::MultiAgentV2Config;
use crate::context::MultiAgentRoleInstructions;
use crate::session::step_context::StepContext;
use codex_features::AgentPolling;
use codex_prompts::ResolvedMessage;
use codex_prompts::ResolvedModelMessages;
use codex_prompts::ResolvedMultiAgentMessages;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

const SPAWN_AGENT_WAKE_ON_REPORT_TEXT: &str = "Delegation is asynchronous. Continue with independent work while children work. When no independent work remains, end your turn; a child report starts another turn and includes the result. Results may also arrive during your turn.";
const LIST_AGENTS_DESCRIPTION: &str =
    "List live agents in the current root thread tree. Optionally filter by task-path prefix.";
const LIST_AGENTS_WAKE_ON_REPORT_DESCRIPTION: &str = "Look up the names and task paths of live agents in the current root thread tree, to address them with `send_message` or `followup_task`. Optionally filter by task-path prefix.";

/// How a MultiAgentV2 thread learns that a child agent has reported back.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChildReportMode {
    /// The thread calls `wait_agent` to block until a child's mailbox update arrives.
    #[default]
    WaitAgent,
    /// The thread ends its turn while children work, and a child's final answer starts its
    /// next turn when a child reports back.
    WakeOnReport,
}

impl ChildReportMode {
    pub(crate) fn for_thread(
        config: &MultiAgentV2Config,
        session_source: &SessionSource,
        wake_mode_active: bool,
    ) -> Self {
        if config.agent_polling == AgentPolling::Enabled || !wake_mode_active {
            Self::WaitAgent
        } else {
            match session_source {
                SessionSource::Exec | SessionSource::Internal(_) => Self::WaitAgent,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                | SessionSource::Cli
                | SessionSource::VSCode
                | SessionSource::Mcp
                | SessionSource::Custom(_)
                | SessionSource::Unknown => Self::WakeOnReport,
                SessionSource::SubAgent(
                    SubAgentSource::Review
                    | SubAgentSource::Compact
                    | SubAgentSource::MemoryConsolidation
                    | SubAgentSource::Other(_),
                ) => Self::WaitAgent,
            }
        }
    }

    pub(crate) fn for_spawned_thread(config: &MultiAgentV2Config, wake_mode_active: bool) -> Self {
        if config.agent_polling == AgentPolling::Disabled && wake_mode_active {
            Self::WakeOnReport
        } else {
            Self::WaitAgent
        }
    }

    pub(crate) fn spawn_agent_guidance(self) -> Option<&'static str> {
        match self {
            Self::WaitAgent => None,
            Self::WakeOnReport => Some(SPAWN_AGENT_WAKE_ON_REPORT_TEXT),
        }
    }

    pub(crate) fn list_agents_description(self) -> &'static str {
        match self {
            Self::WaitAgent => LIST_AGENTS_DESCRIPTION,
            Self::WakeOnReport => LIST_AGENTS_WAKE_ON_REPORT_DESCRIPTION,
        }
    }
}

pub(super) fn usage_hint_text(step_context: &StepContext) -> Option<MultiAgentRoleInstructions> {
    let turn_context = step_context.turn.as_ref();
    if turn_context.multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    let multi_agent_messages =
        ResolvedModelMessages::from_model(&step_context.settings.model_info).multi_agent();
    let root_polling_enabled = ChildReportMode::for_thread(
        &turn_context.config.multi_agent_v2,
        &turn_context.session_source,
        turn_context.wake_mode_active,
    ) == ChildReportMode::WaitAgent;
    let subagent_polling_enabled = ChildReportMode::for_spawned_thread(
        &turn_context.config.multi_agent_v2,
        turn_context.wake_mode_active,
    ) == ChildReportMode::WaitAgent;
    let snapshot = resolve_usage_hints_with_polling_modes(
        &turn_context.config.multi_agent_v2,
        multi_agent_messages,
        !turn_context.config.update_plan_enabled && turn_context.config.model_catalog.is_none(),
        root_polling_enabled,
        subagent_polling_enabled,
    );
    match &turn_context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => snapshot.subagent,
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => snapshot.root,
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

pub(crate) fn resolve_usage_hints(
    config: &MultiAgentV2Config,
    multi_agent_messages: ResolvedMultiAgentMessages<'_>,
    omit_update_plan_instructions: bool,
) -> ResolvedMultiAgentV2UsageHints {
    resolve_usage_hints_with_root_polling(
        config,
        multi_agent_messages,
        omit_update_plan_instructions,
        config.agent_polling == AgentPolling::Enabled,
    )
}

pub(crate) fn resolve_usage_hints_with_root_polling(
    config: &MultiAgentV2Config,
    multi_agent_messages: ResolvedMultiAgentMessages<'_>,
    omit_update_plan_instructions: bool,
    root_polling_enabled: bool,
) -> ResolvedMultiAgentV2UsageHints {
    resolve_usage_hints_with_polling_modes(
        config,
        multi_agent_messages,
        omit_update_plan_instructions,
        root_polling_enabled,
        /*subagent_polling_enabled*/ true,
    )
}

pub(crate) fn resolve_usage_hints_with_polling_modes(
    config: &MultiAgentV2Config,
    multi_agent_messages: ResolvedMultiAgentMessages<'_>,
    omit_update_plan_instructions: bool,
    root_polling_enabled: bool,
    subagent_polling_enabled: bool,
) -> ResolvedMultiAgentV2UsageHints {
    let resolve_role = |configured: Option<&str>,
                        message: ResolvedMessage<'_>,
                        agent_polling_enabled: bool,
                        is_root: bool| {
        // Configured roles take precedence; empty configured or catalog roles suppress fallback.
        if let Some(configured) = configured {
            return (!configured.is_empty()).then(|| {
                let text = if agent_polling_enabled {
                    configured.to_owned()
                } else if is_root {
                    format!("{configured}\n\n{SPAWN_AGENT_WAKE_ON_REPORT_TEXT}")
                } else {
                    format!(
                        "{configured}\n\n{}",
                        codex_prompts::SUBAGENT_WAKE_ON_REPORT_USAGE_HINT_TEXT
                    )
                };
                MultiAgentRoleInstructions::Configured(text)
            });
        }

        let base = message.text();
        if base.is_empty() {
            return None;
        }
        Some(MultiAgentRoleInstructions::Composed {
            base: base.to_owned(),
            marked: message.catalog_override().is_some(),
            omit_update_plan_instructions,
            max_concurrency: config.max_concurrent_threads_per_session,
            agent_polling_enabled,
            expose_model_overrides: config.expose_spawn_agent_model_overrides,
            is_root,
        })
    };

    ResolvedMultiAgentV2UsageHints {
        root: resolve_role(
            config.root_agent_usage_hint_text.as_deref(),
            multi_agent_messages.root,
            root_polling_enabled,
            /*is_root*/ true,
        ),
        subagent: resolve_role(
            config.subagent_usage_hint_text.as_deref(),
            multi_agent_messages.subagent,
            subagent_polling_enabled,
            /*is_root*/ false,
        ),
    }
}

pub(crate) fn effective_multi_agent_mode(step_context: &StepContext) -> Option<MultiAgentMode> {
    let turn_context = step_context.turn.as_ref();
    let settings = &step_context.settings;
    if turn_context.multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    let multi_agent_messages =
        ResolvedModelMessages::from_model(&settings.model_info).multi_agent();
    let hint = turn_context
        .config
        .multi_agent_v2
        .multi_agent_mode_hint_text
        .as_deref()
        .or(multi_agent_messages.hint);
    let multi_agent_mode = match hint {
        Some(text) => MultiAgentMode::Custom(text.to_owned()),
        None => {
            let (message, builtin) =
                if settings.effective_reasoning_effort() == Some(ReasoningEffort::Ultra) {
                    (multi_agent_messages.proactive, MultiAgentMode::Proactive)
                } else {
                    (
                        multi_agent_messages.explicit,
                        MultiAgentMode::ExplicitRequestOnly,
                    )
                };
            match message {
                ResolvedMessage::Catalog(text) => MultiAgentMode::Custom(text.to_owned()),
                ResolvedMessage::Bundled(_) => builtin,
            }
        }
    };

    match &turn_context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        | SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => Some(multi_agent_mode),
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}
