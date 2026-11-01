//! Rebases a child agent onto a subdirectory of its parent's working directory.
//!
//! The child runs as if Codex had started in that directory: project `.codex` layers between the
//! directory and its project root replace the parent's, and the settings those new layers set win
//! over the parent's. Settings they leave unset keep the parent's values. The parent's approval
//! policy and permission profile remain a ceiling the directory can tighten but never loosen.

use crate::config::Config;
use crate::config::PermissionProfileSnapshot;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
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
    /// Normalized path relative to the parent's cwd, never empty.
    pub(crate) relative: PathBuf,
    /// `relative` joined onto the parent's cwd.
    pub(crate) cwd: AbsolutePathBuf,
    /// The parent's cwd that `relative` is resolved against.
    pub(crate) parent_cwd: AbsolutePathBuf,
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
        let cwd = parent_cwd.join(&relative);
        let metadata = std::fs::metadata(cwd.as_path()).map_err(|err| {
            format!(
                "cwd `{shown}` cannot be used: {} ({err})",
                cwd.as_path().display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "cwd `{shown}` is not a directory: {}",
                cwd.as_path().display()
            ));
        }
        let canonical_parent = dunce::canonicalize(parent_cwd.as_path())
            .map_err(|err| format!("cwd `{shown}` cannot be checked: {err}"))?;
        let canonical_child = dunce::canonicalize(cwd.as_path())
            .map_err(|err| format!("cwd `{shown}` cannot be checked: {err}"))?;
        if !canonical_child.starts_with(&canonical_parent) {
            return Err(format!(
                "cwd `{shown}` resolves outside the parent agent's working directory"
            ));
        }
        Ok(Some(Self {
            relative,
            cwd,
            parent_cwd: parent_cwd.clone(),
        }))
    }

    /// Rebases an environment cwd that equals the parent's cwd onto this directory.
    pub(crate) fn rebase_environment_cwd(&self, environment_cwd: &PathUri) -> Option<PathUri> {
        if *environment_cwd != PathUri::from_abs_path(&self.parent_cwd) {
            return None;
        }
        let relative = self.relative.to_str()?;
        environment_cwd.join_descendant(relative).ok()
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
    pub(crate) async fn load(base: &Config, directory: ChildDirectory) -> Result<Self, String> {
        let shown = directory.relative.display().to_string();
        let project_layers = load_project_layers_for_cwd(
            LOCAL_FS.as_ref(),
            base.codex_home.as_path(),
            &base.config_layer_stack,
            &directory.cwd,
        )
        .await
        .map_err(|err| format!("cwd `{shown}` config could not be loaded: {err}"))?;
        let parent_folders = base
            .config_layer_stack
            .all_layers_low_to_high()
            .filter_map(|layer| match &layer.name {
                ConfigLayerSource::Project { dot_codex_folder } => Some(dot_codex_folder.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut directory_toml = TomlValue::Table(Default::default());
        let mut warnings = project_layers.startup_warnings;
        for layer in &project_layers.layers {
            let ConfigLayerSource::Project { dot_codex_folder } = &layer.name else {
                continue;
            };
            if parent_folders.contains(dot_codex_folder) {
                continue;
            }
            match &layer.disabled_reason {
                Some(reason) => warnings.push(format!(
                    "Project config in {} was not loaded for cwd `{shown}`: {reason}",
                    dot_codex_folder.as_path().display()
                )),
                None => merge_toml_values(&mut directory_toml, &layer.config),
            }
        }
        let config_layer_stack = base
            .config_layer_stack
            .with_project_layers(project_layers.layers)
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
        if self.sets("mcp_servers") {
            config.mcp_servers = directory.mcp_servers.clone();
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

    pub(crate) fn into_warnings(self) -> Vec<String> {
        self.warnings
    }
}

/// Orders approval policies by how much user oversight they keep.
fn approval_oversight(policy: AskForApproval) -> u8 {
    match policy {
        AskForApproval::Never => 0,
        AskForApproval::Granular(_) => 1,
        AskForApproval::OnRequest => 2,
        AskForApproval::UnlessTrusted => 3,
    }
}

fn approval_is_at_least_as_strict(requested: AskForApproval, parent: AskForApproval) -> bool {
    match (requested, parent) {
        (AskForApproval::Granular(requested), AskForApproval::Granular(parent)) => {
            requested == parent
        }
        _ => approval_oversight(requested) >= approval_oversight(parent),
    }
}

#[cfg(test)]
#[path = "child_directory_tests.rs"]
mod tests;
