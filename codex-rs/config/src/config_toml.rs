//! Schema-heavy configuration TOML types used by Codex.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::Path;

use crate::HooksToml;
use crate::browser_use::BrowserUseConfigToml;
use crate::computer_use::ComputerUseConfigToml;
use crate::permissions_toml::PermissionsToml;
use crate::profile_toml::ConfigProfile;
use crate::types::AnalyticsConfigToml;
use crate::types::ApprovalsReviewer;
use crate::types::AppsConfigToml;
use crate::types::AuthCredentialsStoreMode;
use crate::types::FeedbackConfigToml;
use crate::types::History;
use crate::types::MarketplaceConfig;
use crate::types::McpServerConfig;
use crate::types::MemoriesToml;
use crate::types::Notice;
use crate::types::OAuthCredentialsStoreMode;
use crate::types::OtelConfigToml;
use crate::types::PluginConfig;
use crate::types::SandboxWorkspaceWrite;
use crate::types::ShellEnvironmentPolicyToml;
use crate::types::SkillsConfig;
use crate::types::ToolSuggestConfig;
use crate::types::Tui;
use crate::types::UriBasedFileOpener;
use crate::types::WindowsToml;
use codex_features::FeaturesToml;
use codex_model_provider_info::AMAZON_BEDROCK_PROVIDER_ID;
use codex_model_provider_info::AMAZON_BEDROCK_RUNTIME_PROVIDER_ID;
use codex_model_provider_info::LEGACY_OLLAMA_CHAT_PROVIDER_ID;
use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OLLAMA_CHAT_PROVIDER_REMOVED_ERROR;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::config_types::TrustLevel;
use codex_protocol::config_types::Verbosity;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::config_types::WebSearchToolConfig;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path::normalize_for_path_comparison;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as SerdeError;
use serde_json::Value as JsonValue;

const RESERVED_MODEL_PROVIDER_IDS: [&str; 5] = [
    AMAZON_BEDROCK_PROVIDER_ID,
    AMAZON_BEDROCK_RUNTIME_PROVIDER_ID,
    OPENAI_PROVIDER_ID,
    OLLAMA_OSS_PROVIDER_ID,
    LMSTUDIO_OSS_PROVIDER_ID,
];

pub const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;

fn default_history() -> Option<History> {
    Some(History::default())
}

const fn default_project_doc_max_bytes() -> Option<usize> {
    Some(DEFAULT_PROJECT_DOC_MAX_BYTES)
}

fn default_project_doc_fallback_filenames() -> Option<Vec<String>> {
    Some(Vec::new())
}

const fn default_hide_agent_reasoning() -> Option<bool> {
    Some(false)
}

const fn default_true() -> bool {
    true
}

/// Backward-compatible shape for ChatGPT workspace login restrictions in config.toml.
#[derive(Serialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(untagged)]
pub enum ForcedChatgptWorkspaceIds {
    Single(String),
    Multiple(Vec<String>),
}

impl ForcedChatgptWorkspaceIds {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::Single(value) => vec![value],
            Self::Multiple(values) => values,
        }
    }
}

impl<'de> Deserialize<'de> for ForcedChatgptWorkspaceIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Single(String),
            Multiple(Vec<String>),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Single(value) if value.contains(',') => Err(D::Error::custom(
                "forced_chatgpt_workspace_id must be a single workspace ID string or a TOML list \
of strings; comma-separated strings are not supported. Use \
`forced_chatgpt_workspace_id = [\"123e4567-e89b-42d3-a456-426614174000\", \
\"123e4567-e89b-42d3-a456-426614174001\"]` instead.",
            )),
            Repr::Single(value) => Ok(Self::Single(value)),
            Repr::Multiple(values) => Ok(Self::Multiple(values)),
        }
    }
}

/// Orchestrator-owned feature settings.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorToml {
    pub skills: Option<OrchestratorFeatureToml>,
    pub mcp: Option<OrchestratorFeatureToml>,
}

/// Settings for a feature owned by the orchestrator.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorFeatureToml {
    pub enabled: Option<bool>,
}

/// Base config deserialized from ~/.codex/config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ConfigToml {
    /// Optional override of model selection.
    pub model: Option<String>,
    /// Review model override used by the `/review` feature.
    pub review_model: Option<String>,

    /// Provider to use from the model_providers map.
    pub model_provider: Option<String>,

    /// Size of the context window for the model, in tokens.
    pub model_context_window: Option<i64>,

    /// Token usage threshold triggering auto-compaction of conversation history.
    pub model_auto_compact_token_limit: Option<i64>,

    /// Controls whether the auto-compaction limit applies to the full context or
    /// only to tokens after the carried prefix in the current compaction window.
    pub model_auto_compact_token_limit_scope: Option<AutoCompactTokenLimitScope>,

    /// Automatic surgical context reduction ("auto-shake") run instead of
    /// auto-compaction when enough of the context is elidable.
    #[serde(default)]
    pub auto_shake: Option<AutoShakeToml>,

    /// Default approval policy for executing commands.
    #[schemars(with = "Option<crate::schema::ConfigAskForApproval>")]
    pub approval_policy: Option<AskForApproval>,

    /// Configures who approval requests are routed to for review once they have
    /// been escalated. This does not disable separate safety checks such as
    /// ARC.
    pub approvals_reviewer: Option<ApprovalsReviewer>,

    /// Optional policy instructions for the guardian auto-reviewer.
    #[serde(default)]
    pub auto_review: Option<AutoReviewToml>,

    pub browser_use: Option<BrowserUseConfigToml>,

    pub computer_use: Option<ComputerUseConfigToml>,

    #[serde(default)]
    pub shell_environment_policy: ShellEnvironmentPolicyToml,

    /// Whether the model may request a login shell for shell-based tools.
    /// Default to `true`
    ///
    /// If `true`, the model may request a login shell (`login = true`), and
    /// omitting `login` defaults to using a login shell.
    /// If `false`, the model can never use a login shell: `login = true`
    /// requests are rejected, and omitting `login` defaults to a non-login
    /// shell.
    pub allow_login_shell: Option<bool>,

    /// Sandbox mode to use.
    pub sandbox_mode: Option<SandboxMode>,

    /// Allow macOS sandbox writable roots at or beneath CODEX_HOME to traverse
    /// symlinks. Read only from the host's user config at startup; defaults to false.
    /// This grants no write access by itself, but trusts symlink targets even if
    /// they change between commands or lie outside CODEX_HOME.
    /// This setting has no effect on Linux or Windows.
    pub allow_symlinked_codex_home: Option<bool>,

    /// Sandbox configuration to apply if `sandbox` is `WorkspaceWrite`.
    pub sandbox_workspace_write: Option<SandboxWorkspaceWrite>,

    /// Default permissions profile to apply. Names starting with `:` refer to
    /// built-in profiles; other names are resolved from the `[permissions]`
    /// table.
    pub default_permissions: Option<String>,

    /// Named permissions profiles.
    #[serde(default)]
    pub permissions: Option<PermissionsToml>,

    /// Optional external command to spawn for end-user notifications.
    #[serde(default)]
    pub notify: Option<Vec<String>>,

    /// System instructions.
    pub instructions: Option<String>,

    /// Developer instructions inserted as a `developer` role message.
    #[serde(default)]
    pub developer_instructions: Option<String>,

    /// Whether to inject the `<permissions instructions>` developer block.
    pub include_permissions_instructions: Option<bool>,

    /// Whether to inject the `<apps_instructions>` developer block.
    pub include_apps_instructions: Option<bool>,

    /// Whether to inject the `<collaboration_mode>` developer block.
    pub include_collaboration_mode_instructions: Option<bool>,

    /// Whether to inject the `<environment_context>` user block.
    pub include_environment_context: Option<bool>,

    /// Optional path to a file containing model instructions that will override
    /// the built-in instructions for the selected model. Users are STRONGLY
    /// DISCOURAGED from using this field, as deviating from the instructions
    /// sanctioned by Codex will likely degrade model performance.
    pub model_instructions_file: Option<AbsolutePathBuf>,

    /// Compact prompt used for history compaction.
    pub compact_prompt: Option<String>,

    /// When set, restricts ChatGPT login to one or more workspace identifiers.
    #[serde(default)]
    pub forced_chatgpt_workspace_id: Option<ForcedChatgptWorkspaceIds>,

    /// When set, restricts the login mechanism users may use.
    #[serde(default)]
    pub forced_login_method: Option<ForcedLoginMethod>,

    /// Preferred backend for storing CLI auth credentials.
    /// file (default): Use a file in the Codex home directory.
    /// keyring: Use an OS-specific keyring service.
    /// auto: Use the keyring if available, otherwise use a file.
    #[serde(default)]
    pub cli_auth_credentials_store: Option<AuthCredentialsStoreMode>,

    /// Definition for MCP servers that Codex can reach out to for tool calls.
    #[serde(default)]
    // Uses the raw MCP input shape (custom deserialization) rather than `McpServerConfig`.
    #[schemars(schema_with = "crate::schema::mcp_servers_schema")]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// Preferred backend for storing MCP OAuth credentials.
    /// keyring: Use an OS-specific keyring service.
    ///          https://github.com/openai/codex/blob/main/codex-rs/rmcp-client/src/oauth.rs#L2
    /// file: Use a file in the Codex home directory.
    /// auto (default): Use the OS-specific keyring service if available, otherwise use a file.
    #[serde(default)]
    pub mcp_oauth_credentials_store: Option<OAuthCredentialsStoreMode>,

    /// Optional fixed port for the local HTTP callback server used during MCP OAuth login.
    /// When unset, Codex will bind to an ephemeral port chosen by the OS.
    pub mcp_oauth_callback_port: Option<u16>,

    /// Optional redirect URI to use during MCP OAuth login.
    /// When set, this URI is used in the OAuth authorization request instead
    /// of the local listener address. The local callback listener still binds
    /// to 127.0.0.1 (using `mcp_oauth_callback_port` when provided).
    pub mcp_oauth_callback_url: Option<String>,

    /// Milliseconds to wait for optional MCP servers while building the initial tool catalog.
    ///
    /// Defaults to 1000. Set to 0 to disable the shared grace and wait for each
    /// server's configured `startup_timeout_sec` instead.
    pub mcp_optional_startup_grace_ms: Option<u64>,

    /// User-defined provider entries that extend the built-in list. Built-in
    /// IDs cannot be overridden.
    #[serde(default, deserialize_with = "deserialize_model_providers")]
    pub model_providers: HashMap<String, ModelProviderInfo>,

    /// Maximum total bytes of project instruction content across all selected environments.
    #[serde(default = "default_project_doc_max_bytes")]
    pub project_doc_max_bytes: Option<usize>,

    /// Ordered list of fallback filenames to look for when AGENTS.md is missing.
    #[serde(default = "default_project_doc_fallback_filenames")]
    pub project_doc_fallback_filenames: Option<Vec<String>>,

    /// Token budget applied when storing tool/function outputs in the context manager.
    pub tool_output_token_limit: Option<usize>,

    /// Maximum poll window for background terminal output (`write_stdin`), in milliseconds.
    /// Default: `300000` (5 minutes).
    pub background_terminal_max_timeout: Option<u64>,

    /// Seconds a thread must have no subscribers and no activity before app-server
    /// unloads it. Defaults to 60; zero unloads immediately. Changes require a server restart.
    pub thread_unload_delay_secs: Option<u64>,

    /// Deprecated: ignored.
    #[schemars(skip)]
    pub js_repl_node_path: Option<AbsolutePathBuf>,

    /// Deprecated: ignored.
    #[schemars(skip)]
    pub js_repl_node_module_dirs: Option<Vec<AbsolutePathBuf>>,

    /// Profile to use from the `profiles` map.
    pub profile: Option<String>,

    /// Named profiles to facilitate switching between different configurations.
    #[serde(default)]
    pub profiles: HashMap<String, ConfigProfile>,

    /// Settings that govern if and what will be written to `~/.codex/history.jsonl`.
    #[serde(default = "default_history")]
    pub history: Option<History>,

    /// Directory where Codex stores the SQLite state DB.
    /// Defaults to `$CODEX_SQLITE_HOME` when set. Otherwise uses `$CODEX_HOME`.
    pub sqlite_home: Option<AbsolutePathBuf>,

    /// Directory where Codex writes log files. Setting this value explicitly
    /// also enables the TUI text log in this directory.
    /// Defaults to `$CODEX_HOME/log`.
    pub log_dir: Option<AbsolutePathBuf>,

    /// Optional URI-based file opener. If set, citations to files in the model
    /// output will be hyperlinked using the specified URI scheme.
    pub file_opener: Option<UriBasedFileOpener>,

    /// Collection of settings that are specific to the TUI.
    pub tui: Option<Tui>,

    /// When set to `true`, `AgentReasoning` events will be hidden from the
    /// UI/output. Defaults to `false`.
    #[serde(default = "default_hide_agent_reasoning")]
    pub hide_agent_reasoning: Option<bool>,

    /// When set to `true`, `AgentReasoningRawContentEvent` events will be shown in the UI/output.
    /// Defaults to `false`.
    pub show_raw_agent_reasoning: Option<bool>,

    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub plan_mode_reasoning_effort: Option<ReasoningEffort>,
    pub model_reasoning_summary: Option<ReasoningSummary>,
    /// Optional verbosity control for GPT-5 models (Responses API `text.verbosity`).
    pub model_verbosity: Option<Verbosity>,

    /// Optional path to a JSON model catalog (applied on startup only).
    /// Per-thread `config` overrides are accepted but do not reapply this (no-ops).
    pub model_catalog_json: Option<AbsolutePathBuf>,

    /// Optionally specify a personality for the model
    pub personality: Option<Personality>,

    /// Optional explicit service tier request id for new turns (for example
    /// `default`, `priority`, or `flex`; legacy `fast` also works).
    pub service_tier: Option<String>,

    /// Base URL for requests to ChatGPT (as opposed to the OpenAI API).
    pub chatgpt_base_url: Option<String>,

    /// Optional product SKU forwarded on host-owned Codex Apps MCP requests.
    pub apps_mcp_product_sku: Option<String>,

    /// Bounded, product-owned metadata attached to every Responses API request.
    pub responses_api_metadata: Option<BTreeMap<String, String>>,

    /// Orchestrator-owned feature settings.
    pub orchestrator: Option<OrchestratorToml>,

    /// Base URL override for the built-in `openai` model provider.
    pub openai_base_url: Option<String>,

    /// Machine-local realtime audio device preferences used by realtime voice.
    #[serde(default)]
    pub audio: Option<RealtimeAudioToml>,

    /// Experimental / do not use. Overrides only the realtime conversation
    /// websocket transport base URL (the `Op::RealtimeConversation`
    /// `/v1/realtime`
    /// connection) without changing normal provider HTTP requests.
    pub experimental_realtime_ws_base_url: Option<String>,
    /// Experimental / do not use. Overrides only the WebRTC realtime call
    /// creation base URL. This is separate from `experimental_realtime_ws_base_url`
    /// because WebRTC call creation is HTTP, while sideband control is websocket.
    pub experimental_realtime_webrtc_call_base_url: Option<String>,
    /// Experimental / do not use. Selects the realtime websocket model/snapshot
    /// used for the `Op::RealtimeConversation` connection.
    pub experimental_realtime_ws_model: Option<String>,
    /// Experimental / do not use. Realtime websocket session selection.
    /// `version` controls v1/v2 and `type` controls conversational/transcription.
    #[serde(default)]
    pub realtime: Option<RealtimeToml>,
    /// Experimental / do not use. Overrides only the realtime conversation
    /// websocket transport instructions (the `Op::RealtimeConversation`
    /// `/ws` session.update instructions) without changing normal prompts.
    pub experimental_realtime_ws_backend_prompt: Option<String>,
    /// Experimental / do not use. Replaces the synthesized realtime startup
    /// context appended to websocket session instructions. An empty string
    /// disables startup context injection entirely.
    pub experimental_realtime_ws_startup_context: Option<String>,
    /// Experimental / do not use. Replaces the built-in realtime start
    /// instructions inserted into developer messages when realtime becomes
    /// active.
    pub experimental_realtime_start_instructions: Option<String>,

    /// Removed. Former remote thread-store endpoint setting kept only so we can
    /// fail fast instead of silently falling back to local persistence.
    #[schemars(skip)]
    pub experimental_thread_store_endpoint: Option<String>,

    /// Experimental / do not use. Selects the thread store implementation.
    pub experimental_thread_store: Option<ThreadStoreToml>,
    pub projects: Option<HashMap<String, ProjectConfig>>,

    /// Controls the web search tool mode: disabled, cached, indexed, or live.
    pub web_search: Option<WebSearchMode>,

    /// Nested tools section for feature toggles
    pub tools: Option<ToolsToml>,

    /// Additional discoverable tools that can be suggested for installation.
    pub tool_suggest: Option<ToolSuggestConfig>,

    /// Agent-related settings (thread limits, etc.).
    pub agents: Option<AgentsToml>,

    /// Goal-related settings.
    pub goals: Option<GoalsToml>,

    /// Memories subsystem settings.
    pub memories: Option<MemoriesToml>,

    /// User-level skill config entries keyed by SKILL.md path.
    pub skills: Option<SkillsConfig>,

    /// Lifecycle hooks configured inline in TOML plus user-level overrides.
    pub hooks: Option<HooksToml>,

    /// User-level plugin config entries keyed by plugin name.
    #[serde(default)]
    pub plugins: HashMap<String, PluginConfig>,

    /// User-level marketplace entries keyed by marketplace name.
    #[serde(default)]
    pub marketplaces: HashMap<String, MarketplaceConfig>,

    /// Centralized feature flags (new). Prefer this over individual toggles.
    #[serde(default)]
    // Injects known feature keys into the schema and forbids unknown keys.
    #[schemars(schema_with = "crate::schema::features_schema")]
    pub features: Option<FeaturesToml>,

    /// Suppress warnings about unstable (under development) features.
    pub suppress_unstable_features_warning: Option<bool>,

    /// Compatibility-only settings retained so legacy `ghost_snapshot`
    /// config still loads.
    #[serde(default)]
    pub ghost_snapshot: Option<GhostSnapshotToml>,

    /// Markers used to detect the project root when searching parent
    /// directories for `.codex` folders. Defaults to [".git"] when unset.
    #[serde(default)]
    pub project_root_markers: Option<Vec<String>>,

    /// When `true`, checks for Codex updates on startup and surfaces update prompts.
    /// Set to `false` only if your Codex updates are centrally managed.
    /// Defaults to `true`.
    pub check_for_update_on_startup: Option<bool>,

    /// Legacy fallback for `tui.disable_paste_burst`. Prefer the setting under `[tui]`.
    pub disable_paste_burst: Option<bool>,

    /// When `false`, disables analytics across Codex product surfaces in this machine.
    /// Defaults to `true`.
    pub analytics: Option<AnalyticsConfigToml>,

    /// When `false`, disables feedback collection across Codex product surfaces.
    /// Defaults to `true`.
    pub feedback: Option<FeedbackConfigToml>,

    /// Settings for app-specific controls.
    #[serde(default)]
    pub apps: Option<AppsConfigToml>,

    /// Opaque desktop settings stored alongside the rest of config.toml.
    #[serde(default)]
    pub desktop: Option<HashMap<String, JsonValue>>,

    /// OTEL configuration.
    pub otel: Option<OtelConfigToml>,

    /// Windows-specific configuration.
    #[serde(default)]
    pub windows: Option<WindowsToml>,

    /// Collection of in-product notices (different from notifications)
    /// See [`crate::types::Notice`] for more details
    pub notice: Option<Notice>,

    pub experimental_compact_prompt_file: Option<AbsolutePathBuf>,
    pub experimental_use_unified_exec_tool: Option<bool>,
    /// Preferred OSS provider for local models, e.g. "lmstudio" or "ollama".
    pub oss_provider: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadStoreToml {
    Local {},
    #[schemars(skip)]
    InMemory {
        id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
pub struct AutoReviewToml {
    /// Additional policy instructions inserted into the guardian prompt.
    pub policy: Option<String>,
}

/// Automatic surgical context reduction ("auto-shake") settings.
///
/// Auto-shake runs at the same pre-sampling point where auto-compaction
/// decides. Fields left unset fall back to the built-in per-model-family
/// defaults, then to the built-in global defaults. See
/// `codex-rs/docs/shake.md`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AutoShakeToml {
    /// Global auto-shake threshold: `"off"`, an integer percent (`40`) or a
    /// percent string (`"40%"`) of the model's resolved context window, or an
    /// absolute token count (`"160k"`, `160000`) above 100. `"inherit"` is
    /// invalid at the global level (there is nothing to inherit from) and is
    /// rejected as a config error.
    pub threshold: Option<AutoShakeThresholdToml>,

    /// Skip auto-shake when the read-only preview reports that it would free
    /// less than this percent of the current context. Guards against thrash on
    /// histories with little elidable content. Global override.
    pub min_elidable_percent: Option<i64>,

    /// Absolute minimum-savings floor, in tokens. Skip auto-shake when the
    /// read-only preview reports it would free fewer tokens than this, even if
    /// `min_elidable_percent` is cleared — matters most on small context
    /// windows. Global override; defaults to 4000.
    pub min_savings_tokens: Option<i64>,

    /// Per-model-family overrides keyed by family prefix, e.g. `gpt-5.6` or
    /// `gpt-6-astra`. A family matches a slug that equals it or extends it with
    /// a `-` suffix (`gpt-5.6` matches `gpt-5.6-sol`), after provider and
    /// region qualifiers such as `us.openai.` are stripped.
    #[serde(default)]
    pub models: BTreeMap<String, AutoShakeModelToml>,

    /// Cold-resume trigger: shake when the thread has been idle longer than
    /// the provider's prompt-cache TTL, regardless of how full the context is.
    /// Defaults to `true`. Setting it to `false` disables only this trigger;
    /// `threshold = "off"` disables auto-shake entirely, this trigger
    /// included.
    pub cold_resume: Option<bool>,

    /// Prompt-cache TTL override applied to every provider: `"1h"`, `"30m"`,
    /// `"90s"`, or a bare integer number of seconds. Overrides the built-in
    /// per-provider table for every provider at once.
    pub cache_ttl: Option<AutoShakeDurationToml>,

    /// Per-provider prompt-cache TTL overrides, keyed by the `model_providers`
    /// id (`openai`, `amazon-bedrock`, `ollama`, ...). Highest precedence.
    #[serde(default)]
    pub providers: BTreeMap<String, AutoShakeProviderToml>,
}

/// Per-provider auto-shake overrides. An unset field falls back to the global
/// `auto_shake.cache_ttl`, then to the built-in per-provider table.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AutoShakeProviderToml {
    /// Prompt-cache TTL for this provider: `"1h"`, `"30m"`, `"90s"`, or a bare
    /// integer number of seconds.
    pub cache_ttl: Option<AutoShakeDurationToml>,
}

/// A duration in whole seconds, accepted either as a bare positive integer
/// (seconds) or as a string with a single `s`/`m`/`h`/`d` suffix (`"90s"`,
/// `"30m"`, `"1h"`, `"2d"`). Zero is allowed and means "always expired", which
/// is useful in tests; negative values are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AutoShakeDurationToml(pub i64);

impl AutoShakeDurationToml {
    pub fn as_secs(self) -> i64 {
        self.0
    }

    fn parse_str(value: &str) -> Result<Self, String> {
        let trimmed = value.trim();
        let (digits, multiplier) = match trimmed.chars().last() {
            Some('s' | 'S') => (&trimmed[..trimmed.len() - 1], 1),
            Some('m' | 'M') => (&trimmed[..trimmed.len() - 1], 60),
            Some('h' | 'H') => (&trimmed[..trimmed.len() - 1], 3_600),
            Some('d' | 'D') => (&trimmed[..trimmed.len() - 1], 86_400),
            _ => (trimmed, 1),
        };
        let parsed: i64 = digits.trim().parse().map_err(|_| Self::error(value))?;
        Self::checked(parsed.saturating_mul(multiplier), value)
    }

    fn checked(value: i64, original: impl std::fmt::Display) -> Result<Self, String> {
        if value < 0 {
            return Err(format!(
                "invalid auto_shake duration \"{original}\": must not be negative"
            ));
        }
        Ok(Self(value))
    }

    fn error(value: &str) -> String {
        format!(
            "invalid auto_shake duration {value:?}: expected a number of seconds (e.g. 300) or a \
             duration string like \"5m\", \"1h\" or \"2d\""
        )
    }
}

impl<'de> Deserialize<'de> for AutoShakeDurationToml {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(i64),
            Str(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Int(value) => {
                AutoShakeDurationToml::checked(value, value).map_err(SerdeError::custom)
            }
            Raw::Str(value) => AutoShakeDurationToml::parse_str(&value).map_err(SerdeError::custom),
        }
    }
}

impl Serialize for AutoShakeDurationToml {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl JsonSchema for AutoShakeDurationToml {
    fn schema_name() -> String {
        "AutoShakeDuration".to_string()
    }

    fn json_schema(_generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::InstanceType;
        use schemars::schema::Metadata;
        use schemars::schema::Schema;
        use schemars::schema::SchemaObject;
        Schema::Object(SchemaObject {
            instance_type: Some(vec![InstanceType::Integer, InstanceType::String].into()),
            metadata: Some(Box::new(Metadata {
                description: Some(
                    "A duration: a bare integer number of seconds (e.g. 300), or a string with an \
                     s/m/h/d suffix (e.g. \"90s\", \"30m\", \"1h\", \"2d\"). Must not be negative."
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        })
    }
}

/// Per-model-family auto-shake overrides. Unset fields inherit the built-in
/// family default, then the built-in global default.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AutoShakeModelToml {
    /// Family threshold: `"off"`, an integer/percent-string percent, an
    /// absolute token count (`"160k"`, `160000`) above 100, or `"inherit"` to
    /// defer to the resolved global `auto_shake.threshold`.
    pub threshold: Option<AutoShakeThresholdToml>,
    pub min_elidable_percent: Option<i64>,
}

/// Auto-shake threshold value, accepted in four forms:
///
/// - `"off"` — auto-shake disabled for this scope;
/// - an integer percent (`40`) or a percent string (`"40%"`) of the model's
///   resolved context window;
/// - an absolute token count, written as a string with a `k` suffix
///   (`"160k"`) or as a bare integer/string above 100 (`160000`,
///   `"200000"`);
/// - `"inherit"` — defer to the resolved global value. Valid only on a
///   per-family entry; a global `auto_shake.threshold = "inherit"` has
///   nothing to inherit from and is rejected as a config error.
///
/// Disambiguation for a bare integer (no `%` or `k` suffix): 1..=100 is a
/// percent, anything above 100 is an absolute token count. 0 and negative
/// values are rejected for every numeric form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoShakeThresholdToml {
    Off,
    Percent(i64),
    Tokens(i64),
    Inherit,
}

impl AutoShakeThresholdToml {
    fn parse_str(value: &str) -> Result<Self, String> {
        match value {
            "off" => return Ok(Self::Off),
            "inherit" => return Ok(Self::Inherit),
            _ => {}
        }
        if let Some(percent) = value.strip_suffix('%') {
            let percent: i64 = percent.parse().map_err(|_| {
                format!(
                    "invalid auto_shake threshold {value:?}: expected \"off\", \"inherit\", an integer percent, a percent string like \"40%\", or an absolute token count like \"160k\""
                )
            })?;
            return Self::checked_percent(percent, value);
        }
        if let Some(tokens) = value.strip_suffix('k').or_else(|| value.strip_suffix('K')) {
            let tokens: i64 = tokens.parse().map_err(|_| {
                format!(
                    "invalid auto_shake threshold {value:?}: expected \"off\", \"inherit\", an integer percent, a percent string like \"40%\", or an absolute token count like \"160k\""
                )
            })?;
            return Self::checked_tokens(tokens.saturating_mul(1_000), value);
        }
        let bare: i64 = value.parse().map_err(|_| {
            format!(
                "invalid auto_shake threshold {value:?}: expected \"off\", \"inherit\", an integer percent, a percent string like \"40%\", or an absolute token count like \"160k\""
            )
        })?;
        Self::from_bare_int(bare, value)
    }

    /// Disambiguates a bare integer (from a plain TOML integer, or a bare
    /// numeric string with no `%`/`k` suffix): 1..=100 is a percent, above
    /// 100 is an absolute token count. 0 and negative values are rejected.
    fn from_bare_int(value: i64, original: impl std::fmt::Display) -> Result<Self, String> {
        if value <= 100 {
            Self::checked_percent(value, original)
        } else {
            Self::checked_tokens(value, original)
        }
    }

    fn checked_percent(value: i64, original: impl std::fmt::Display) -> Result<Self, String> {
        if value <= 0 {
            return Err(format!(
                "invalid auto_shake threshold \"{original}\": percent must be positive"
            ));
        }
        Ok(Self::Percent(value))
    }

    fn checked_tokens(value: i64, original: impl std::fmt::Display) -> Result<Self, String> {
        if value <= 0 {
            return Err(format!(
                "invalid auto_shake threshold \"{original}\": absolute token count must be positive"
            ));
        }
        Ok(Self::Tokens(value))
    }
}

impl<'de> Deserialize<'de> for AutoShakeThresholdToml {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(i64),
            Str(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Int(value) => {
                AutoShakeThresholdToml::from_bare_int(value, value).map_err(SerdeError::custom)
            }
            Raw::Str(value) => {
                AutoShakeThresholdToml::parse_str(&value).map_err(SerdeError::custom)
            }
        }
    }
}

impl Serialize for AutoShakeThresholdToml {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Off => serializer.serialize_str("off"),
            Self::Inherit => serializer.serialize_str("inherit"),
            Self::Percent(value) => serializer.serialize_i64(*value),
            Self::Tokens(value) => serializer.serialize_i64(*value),
        }
    }
}

impl JsonSchema for AutoShakeThresholdToml {
    fn schema_name() -> String {
        "AutoShakeThreshold".to_string()
    }

    fn json_schema(_generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::InstanceType;
        use schemars::schema::Metadata;
        use schemars::schema::Schema;
        use schemars::schema::SchemaObject;
        Schema::Object(SchemaObject {
            instance_type: Some(vec![InstanceType::Integer, InstanceType::String].into()),
            metadata: Some(Box::new(Metadata {
                description: Some(
                    "\"off\", \"inherit\" (per-family entries only), an integer percent 1-100 (e.g. 40), a percent string (e.g. \"40%\"), or an absolute token count above 100 (e.g. 160000 or \"160k\"). A bare integer 1-100 is a percent; above 100 it is a token count."
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ProjectConfig {
    pub trust_level: Option<TrustLevel>,
}

impl ProjectConfig {
    pub fn is_trusted(&self) -> bool {
        matches!(self.trust_level, Some(TrustLevel::Trusted))
    }

    pub fn is_untrusted(&self) -> bool {
        matches!(self.trust_level, Some(TrustLevel::Untrusted))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RealtimeAudioConfig {
    pub microphone: Option<String>,
    pub speaker: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeWsMode {
    #[default]
    Conversational,
    Transcription,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeTransport {
    #[default]
    #[serde(rename = "webrtc")]
    WebRtc,
    Websocket,
}

pub use codex_protocol::protocol::RealtimeConversationVersion as RealtimeWsVersion;
pub use codex_protocol::protocol::RealtimeVoice;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RealtimeConfig {
    pub version: RealtimeWsVersion,
    #[serde(rename = "type")]
    pub session_type: RealtimeWsMode,
    pub transport: RealtimeTransport,
    pub voice: Option<RealtimeVoice>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RealtimeToml {
    pub version: Option<RealtimeWsVersion>,
    #[serde(rename = "type")]
    pub session_type: Option<RealtimeWsMode>,
    pub transport: Option<RealtimeTransport>,
    pub voice: Option<RealtimeVoice>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RealtimeAudioToml {
    pub microphone: Option<String>,
    pub speaker: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolsToml {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_web_search_tool_config"
    )]
    pub web_search: Option<WebSearchToolConfig>,
    pub experimental_request_user_input: Option<ExperimentalRequestUserInput>,
    pub update_plan: Option<UpdatePlanToolConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExperimentalRequestUserInput {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct UpdatePlanToolConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WebSearchToolConfigInput {
    Enabled(bool),
    Config(WebSearchToolConfig),
}

fn deserialize_optional_web_search_tool_config<'de, D>(
    deserializer: D,
) -> Result<Option<WebSearchToolConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<WebSearchToolConfigInput>::deserialize(deserializer)?;

    Ok(match value {
        None => None,
        Some(WebSearchToolConfigInput::Enabled(enabled)) => {
            let _ = enabled;
            None
        }
        Some(WebSearchToolConfigInput::Config(config)) => Some(config),
    })
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GoalsToml {
    /// Maximum token budget allowed for a goal and default budget for new goals.
    pub max_goal_token_budget: Option<NonZeroU64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentsToml {
    /// Whether multi-agent tools are enabled. Defaults to true.
    /// An enabled `features.multi_agent_v2` setting takes precedence.
    pub enabled: Option<bool>,
    /// Maximum number of spawned agent threads that can be open concurrently per session.
    /// When unset, the selected multi-agent backend uses its default.
    #[serde(alias = "max_threads")]
    #[schemars(range(min = 1))]
    pub max_concurrent_threads_per_session: Option<usize>,
    /// Maximum nesting depth for V1 agent threads. Ignored by V2.
    pub max_depth: Option<i32>,
    /// Default model for spawned subagents when the spawn call does not select one.
    pub default_subagent_model: Option<String>,
    /// Default reasoning effort for spawned subagents when the spawn call does not select one.
    pub default_subagent_reasoning_effort: Option<ReasoningEffort>,
    /// Removed agent-job setting retained as a no-op for compatibility.
    #[schemars(skip)]
    pub job_max_runtime_seconds: Option<u64>,
    /// Whether to record a model-visible message when an agent turn is interrupted.
    /// Defaults to true.
    pub interrupt_message: Option<bool>,

    /// User-defined role declarations keyed by role name.
    ///
    /// Example:
    /// ```toml
    /// [agents.researcher]
    /// description = "Research-focused role."
    /// config_file = "./agents/researcher.toml"
    /// nickname_candidates = ["Herodotus", "Ibn Battuta"]
    /// ```
    #[serde(default, flatten)]
    pub roles: BTreeMap<String, AgentRoleToml>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentRoleToml {
    /// Human-facing role documentation used in spawn tool guidance.
    /// Required unless supplied by the referenced agent role file.
    pub description: Option<String>,

    /// Path to a role-specific config layer.
    /// Relative paths are resolved relative to the `config.toml` that defines them.
    pub config_file: Option<AbsolutePathBuf>,

    /// Candidate nicknames for agents spawned with this role.
    pub nickname_candidates: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GhostSnapshotToml {
    /// Legacy no-op setting retained for compatibility.
    #[serde(alias = "ignore_untracked_files_over_bytes")]
    pub ignore_large_untracked_files: Option<i64>,
    /// Legacy no-op setting retained for compatibility.
    #[serde(alias = "large_untracked_dir_warning_threshold")]
    pub ignore_large_untracked_dirs: Option<i64>,
    /// Legacy no-op setting retained for compatibility.
    pub disable_warnings: Option<bool>,
}

impl ConfigToml {
    /// Derive the effective permission profile from legacy sandbox config.
    ///
    /// Call this only after ruling out `default_permissions`: named
    /// `[permissions]` profiles must be compiled through the permissions
    /// profile pipeline, not reconstructed from `sandbox_mode`.
    pub async fn derive_permission_profile(
        &self,
        sandbox_mode_override: Option<SandboxMode>,
        windows_sandbox_level: WindowsSandboxLevel,
        active_project: Option<&ProjectConfig>,
        permission_profile_constraint: Option<&crate::Constrained<PermissionProfile>>,
    ) -> PermissionProfile {
        let configured_sandbox_mode = sandbox_mode_override.or(self.sandbox_mode);
        let resolved_sandbox_mode = configured_sandbox_mode
            .or_else(|| {
                // If no sandbox_mode is set but this directory has a trust decision,
                // default to workspace-write except on unsandboxed Windows where we
                // default to read-only.
                active_project
                    .filter(|project| project.is_trusted() || project.is_untrusted())
                    .map(|_| {
                        if cfg!(target_os = "windows")
                            && windows_sandbox_level == WindowsSandboxLevel::Disabled
                        {
                            SandboxMode::ReadOnly
                        } else {
                            SandboxMode::WorkspaceWrite
                        }
                    })
            })
            .unwrap_or_default();
        let effective_sandbox_mode = if cfg!(target_os = "windows")
            // If the experimental Windows sandbox is enabled, do not force a downgrade.
            && windows_sandbox_level == WindowsSandboxLevel::Disabled
            && matches!(resolved_sandbox_mode, SandboxMode::WorkspaceWrite)
        {
            SandboxMode::ReadOnly
        } else {
            resolved_sandbox_mode
        };

        let permission_profile = match effective_sandbox_mode {
            SandboxMode::ReadOnly => PermissionProfile::read_only(),
            SandboxMode::WorkspaceWrite => match self.sandbox_workspace_write.as_ref() {
                Some(SandboxWorkspaceWrite {
                    writable_roots,
                    network_access,
                    exclude_tmpdir_env_var,
                    exclude_slash_tmp,
                }) => {
                    let network_policy = if *network_access {
                        NetworkSandboxPolicy::Enabled
                    } else {
                        NetworkSandboxPolicy::Restricted
                    };
                    PermissionProfile::workspace_write_with(
                        writable_roots,
                        network_policy,
                        *exclude_tmpdir_env_var,
                        *exclude_slash_tmp,
                    )
                }
                None => PermissionProfile::workspace_write(),
            },
            SandboxMode::DangerFullAccess => PermissionProfile::Disabled,
        };
        if configured_sandbox_mode.is_none()
            && let Some(constraint) = permission_profile_constraint
            && let Err(err) = constraint.can_set(&permission_profile)
        {
            tracing::warn!(
                error = %err,
                "default sandbox policy is disallowed by requirements; falling back to required default"
            );
            PermissionProfile::read_only()
        } else {
            permission_profile
        }
    }

    /// Resolves the cwd to an existing project, or returns None if ConfigToml
    /// does not contain a project corresponding to cwd or the resolved git repo
    /// root for cwd.
    pub fn get_active_project(
        &self,
        resolved_cwd: &Path,
        repo_root: Option<&Path>,
    ) -> Option<ProjectConfig> {
        let projects = self.projects.as_ref()?;

        for normalized_cwd in normalized_project_lookup_keys(resolved_cwd) {
            if let Some(project_config) = project_config_for_lookup_key(projects, &normalized_cwd) {
                return Some(project_config);
            }
        }

        if let Some(repo_root) = repo_root {
            for normalized_repo_root in normalized_project_lookup_keys(repo_root) {
                if let Some(project_config_for_root) =
                    project_config_for_lookup_key(projects, &normalized_repo_root)
                {
                    return Some(project_config_for_root);
                }
            }
        }

        None
    }
}

/// Canonicalize the path and convert it to a string to be used as a key in the
/// projects trust map. On Windows, strips UNC, when possible, to try to ensure
/// that different paths that point to the same location have the same key.
fn normalized_project_lookup_keys(path: &Path) -> Vec<String> {
    let normalized_path = normalize_project_lookup_key(path.to_string_lossy().to_string());
    let normalized_canonical_path = normalize_project_lookup_key(
        normalize_for_path_comparison(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_string(),
    );
    if normalized_path == normalized_canonical_path {
        vec![normalized_canonical_path]
    } else {
        vec![normalized_canonical_path, normalized_path]
    }
}

fn normalize_project_lookup_key(key: String) -> String {
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn project_config_for_lookup_key(
    projects: &HashMap<String, ProjectConfig>,
    lookup_key: &str,
) -> Option<ProjectConfig> {
    if let Some(project_config) = projects.get(lookup_key) {
        return Some(project_config.clone());
    }

    let mut normalized_matches: Vec<_> = projects
        .iter()
        .filter(|(key, _)| normalize_project_lookup_key((*key).clone()) == lookup_key)
        .collect();
    normalized_matches.sort_by_key(|(key, _)| *key);
    normalized_matches
        .first()
        .map(|(_, project_config)| (**project_config).clone())
}

pub fn validate_reserved_model_provider_ids(
    model_providers: &HashMap<String, ModelProviderInfo>,
) -> Result<(), String> {
    let mut conflicts = model_providers
        .keys()
        .filter(|key| {
            !matches!(
                key.as_str(),
                AMAZON_BEDROCK_PROVIDER_ID | AMAZON_BEDROCK_RUNTIME_PROVIDER_ID
            ) && RESERVED_MODEL_PROVIDER_IDS.contains(&key.as_str())
        })
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>();
    conflicts.sort_unstable();
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "model_providers contains reserved built-in provider IDs: {}. \
Built-in providers cannot be overridden. Rename your custom provider (for example, `openai-custom`).",
            conflicts.join(", ")
        ))
    }
}

pub fn validate_model_providers(
    model_providers: &HashMap<String, ModelProviderInfo>,
) -> Result<(), String> {
    validate_reserved_model_provider_ids(model_providers)?;
    for (key, provider) in model_providers {
        if !matches!(
            key.as_str(),
            AMAZON_BEDROCK_PROVIDER_ID | AMAZON_BEDROCK_RUNTIME_PROVIDER_ID
        ) {
            if provider.aws.is_some() {
                return Err(format!(
                    "model_providers.{key}: provider aws is only supported for \
`{AMAZON_BEDROCK_PROVIDER_ID}` or `{AMAZON_BEDROCK_RUNTIME_PROVIDER_ID}`"
                ));
            }
            if provider.name.trim().is_empty() {
                return Err(format!(
                    "model_providers.{key}: provider name must not be empty"
                ));
            }
        }
        provider
            .validate()
            .map_err(|message| format!("model_providers.{key}: {message}"))?;
    }
    Ok(())
}

fn deserialize_model_providers<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, ModelProviderInfo>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let model_providers = HashMap::<String, ModelProviderInfo>::deserialize(deserializer)?;
    validate_model_providers(&model_providers).map_err(serde::de::Error::custom)?;
    Ok(model_providers)
}

#[cfg(test)]
#[path = "bedrock_runtime_tests.rs"]
mod bedrock_runtime_tests;

pub fn validate_oss_provider(provider: &str) -> std::io::Result<()> {
    match provider {
        LMSTUDIO_OSS_PROVIDER_ID | OLLAMA_OSS_PROVIDER_ID => Ok(()),
        LEGACY_OLLAMA_CHAT_PROVIDER_ID => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            OLLAMA_CHAT_PROVIDER_REMOVED_ERROR,
        )),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Invalid OSS provider '{provider}'. Must be one of: {LMSTUDIO_OSS_PROVIDER_ID}, {OLLAMA_OSS_PROVIDER_ID}"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const WORKSPACE_ID_A: &str = "123e4567-e89b-42d3-a456-426614174000";
    const WORKSPACE_ID_B: &str = "123e4567-e89b-42d3-a456-426614174001";

    #[test]
    fn thread_unload_delay_requires_nonnegative_seconds() {
        for value in ["-1", "1.5", "\"60\""] {
            let error =
                toml::from_str::<ConfigToml>(&format!("thread_unload_delay_secs = {value}"))
                    .expect_err("idle timeout must be a nonnegative integer");
            assert!(error.to_string().contains("thread_unload_delay_secs"));
        }
    }

    #[test]
    fn forced_chatgpt_workspace_id_accepts_single_string() {
        let config: ConfigToml = toml::from_str(&format!(
            r#"forced_chatgpt_workspace_id = "{WORKSPACE_ID_A}""#
        ))
        .expect("single workspace id should deserialize");

        assert_eq!(
            config
                .forced_chatgpt_workspace_id
                .expect("workspace id should be set")
                .into_vec(),
            vec![WORKSPACE_ID_A.to_string()]
        );
    }

    #[test]
    fn forced_chatgpt_workspace_id_accepts_string_list() {
        let config: ConfigToml = toml::from_str(&format!(
            r#"forced_chatgpt_workspace_id = ["{WORKSPACE_ID_A}", "{WORKSPACE_ID_B}"]"#
        ))
        .expect("workspace id list should deserialize");

        assert_eq!(
            config
                .forced_chatgpt_workspace_id
                .expect("workspace ids should be set")
                .into_vec(),
            vec![WORKSPACE_ID_A.to_string(), WORKSPACE_ID_B.to_string()]
        );
    }

    #[test]
    fn forced_chatgpt_workspace_id_rejects_comma_separated_string() {
        let err = toml::from_str::<ConfigToml>(&format!(
            r#"forced_chatgpt_workspace_id = "{WORKSPACE_ID_A},{WORKSPACE_ID_B}""#
        ))
        .expect_err("comma-separated string should be rejected");

        let message = err.to_string();
        assert!(message.contains("TOML list of strings"));
        assert!(message.contains("comma-separated strings are not supported"));
    }

    #[test]
    fn amazon_bedrock_auth_command_must_not_be_empty() {
        let err = toml::from_str::<ConfigToml>(
            r#"
[model_providers.amazon-bedrock.auth]
command = "   "
"#,
        )
        .expect_err("empty Amazon Bedrock auth command should be rejected");

        assert!(
            err.to_string().contains(
                "model_providers.amazon-bedrock: provider auth.command must not be empty"
            )
        );
    }

    #[test]
    fn auto_shake_threshold_accepts_every_documented_spelling() {
        let cases: &[(&str, AutoShakeThresholdToml)] = &[
            ("\"off\"", AutoShakeThresholdToml::Off),
            ("\"inherit\"", AutoShakeThresholdToml::Inherit),
            ("40", AutoShakeThresholdToml::Percent(40)),
            ("\"40\"", AutoShakeThresholdToml::Percent(40)),
            ("\"40%\"", AutoShakeThresholdToml::Percent(40)),
            ("100", AutoShakeThresholdToml::Percent(100)),
            ("\"100%\"", AutoShakeThresholdToml::Percent(100)),
            ("160000", AutoShakeThresholdToml::Tokens(160_000)),
            ("\"160000\"", AutoShakeThresholdToml::Tokens(160_000)),
            ("\"160k\"", AutoShakeThresholdToml::Tokens(160_000)),
            ("\"160K\"", AutoShakeThresholdToml::Tokens(160_000)),
            ("101", AutoShakeThresholdToml::Tokens(101)),
            ("\"200000\"", AutoShakeThresholdToml::Tokens(200_000)),
        ];
        for (raw, expected) in cases {
            let parsed: AutoShakeThresholdToml = toml::from_str(&format!("threshold = {raw}\n"))
                .map(|wrapper: ThresholdWrapper| wrapper.threshold)
                .unwrap_or_else(|error| panic!("failed to parse {raw}: {error}"));
            assert_eq!(parsed, *expected, "input {raw}");
        }
    }

    #[test]
    fn auto_shake_threshold_rejects_zero_and_negative_values() {
        for raw in [
            "0", "-1", "\"0\"", "\"-5\"", "\"0%\"", "\"-10%\"", "\"0k\"", "\"-1k\"",
        ] {
            let error = toml::from_str::<ThresholdWrapper>(&format!("threshold = {raw}\n"))
                .expect_err(&format!("{raw} should be rejected"));
            let message = error.to_string();
            assert!(
                message.contains("positive") || message.contains("invalid auto_shake threshold"),
                "unexpected error for {raw}: {message}"
            );
        }
    }

    #[test]
    fn auto_shake_threshold_rejects_garbage_strings() {
        for raw in ["\"banana\"", "\"40x\"", "\"\""] {
            let error = toml::from_str::<ThresholdWrapper>(&format!("threshold = {raw}\n"))
                .expect_err(&format!("{raw} should be rejected"));
            assert!(
                error.to_string().contains("invalid auto_shake threshold"),
                "{error}"
            );
        }
    }

    /// Minimal wrapper so `AutoShakeThresholdToml`'s custom `Deserialize` can
    /// be exercised directly against raw TOML values (ints and strings)
    /// without going through the full `AutoShakeToml` struct.
    #[derive(Debug, Deserialize)]
    struct ThresholdWrapper {
        threshold: AutoShakeThresholdToml,
    }
}
