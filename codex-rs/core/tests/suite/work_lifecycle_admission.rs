use anyhow::Result;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use wiremock::Mock;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_core_session_can_complete_more_than_32_root_turns() -> Result<()> {
    const TURN_COUNT: usize = 33;
    let server = start_mock_server().await;
    Mock::given(method("POST"))
        .and(path_regex("/v1/responses"))
        .respond_with(sse_response(sse(vec![
            ev_response_created("legacy-turn"),
            ev_completed("legacy-turn"),
        ])))
        .expect(TURN_COUNT as u64)
        .mount(&server)
        .await;

    let test = test_codex().build_with_auto_env(&server).await?;
    for index in 0..TURN_COUNT {
        test.submit_text_turn(&format!("legacy turn {index}"))
            .await?;
    }
    Ok(())
}
