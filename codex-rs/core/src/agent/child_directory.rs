//! Rebases a child agent onto a subdirectory of its parent's working directory.
//!
//! The child runs as if Codex had started in that directory: project `.codex` layers between the
//! directory and its project root are added above the parent's, and the settings those new layers
//! set win over the parent's. Settings they leave unset keep the parent's values.
//!
//! The parent's authority is a ceiling the directory can tighten but never loosen:
//! - approval policy and permission profile are clamped to the parent's;
//! - command rules start from the parent's live rules, and the directory adds only `prompt` and
//!   `forbidden` rules (see [`ChildDirectory::rule_folders`]);
//! - the directory's MCP servers run, but their tools cannot be pre-approved, so every call goes
//!   through the child's clamped approval policy;
//! - the directory's hooks follow the normal per-hook trust review.

use crate::config::Config;
use crate::config::PermissionProfileSnapshot;
use codex_config::AppToolApproval;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::McpServerConfig;
use codex_config::loader::load_project_layers_for_cwd;
use codex_config::merge_toml_values;
use codex_exec_server::LOCAL_FS;
use codex_features::Feature;
use codex_features::feature_for_key;
use codex_protocol::intersect_effective_permission_profiles;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use toml::Value as TomlValue;

/// Keys whose presence in the directory's own layers selects its permission profile.
const PERMISSION_KEYS: &[&str] = &[
    "sandbox_mode",
    "sandbox_workspace_write",
    "default_permissions",
    "permissions",
];

/// Capability features a directory may switch off; like roles, it cannot switch any on.
const DIRECTORY_DISABLEABLE_FEATURES: &[Feature] = &[
    Feature::ShellTool,
    Feature::Apps,
    Feature::Plugins,
    Feature::MemoryTool,
    Feature::RequestPermissionsTool,
];

/// A validated child working directory below the parent's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildDirectory {
    /// Path of `cwd` relative to the parent's canonical cwd, never empty.
    pub(crate) relative: PathBuf,
    /// Canonical child cwd, so a symlink repointed after validation cannot move the child.
    pub(crate) cwd: AbsolutePathBuf,
    /// The parent's cwd as the parent sees it.
    pub(crate) parent_cwd: AbsolutePathBuf,
    /// Canonical form of `parent_cwd`.
    canonical_parent_cwd: PathBuf,
    /// `.codex` folders the directory added; only their `prompt` and `forbidden` rules apply.
    pub(crate) rule_folders: Vec<AbsolutePathBuf>,
}

impl ChildDirectory {
    /// Validates `requested` against `parent_cwd`.
    ///
    /// Returns `Ok(None)` when the request names the parent's own directory, so the child keeps
    /// today's configuration.
    pub(crate) fn resolve(
        parent_cwd: &AbsolutePathBuf,
        requested: &Path,
    ) -> Result<Option<Self>, String> {
        let shown = requested.display();
        let outside = || {
            format!(
                "cwd `{shown}` must be a relative path inside the parent agent's working directory"
            )
        };
        let mut relative = PathBuf::new();
        for component in requested.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => relative.push(part),
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(outside());
                }
            }
        }
        if relative.as_os_str().is_empty() {
            return Ok(None);
        }
        let joined = parent_cwd.join(&relative);
        let metadata = std::fs::metadata(joined.as_path()).map_err(|err| {
            format!(
                "cwd `{shown}` cannot be used: {} ({err})",
                joined.as_path().display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "cwd `{shown}` is not a directory: {}",
                joined.as_path().display()
            ));
        }
        let canonical_parent_cwd = dunce::canonicalize(parent_cwd.as_path())
            .map_err(|err| format!("cwd `{shown}` cannot be checked: {err}"))?;
        let canonical_child = dunce::canonicalize(joined.as_path())
            .map_err(|err| format!("cwd `{shown}` cannot be checked: {err}"))?;
        let Ok(relative) = canonical_child.strip_prefix(&canonical_parent_cwd) else {
            return Err(format!(
                "cwd `{shown}` resolves outside the parent agent's working directory"
            ));
        };
        if relative.as_os_str().is_empty() {
            return Ok(None);
        }
        let relative = relative.to_path_buf();
        let cwd = AbsolutePathBuf::from_absolute_path(&canonical_child)
            .map_err(|err| format!("cwd `{shown}` cannot be checked: {err}"))?;
        Ok(Some(Self {
            relative,
            cwd,
            parent_cwd: parent_cwd.clone(),
            canonical_parent_cwd,
            rule_folders: Vec::new(),
        }))
    }

    /// Recovers the directory of a child whose recorded cwd lies below `parent_cwd`.
    ///
    /// The child's thread records its cwd, so resume and cold reload restore the directory the
    /// child actually ran in, whichever role or spawn argument chose it.
    pub(crate) fn from_thread_cwd(
        parent_cwd: &AbsolutePathBuf,
        thread_cwd: &Path,
    ) -> Result<Option<Self>, String> {
        let (Ok(canonical_parent), Ok(canonical_thread)) = (
            dunce::canonicalize(parent_cwd.as_path()),
            dunce::canonicalize(thread_cwd),
        ) else {
            return Ok(None);
        };
        match canonical_thread.strip_prefix(&canonical_parent) {
            Ok(relative) if !relative.as_os_str().is_empty() => Self::resolve(parent_cwd, relative),
            _ => Ok(None),
        }
    }

    /// Moves an environment cwd at the parent's cwd into this directory.
    ///
    /// Paths are compared canonically, so a parent whose environment spells its cwd differently
    /// (a symlink or trailing component) still moves. Other environments stay where they are.
    pub(crate) fn rebase_environment_cwd(&self, environment_cwd: &PathUri) -> Option<PathUri> {
        let at_parent = *environment_cwd == PathUri::from_abs_path(&self.parent_cwd)
            || environment_cwd
                .to_abs_path()
                .ok()
                .and_then(|path| dunce::canonicalize(path.as_path()).ok())
                .is_some_and(|path| path == self.canonical_parent_cwd);
        at_parent.then(|| PathUri::from_abs_path(&self.cwd))
    }
}

/// Loads directory settings for a child of an agent whose cwd is `parent_cwd`.
///
/// `requested` is the explicit spawn cwd, or the role's when the spawn does not name one.
/// Returns `Ok(None)` when no directory applies, leaving the child config unchanged.
pub(crate) async fn load_child_directory_config(
    base: &Config,
    parent_cwd: &AbsolutePathBuf,
    requested: Option<&Path>,
) -> Result<Option<ChildDirectoryConfig>, String> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    match ChildDirectory::resolve(parent_cwd, requested)? {
        Some(directory) => ChildDirectoryConfig::load(base, directory).await.map(Some),
        None => Ok(None),
    }
}

/// Settings loaded for a child directory, ready to layer onto a parent-derived child config.
pub(crate) struct ChildDirectoryConfig {
    directory: ChildDirectory,
    /// The parent's stack with its project layers replaced by the directory's.
    config_layer_stack: ConfigLayerStack,
    /// The directory's materialized config; settings are copied from it selectively.
    materialized: Config,
    /// Merged enabled layers that the parent's stack did not already contain.
    directory_toml: TomlValue,
    warnings: Vec<String>,
}

impl ChildDirectoryConfig {
    /// Loads the project layers for `directory` using the trust inputs of `base`'s stack.
    ///
    /// The parent's project layers always stay underneath, even when the directory has its own
    /// project-root marker such as a submodule's `.git`.
    pub(crate) async fn load(base: &Config, mut directory: ChildDirectory) -> Result<Self, String> {
        let shown = directory.relative.display().to_string();
        let discovered = load_project_layers_for_cwd(
            LOCAL_FS.as_ref(),
            base.codex_home.as_path(),
            &base.config_layer_stack,
            &directory.cwd,
        )
        .await
        .map_err(|err| format!("cwd `{shown}` config could not be loaded: {err}"))?;
        let mut project_layers = base
            .config_layer_stack
            .all_layers_low_to_high()
            .filter(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
            .cloned()
            .collect::<Vec<_>>();
        let mut directory_toml = TomlValue::Table(Default::default());
        let mut warnings = discovered.startup_warnings;
        for layer in discovered.layers {
            if project_layers
                .iter()
                .any(|existing| existing.name == layer.name)
            {
                continue;
            }
            let ConfigLayerSource::Project { dot_codex_folder } = &layer.name else {
                continue;
            };
            match &layer.disabled_reason {
                Some(reason) => warnings.push(format!(
                    "Project config in {} was not loaded for cwd `{shown}`: {reason}",
                    dot_codex_folder.as_path().display()
                )),
                None => {
                    merge_toml_values(&mut directory_toml, &layer.config);
                    directory.rule_folders.push(dot_codex_folder.clone());
                }
            }
            project_layers.push(layer);
        }
        let config_layer_stack = base
            .config_layer_stack
            .with_project_layers(project_layers)
            .map_err(|err| format!("cwd `{shown}` config is invalid: {err}"))?;
        let materialized = base
            .load_for_cwd_and_layer_stack(&directory.cwd, config_layer_stack.clone())
            .await
            .map_err(|err| format!("cwd `{shown}` config is invalid: {err}"))?;
        Ok(Self {
            directory,
            config_layer_stack,
            materialized,
            directory_toml,
            warnings,
        })
    }

    pub(crate) fn directory(&self) -> &ChildDirectory {
        &self.directory
    }

    /// Whether the directory's own layers set top-level `key`.
    pub(crate) fn sets(&self, key: &str) -> bool {
        self.directory_toml
            .as_table()
            .is_some_and(|table| table.contains_key(key))
    }

    /// Applies the directory's settings and layers. Explicit spawn arguments and roles apply after.
    pub(crate) fn apply_settings(&self, config: &mut Config) -> Result<(), String> {
        let directory = &self.materialized;
        config.config_layer_stack = self.config_layer_stack.clone();
        config.cwd = self.directory.cwd.clone();
        if self.sets("model") {
            config.model.clone_from(&directory.model);
        }
        if self.sets("model_reasoning_effort") {
            config
                .model_reasoning_effort
                .clone_from(&directory.model_reasoning_effort);
        }
        if self.sets("model_reasoning_summary") {
            config.model_reasoning_summary = directory.model_reasoning_summary;
        }
        if self.sets("model_verbosity") {
            config.model_verbosity = directory.model_verbosity;
        }
        if self.sets("personality") {
            config.personality = directory.personality;
        }
        if self.sets("developer_instructions") {
            config
                .developer_instructions
                .clone_from(&directory.developer_instructions);
        }
        if let Some(directory_servers) = self
            .directory_toml
            .get("mcp_servers")
            .and_then(TomlValue::as_table)
        {
            // The directory's servers run, but none of their tools may skip approval.
            let mut servers = directory.mcp_servers.get().clone();
            for name in directory_servers.keys() {
                if let Some(server) = servers.get_mut(name) {
                    cap_mcp_tool_approval(server);
                }
            }
            config.mcp_servers = directory.mcp_servers.clone();
            config
                .mcp_servers
                .set(servers)
                .map_err(|err| format!("mcp_servers is invalid: {err}"))?;
        }
        if self.sets("project_doc_max_bytes") {
            config.project_doc_max_bytes = directory.project_doc_max_bytes;
        }
        if self.sets("project_doc_fallback_filenames") {
            config
                .project_doc_fallback_filenames
                .clone_from(&directory.project_doc_fallback_filenames);
        }
        if let Some(features) = self
            .directory_toml
            .get("features")
            .and_then(TomlValue::as_table)
        {
            for key in features.keys() {
                if let Some(feature) = feature_for_key(key)
                    && DIRECTORY_DISABLEABLE_FEATURES.contains(&feature)
                    && !directory.features.enabled(feature)
                {
                    config
                        .features
                        .disable(feature)
                        .map_err(|err| format!("features.{key} is invalid: {err}"))?;
                }
            }
        }
        Ok(())
    }

    /// Restores the directory cwd and tightens permissions after the parent's runtime policy
    /// has been copied onto `config`. The directory can only narrow that policy.
    pub(crate) fn apply_runtime(&mut self, config: &mut Config) -> Result<(), String> {
        config.cwd = self.directory.cwd.clone();
        let directory = &self.materialized;
        let shown = self.directory.relative.display().to_string();
        if self.sets("approval_policy") {
            let parent = config.permissions.approval_policy.value();
            let requested = directory.permissions.approval_policy.value();
            if approval_is_at_least_as_strict(requested, parent) {
                config
                    .permissions
                    .approval_policy
                    .set(requested)
                    .map_err(|err| format!("approval_policy is invalid: {err}"))?;
            } else if requested != parent {
                self.warnings.push(format!(
                    "cwd `{shown}` sets approval_policy `{requested}`, which is looser than the parent's `{parent}`; the parent's policy applies"
                ));
            }
        }
        if PERMISSION_KEYS.iter().any(|key| self.sets(key)) {
            let parent = config.permissions.effective_permission_profile();
            let requested = directory.permissions.effective_permission_profile();
            if requested != parent {
                match intersect_effective_permission_profiles(
                    &parent,
                    &requested,
                    self.directory.cwd.as_path(),
                ) {
                    Ok(bounded) if bounded != parent => {
                        if let Err(err) = config
                            .permissions
                            .set_permission_profile_from_session_snapshot(
                                PermissionProfileSnapshot::legacy(bounded),
                            )
                        {
                            self.warnings.push(format!(
                                "cwd `{shown}` permissions could not be applied ({err}); the parent's permissions apply"
                            ));
                        }
                    }
                    Ok(_) => {}
                    Err(err) => self.warnings.push(format!(
                        "cwd `{shown}` permissions cannot be combined with the parent's ({err}); the parent's permissions apply"
                    )),
                }
            }
        }
        Ok(())
    }

    pub(crate) fn push_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    pub(crate) fn into_warnings(self) -> Vec<String> {
        self.warnings
    }
}

/// Keeps a directory's MCP tools from being pre-approved; `auto` defers to the approval policy.
fn cap_mcp_tool_approval(server: &mut McpServerConfig) {
    let cap = |mode: &mut Option<AppToolApproval>| {
        if *mode == Some(AppToolApproval::Approve) {
            *mode = Some(AppToolApproval::Auto);
        }
    };
    cap(&mut server.default_tools_approval_mode);
    for tool in server.tools.values_mut() {
        cap(&mut tool.approval_mode);
    }
}

/// Whether `requested` grants no approval path that `parent` does not.
///
/// Sandboxed commands run without a prompt under `never`, `granular`, and `on-request`, while
/// `untrusted` asks first, so nothing may replace `untrusted`. `never` escalates nothing, so it
/// may replace any of the others. A `granular` child must keep every prompt category its parent
/// auto-rejects rejected, since an allowed prompt can be approved by the reviewer, not a person.
fn approval_is_at_least_as_strict(requested: AskForApproval, parent: AskForApproval) -> bool {
    use AskForApproval::Granular;
    use AskForApproval::Never;
    use AskForApproval::OnRequest;
    use AskForApproval::UnlessTrusted;
    match (requested, parent) {
        _ if requested == parent => true,
        (_, UnlessTrusted) => false,
        (Never, _) => true,
        (_, Never) => false,
        (Granular(requested), Granular(parent)) => {
            (!requested.sandbox_approval || parent.sandbox_approval)
                && (!requested.rules || parent.rules)
                && (!requested.skill_approval || parent.skill_approval)
                && (!requested.request_permissions || parent.request_permissions)
                && (!requested.mcp_elicitations || parent.mcp_elicitations)
        }
        (Granular(_) | UnlessTrusted, OnRequest) => true,
        (OnRequest | UnlessTrusted, Granular(_)) => false,
        (OnRequest, OnRequest) => true,
    }
}

#[cfg(test)]
#[path = "child_directory_tests.rs"]
mod tests;
