//! Request-scoped settings and capabilities, with live grants bound to the originating turn.

use std::sync::Arc;

use crate::agents_md::LoadedAgentsMd;
use crate::config::TokenBudgetConfig;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::realtime_conversation::RealtimeConversationSnapshot;
use crate::session::step_settings::ResolvedStepSettings;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::router::ToolRouter;
use codex_exec_server::ExecutorCapabilityDiscoverySnapshot;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_extension_api::ExtensionData;
use codex_file_system::EnvironmentAccess;
use codex_file_system::FileSystemEnvironmentAccessor;
use codex_mcp::McpBinding;
use codex_otel::SessionTelemetry;
use codex_protocol::items::ModelInvocationContext;
use codex_protocol::protocol::TurnContextItem;
use codex_tools::LocalNavigationAccess;
use tokio_util::sync::CancellationToken;

/// Request-scoped state that may change between model sampling requests.
pub(crate) struct StepContext {
    pub(crate) turn: Arc<TurnContext>,
    /// Preempts this request and yields its code-mode observations when user input arrives.
    pub(crate) preempt: Option<CancellationToken>,
    /// Realtime call activity and instructions captured for this sampling request.
    pub(crate) realtime: RealtimeConversationSnapshot,
    /// One immutable settings version captured before request preparation.
    pub(crate) settings: Arc<ResolvedStepSettings>,
    /// Frozen turn preferences resolved against this step's captured model.
    pub(crate) token_budget: Option<TokenBudgetConfig>,
    /// Telemetry context tagged with this sampling request's model.
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) environments: TurnEnvironmentSnapshot,
    /// Capability roots bound to ready environments in this exact step.
    pub(crate) selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
    /// Executor-materialized capability files shared by MCP and skills in this exact step.
    pub(crate) executor_capability_discovery: Option<Arc<ExecutorCapabilityDiscoverySnapshot>>,
    /// Keeps the extension inputs used to build this step's tools for its World State as well.
    pub(crate) extension_data: ExtensionData,
    /// The exact MCP connections, configuration, and catalog captured for this step.
    pub(crate) mcp: Arc<McpBinding>,
    /// The finalized tool plan advertised and executed for this exact sampling request.
    pub(crate) tool_router: Arc<ToolRouter>,
    /// The canonical AGENTS.md value observed with this environment snapshot.
    pub(crate) loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
}

impl StepContext {
    /// Whether new context windows should record tool declarations in history.
    pub(crate) fn incremental_tools_enabled(&self) -> bool {
        self.settings.model_info.use_responses_lite
            && self
                .turn
                .config
                .features
                .enabled(codex_features::Feature::IncrementalTools)
    }

    /// Pairs the step's environments with access using current session and originating-turn grants.
    pub(crate) fn environments(&self) -> Vec<(&TurnEnvironment, impl EnvironmentAccess + '_)> {
        self.environments
            .turn_environments()
            .map(|environment| {
                let grants = self
                    .turn
                    .granted_permissions(&environment.selection.environment_id);
                (environment, environment.fs_accessor(grants))
            })
            .collect()
    }

    /// Capture grants once so the accessor and navigation verdict have identical authority.
    pub(crate) fn extension_environments(
        &self,
    ) -> Vec<(
        &TurnEnvironment,
        impl EnvironmentAccess + '_,
        LocalNavigationAccess,
    )> {
        self.environments
            .turn_environments()
            .map(|environment| {
                let grants = self
                    .turn
                    .granted_permissions(&environment.selection.environment_id);
                let sandbox = environment.sandbox_context(grants);
                // A pending/failed second selection must not disappear into a single
                // ready environment and authorize a broader local index.
                let access = local_navigation_access(
                    environment.environment.is_remote(),
                    sandbox.should_read_from_sandbox() || self.environments.environments.len() != 1,
                );
                let fs = FileSystemEnvironmentAccessor::new(
                    environment.environment.filesystem_ref(),
                    sandbox,
                );
                (environment, fs, access)
            })
            .collect()
    }

    /// Persist the context captured for this request, even after a live update.
    pub(crate) fn to_turn_context_item(&self) -> TurnContextItem {
        let mut item = self.turn.to_turn_context_item();
        item.realtime_active = Some(self.realtime.active);
        item
    }

    pub(crate) fn model_context(&self) -> ModelInvocationContext {
        ModelInvocationContext {
            model_slug: self.settings.model_info.slug.clone(),
            reasoning_effort: self
                .settings
                .effective_reasoning_effort()
                .map(|effort| effort.to_string()),
        }
    }
}

fn local_navigation_access(remote: bool, restricted_reads: bool) -> LocalNavigationAccess {
    match (remote, restricted_reads) {
        (true, _) => LocalNavigationAccess::Remote,
        (false, true) => LocalNavigationAccess::Restricted,
        (false, false) => LocalNavigationAccess::Allowed,
    }
}

#[cfg(all(test, unix))]
mod navigation_authority_tests {
    use super::*;
    use crate::environment_selection::TurnEnvironmentState;
    use codex_exec_server::Environment;
    use codex_protocol::models::AdditionalPermissionProfile;
    use codex_protocol::models::FileSystemPermissions;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::models::PermissionProfileSnapshot;
    use codex_utils_path_uri::PathUri;

    #[tokio::test]
    async fn extension_authority_captures_real_locality_and_effective_grants_together() {
        let (_session, turn) = crate::session::tests::make_session_and_context().await;
        let turn = Arc::new(turn);
        let mut step = StepContext::for_test(turn.clone());
        let step = Arc::get_mut(&mut step).expect("unshared step");
        let mut environment = step
            .environments
            .turn_environments()
            .next()
            .expect("local environment")
            .clone();
        let environment_id = environment.selection.environment_id.clone();
        environment.config_mut().permission_profile =
            PermissionProfileSnapshot::legacy(PermissionProfile::default());
        step.environments.environments = vec![TurnEnvironmentState::Ready(environment.clone())];
        let before = step.extension_environments();
        assert_eq!(before[0].2, LocalNavigationAccess::Restricted);
        turn.record_granted_permissions(
            &environment_id,
            AdditionalPermissionProfile {
                file_system: Some(FileSystemPermissions::from_read_write_path_uris(
                    Some(vec![PathUri::from_host_native_path("/").expect("root")]),
                    None,
                )),
                ..Default::default()
            },
            false,
        );
        let after = step.extension_environments();
        assert_eq!(after[0].2, LocalNavigationAccess::Allowed);
        assert_ne!(before[0].1.cache_key(), after[0].1.cache_key());
        // A callback already holding the old accessor retains the old verdict and grants.
        assert_eq!(before[0].2, LocalNavigationAccess::Restricted);
        drop(before);
        drop(after);
        step.environments
            .environments
            .push(TurnEnvironmentState::Ready(environment.clone()));
        assert!(
            step.extension_environments()
                .iter()
                .all(|(_, _, access)| *access == LocalNavigationAccess::Restricted)
        );
        // Same id and local-looking cwd, but the actual executor is remote.
        environment.environment = Arc::new(
            Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_owned()))
                .expect("remote environment"),
        );
        step.environments.environments = vec![TurnEnvironmentState::Ready(environment)];
        assert_eq!(
            step.extension_environments()[0].2,
            LocalNavigationAccess::Remote
        );
    }
}
