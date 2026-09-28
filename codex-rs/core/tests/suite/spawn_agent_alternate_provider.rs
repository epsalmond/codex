//! Verifies that an agent role can run its child on a different configured model provider.

use anyhow::Result;
use codex_core::config::AgentRoleConfig;
use codex_core::config::Config;
use codex_features::Feature;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

const ROOT_PROMPT: &str = "spawn the local worker";
const CHILD_PROMPT: &str = "summarize the local notes";
const SPAWN_CALL_ID: &str = "spawn-local-worker";
const LOCAL_ROLE: &str = "local-worker";
const LOCAL_PROVIDER: &str = "local-provider";
const LOCAL_MODEL: &str = "local-only-model";

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    let body = match request
        .headers
        .get("content-encoding")
        .and_then(|encoding| encoding.to_str().ok())
    {
        Some(encoding) if encoding.eq_ignore_ascii_case("zstd") => {
            zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()
        }
        _ => Some(request.body.clone()),
    };
    body.and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

fn is_child_request(request: &wiremock::Request) -> bool {
    body_contains(request, CHILD_PROMPT) && !body_contains(request, ROOT_PROMPT)
}

fn configure_local_role(config: &mut Config, role_provider: &str, local_base_url: Option<String>) {
    for feature in [Feature::Collab, Feature::MultiAgentV2] {
        config
            .features
            .enable(feature)
            .expect("test config should allow feature update");
    }
    config.model_provider.request_max_retries = Some(0);
    config.model_provider.stream_max_retries = Some(0);
    if let Some(base_url) = local_base_url {
        let local_provider = ModelProviderInfo {
            name: "Local".to_string(),
            base_url: Some(base_url),
            requires_openai_auth: false,
            ..config.model_provider.clone()
        };
        config
            .model_providers
            .insert(LOCAL_PROVIDER.to_string(), local_provider);
    }
    let role_path = config.codex_home.join("local-worker.toml");
    std::fs::write(
        &role_path,
        format!("model_provider = \"{role_provider}\"\nmodel = \"{LOCAL_MODEL}\"\n"),
    )
    .expect("local role should be written");
    config.agent_roles.insert(
        LOCAL_ROLE.to_string(),
        AgentRoleConfig {
            description: Some("Runs on the local provider".to_string()),
            config_file: Some(role_path.to_path_buf()),
            nickname_candidates: None,
        },
    );
}

/// Mounts the root's `spawn_agent` call and the follow-up that consumes its output.
async fn mount_root_spawn(server: &wiremock::MockServer) -> ResponseMock {
    mount_sse_once_match(
        server,
        |request: &wiremock::Request| {
            body_contains(request, ROOT_PROMPT) && !body_contains(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("root-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                "collaboration",
                "spawn_agent",
                &json!({
                    "message": CHILD_PROMPT,
                    "task_name": "local",
                    "agent_type": LOCAL_ROLE,
                    "fork_turns": "none",
                })
                .to_string(),
            ),
            ev_completed("root-spawn"),
        ]),
    )
    .await;
    mount_sse_once_match(
        server,
        |request: &wiremock::Request| {
            body_contains(request, ROOT_PROMPT) && body_contains(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("root-finished"),
            ev_assistant_message("root-finished-message", "spawn handled"),
            ev_completed("root-finished"),
        ]),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_model_provider_routes_child_to_that_provider() -> Result<()> {
    let root_server = start_mock_server().await;
    let local_server = start_mock_server().await;
    let local_base_url = format!("{}/v1", local_server.uri());
    let mut builder = test_codex().with_config(move |config| {
        configure_local_role(config, LOCAL_PROVIDER, Some(local_base_url));
    });
    let test = builder.build_with_auto_env(&root_server).await?;
    let mut created_threads = test.thread_manager.subscribe_thread_created();

    mount_root_spawn(&root_server).await;
    let child_request = mount_sse_once_match(
        &local_server,
        is_child_request,
        sse(vec![
            ev_response_created("child"),
            ev_assistant_message("child-message", "local worker done"),
            ev_completed("child"),
        ]),
    )
    .await;

    test.submit_text_turn(ROOT_PROMPT).await?;
    let child_thread_id = created_threads.recv().await?;
    let child = test.thread_manager.get_thread(child_thread_id).await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let snapshot = child.config_snapshot().await;
    assert_eq!(
        (snapshot.model_provider_id.as_str(), snapshot.model.as_str()),
        (LOCAL_PROVIDER, LOCAL_MODEL)
    );
    assert_eq!(
        child_request.single_request().body_json()["model"],
        json!(LOCAL_MODEL)
    );
    let root_requests = root_server.received_requests().await.unwrap_or_default();
    assert_eq!(
        root_requests
            .iter()
            .filter(|request| is_child_request(request))
            .count(),
        0,
        "the child must not call the root provider"
    );

    child.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_with_unknown_model_provider_fails_spawn() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        configure_local_role(config, "missing-provider", /*local_base_url*/ None);
    });
    let test = builder.build_with_auto_env(&server).await?;

    let root_followup = mount_root_spawn(&server).await;
    test.submit_text_turn(ROOT_PROMPT).await?;

    assert_eq!(
        root_followup.function_call_output_text(SPAWN_CALL_ID),
        Some(format!(
            "agent_type '{LOCAL_ROLE}' references unknown model_provider `missing-provider`; define it under [model_providers] in the root config.toml"
        ))
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
