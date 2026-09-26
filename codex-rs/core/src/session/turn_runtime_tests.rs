use super::maybe_run_pre_sampling_auto_shake;
use crate::session::tests::make_session_and_context;
use codex_config::config_toml::AutoShakeDurationToml;
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
