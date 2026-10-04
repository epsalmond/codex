//! Covers role composition and attribution for captured model instructions.

use super::*;
use codex_context_fragments::AnnotatedContent;
use codex_context_fragments::RenderedFragment;
use pretty_assertions::assert_eq;

#[test]
fn role_segment_filters_base_and_appends_bundled_guidance() {
    let shared = DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT;
    let wait = DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT;
    let model_override = DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT;
    let expected_body = format!(
        "Role.\n## Work\nContinue.\n{shared}\n{wait}\n\nThere are 2 available concurrency slots, meaning that up to 2 agents can be active at once, including you.\n\n{model_override}"
    );
    for marked in [false, true] {
        let instructions = MultiAgentRoleInstructions::Composed {
            base: "Role.\n## Plan tool\nOmit role checklist guidance.\n## Work\nContinue."
                .to_string(),
            marked,
            omit_update_plan_instructions: true,
            max_concurrency: 2,
            agent_polling_enabled: true,
            expose_model_overrides: true,
            is_root: true,
            subagent_context_token_cap: None,
        };
        let expected_text = if marked {
            format!("<multi_agent_role>{expected_body}</multi_agent_role>")
        } else {
            expected_body.clone()
        };
        assert_eq!(
            instructions.render_fragment(),
            RenderedFragment::new(
                "developer",
                AnnotatedContent::input_text(
                    expected_text,
                    ContentItemKind("multi_agent.role_instructions".to_string()),
                ),
            ),
        );
    }
}

#[test]
fn subagent_guidance_reflects_its_polling_mode() {
    assert!(
        SUBAGENT_WAKE_ON_REPORT_USAGE_HINT_TEXT
            .contains("A child report resumes your assignment in a new turn")
    );
    let shared = DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT;
    let wait = DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT;
    let polling_body = format!(
        "Role.\n{shared}\n{wait}\n\n{SUBAGENT_BLOCKED_USAGE_HINT_TEXT}\n\nThere are 2 available concurrency slots, meaning that up to 2 agents can be active at once, including you."
    );
    let wake_shared = shared.replace("`wait_agent`, ", "");
    let wake_body = format!(
        "Role.\n{wake_shared}\n{SUBAGENT_WAKE_ON_REPORT_USAGE_HINT_TEXT}\n\n{SUBAGENT_BLOCKED_USAGE_HINT_TEXT}\n\nThere are 2 available concurrency slots, meaning that up to 2 agents can be active at once, including you."
    );

    for (agent_polling_enabled, expected_body) in [(true, polling_body), (false, wake_body)] {
        let instructions = MultiAgentRoleInstructions::Composed {
            base: "Role.".to_string(),
            marked: false,
            omit_update_plan_instructions: false,
            max_concurrency: 2,
            agent_polling_enabled,
            expose_model_overrides: false,
            is_root: false,
            subagent_context_token_cap: None,
        };

        assert_eq!(instructions.body(), expected_body);
    }
}

#[test]
fn wake_mode_root_guidance_states_how_child_results_arrive() {
    let shared = DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT.replace("`wait_agent`, ", "");
    let expected_body = format!(
        "Role.\n{shared}\nChild results arrive as new turns; when you're waiting only on children, end your turn with a short status.\n\nThere are 2 available concurrency slots, meaning that up to 2 agents can be active at once, including you."
    );
    let instructions = MultiAgentRoleInstructions::Composed {
        base: "Role.".to_string(),
        marked: false,
        omit_update_plan_instructions: false,
        max_concurrency: 2,
        agent_polling_enabled: false,
        expose_model_overrides: false,
        is_root: true,
        subagent_context_token_cap: Some(272_000),
    };

    assert_eq!(instructions.body(), expected_body);
}

#[test]
fn subagent_guidance_states_context_budget() {
    let shared = DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT;
    let wait = DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT;
    let expected_body = format!(
        "Role.\n{shared}\n{wait}\n\n{SUBAGENT_BLOCKED_USAGE_HINT_TEXT}\n\nYour context budget is 272000 tokens; keep tool output focused and report partial results before you exceed it.\n\nThere are 2 available concurrency slots, meaning that up to 2 agents can be active at once, including you."
    );
    let instructions = MultiAgentRoleInstructions::Composed {
        base: "Role.".to_string(),
        marked: false,
        omit_update_plan_instructions: false,
        max_concurrency: 2,
        agent_polling_enabled: true,
        expose_model_overrides: false,
        is_root: false,
        subagent_context_token_cap: Some(272_000),
    };

    assert_eq!(instructions.body(), expected_body);
}
