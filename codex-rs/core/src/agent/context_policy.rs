//! Resolution and inheritance for per-agent context-reduction policies.

use crate::config::Config;
use codex_config::config_toml::SubagentContextReductionOnFailure;
use codex_config::config_toml::SubagentContextReductionShake;
use codex_protocol::protocol::SubagentContextFailurePolicy;
use codex_protocol::protocol::SubagentContextReductionOverrides;
use codex_protocol::protocol::SubagentContextReductionPolicyState;
use codex_protocol::protocol::SubagentContextShakePolicy;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyProvenance {
    Config,
    Inherited,
    Local,
}

impl PolicyProvenance {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Inherited => "inherited",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedSubagentContextPolicy {
    pub(crate) enabled: bool,
    pub(crate) threshold_tokens: i64,
    pub(crate) check_after_tools: bool,
    pub(crate) shake: SubagentContextReductionShake,
    pub(crate) on_failure: SubagentContextReductionOnFailure,
    pub(crate) enabled_from: PolicyProvenance,
    pub(crate) threshold_tokens_from: PolicyProvenance,
    pub(crate) check_after_tools_from: PolicyProvenance,
    pub(crate) shake_from: PolicyProvenance,
    pub(crate) on_failure_from: PolicyProvenance,
}

/// Snake-case JSON input shared by spawn and policy-update tools.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextReductionPolicyArgs {
    pub(crate) enabled: Option<bool>,
    pub(crate) threshold_tokens: Option<i64>,
    pub(crate) check_after_tools: Option<bool>,
    pub(crate) shake: Option<SubagentContextShakePolicy>,
    pub(crate) on_failure: Option<SubagentContextFailurePolicy>,
}

impl ContextReductionPolicyArgs {
    pub(crate) fn into_overrides(self) -> SubagentContextReductionOverrides {
        SubagentContextReductionOverrides {
            enabled: self.enabled,
            threshold_tokens: self.threshold_tokens,
            check_after_tools: self.check_after_tools,
            shake: self.shake,
            on_failure: self.on_failure,
        }
    }
}

/// Produces the child's inherited layer from this parent's current settings.
pub(crate) fn inherited_by_children(
    state: &SubagentContextReductionPolicyState,
) -> SubagentContextReductionOverrides {
    let mut inherited = state.inherited.clone();
    merge_overrides(&mut inherited, state.inheritable.clone());
    inherited
}

/// Applies a partial local update and records whether it should reach new descendants.
pub(crate) fn update_local(
    state: &mut SubagentContextReductionPolicyState,
    patch: SubagentContextReductionOverrides,
    inherit_to_children: bool,
) {
    merge_overrides(&mut state.local, patch.clone());
    if inherit_to_children {
        merge_overrides(&mut state.inheritable, patch);
    } else {
        clear_overridden_fields(&mut state.inheritable, &patch);
    }
    state.desired_revision = state.desired_revision.saturating_add(1);
}

/// Clears this agent's explicit settings while retaining its inherited baseline.
pub(crate) fn reset_local(state: &mut SubagentContextReductionPolicyState) {
    state.local = SubagentContextReductionOverrides::default();
    state.inheritable = SubagentContextReductionOverrides::default();
    state.desired_revision = state.desired_revision.saturating_add(1);
}

/// Creates the policy state for a new child from inherited and spawn overrides.
pub(crate) fn for_child(
    parent: &SubagentContextReductionPolicyState,
    patch: SubagentContextReductionOverrides,
    inherit_to_children: bool,
) -> SubagentContextReductionPolicyState {
    let mut child = SubagentContextReductionPolicyState {
        inherited: inherited_by_children(parent),
        ..SubagentContextReductionPolicyState::default()
    };
    update_local(&mut child, patch, inherit_to_children);
    child
}

/// Resolves config defaults, ancestor overrides, and this thread's local overrides.
pub(crate) fn resolve(
    state: &SubagentContextReductionPolicyState,
    config: &Config,
) -> ResolvedSubagentContextPolicy {
    let defaults = &config.subagent_context_reduction;
    let (enabled, enabled_from) = resolve_field(
        state.local.enabled,
        state.inherited.enabled,
        defaults.enabled,
    );
    let (threshold_tokens, threshold_tokens_from) = resolve_field(
        state.local.threshold_tokens,
        state.inherited.threshold_tokens,
        defaults.threshold_tokens,
    );
    let (check_after_tools, check_after_tools_from) = resolve_field(
        state.local.check_after_tools,
        state.inherited.check_after_tools,
        defaults.check_after_tools,
    );
    let (shake, shake_from) = resolve_field(
        state.local.shake.map(shake_from_protocol),
        state.inherited.shake.map(shake_from_protocol),
        defaults.shake,
    );
    let (on_failure, on_failure_from) = resolve_field(
        state.local.on_failure.map(failure_from_protocol),
        state.inherited.on_failure.map(failure_from_protocol),
        defaults.on_failure,
    );
    ResolvedSubagentContextPolicy {
        enabled,
        threshold_tokens,
        check_after_tools,
        shake,
        on_failure,
        enabled_from,
        threshold_tokens_from,
        check_after_tools_from,
        shake_from,
        on_failure_from,
    }
}

pub(crate) fn validate_overrides(
    overrides: &SubagentContextReductionOverrides,
) -> Result<(), String> {
    if overrides.threshold_tokens.is_some_and(|tokens| tokens <= 0) {
        return Err("subagent context threshold_tokens must be positive".to_string());
    }
    Ok(())
}

pub(crate) fn validate_state(state: &SubagentContextReductionPolicyState) -> Result<(), String> {
    validate_overrides(&state.inherited)?;
    validate_overrides(&state.local)?;
    validate_overrides(&state.inheritable)
}

pub(crate) fn should_report_failure_episode(
    reported_revision: Option<u64>,
    current_revision: u64,
) -> bool {
    reported_revision != Some(current_revision)
}

fn resolve_field<T: Copy>(
    local: Option<T>,
    inherited: Option<T>,
    configured: T,
) -> (T, PolicyProvenance) {
    if let Some(value) = local {
        (value, PolicyProvenance::Local)
    } else if let Some(value) = inherited {
        (value, PolicyProvenance::Inherited)
    } else {
        (configured, PolicyProvenance::Config)
    }
}

fn merge_overrides(
    target: &mut SubagentContextReductionOverrides,
    next: SubagentContextReductionOverrides,
) {
    if next.enabled.is_some() {
        target.enabled = next.enabled;
    }
    if next.threshold_tokens.is_some() {
        target.threshold_tokens = next.threshold_tokens;
    }
    if next.check_after_tools.is_some() {
        target.check_after_tools = next.check_after_tools;
    }
    if next.shake.is_some() {
        target.shake = next.shake;
    }
    if next.on_failure.is_some() {
        target.on_failure = next.on_failure;
    }
}

fn clear_overridden_fields(
    target: &mut SubagentContextReductionOverrides,
    patch: &SubagentContextReductionOverrides,
) {
    if patch.enabled.is_some() {
        target.enabled = None;
    }
    if patch.threshold_tokens.is_some() {
        target.threshold_tokens = None;
    }
    if patch.check_after_tools.is_some() {
        target.check_after_tools = None;
    }
    if patch.shake.is_some() {
        target.shake = None;
    }
    if patch.on_failure.is_some() {
        target.on_failure = None;
    }
}

fn shake_from_protocol(value: SubagentContextShakePolicy) -> SubagentContextReductionShake {
    match value {
        SubagentContextShakePolicy::Inherit => SubagentContextReductionShake::Inherit,
        SubagentContextShakePolicy::On => SubagentContextReductionShake::On,
        SubagentContextShakePolicy::Off => SubagentContextReductionShake::Off,
    }
}

fn failure_from_protocol(value: SubagentContextFailurePolicy) -> SubagentContextReductionOnFailure {
    match value {
        SubagentContextFailurePolicy::Stop => SubagentContextReductionOnFailure::Stop,
        SubagentContextFailurePolicy::Continue => SubagentContextReductionOnFailure::Continue,
    }
}

#[cfg(test)]
#[path = "context_policy_tests.rs"]
mod tests;
