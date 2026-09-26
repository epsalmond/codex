use super::maybe_run_pre_sampling_auto_shake;
use super::take_context_reset_request;
use crate::session::TurnInput;
use crate::session::tests::make_session_and_context;
use codex_config::config_toml::AutoShakeDurationToml;
use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use std::sync::Arc;

/// The cold-resume marker is set before the shake pass runs. Re-entering the
/// same pre-sampling path without a request in between must therefore skip the
/// second decision for that idle window.
#[tokio::test]
#[tracing_test::traced_test]
async fn cold_resume_decision_is_deduped_before_the_next_sampling_request() {
    let (session, mut turn_context) = make_session_and_context().await;
    Arc::make_mut(&mut turn_context.config).auto_shake.cache_ttl = Some(AutoShakeDurationToml(0));
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);

    session.prompt_cache_clock.record_sampling_request();
    maybe_run_pre_sampling_auto_shake(&session, &turn_context).await;
    assert!(session.prompt_cache_clock.cold_resume_already_decided());

    maybe_run_pre_sampling_auto_shake(&session, &turn_context).await;
    assert!(
        logs_contain("cold_resume_already_decided"),
        "the repeated pre-sampling decision should record its deduplication reason"
    );
}

#[tokio::test]
async fn context_reset_request_is_retained_when_turn_has_no_follow_up() {
    let (session, _) = make_session_and_context().await;
    session.request_new_context_window().await;

    assert!(!take_context_reset_request(&session, /*needs_follow_up*/ false).await);
    assert!(take_context_reset_request(&session, /*needs_follow_up*/ true).await);
    assert!(!take_context_reset_request(&session, /*needs_follow_up*/ true).await);
}

#[test]
fn explicit_assignment_input_is_distinct_from_queue_only_wakeups() {
    let queue_only = TurnInput::InterAgentCommunication(InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        "status update".to_string(),
        /*trigger_turn*/ false,
    ));
    let followup = TurnInput::InterAgentCommunication(InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        "new assignment".to_string(),
        /*trigger_turn*/ true,
    ));
    let user_input = TurnInput::UserInput {
        content: Vec::new(),
        client_id: None,
        acceptance_order: None,
    };

    assert!(!super::has_explicit_assignment_input(&[queue_only]));
    assert!(super::has_explicit_assignment_input(&[followup]));
    assert!(super::has_explicit_assignment_input(&[user_input]));
}
