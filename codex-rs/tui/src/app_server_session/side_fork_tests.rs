//! Side forks disable collaboration on the wire, including compatibility retries.

use super::*;
use crate::legacy_core::config::ConfigBuilder;
use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn side_fork_overrides_disable_agents_and_survive_pagination_retry() -> Result<()> {
    for mode in [ThreadParamsMode::Embedded, ThreadParamsMode::Remote] {
        for (presentation, retry) in [
            (ForkPresentation::SideConversation, false),
            (ForkPresentation::SideConversation, true),
            (ForkPresentation::Regular, false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut socket = tokio_tungstenite::accept_async(stream).await?;
                let mut requests = Vec::new();
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                        continue;
                    };
                    let mut reply = match request.method.as_str() {
                        "initialize" => json!({"result": {"userAgent": "side-fork-test/1.0"}}),
                        "thread/read" => {
                            json!({"error": {"code": -32603, "message": "no saved title"}})
                        }
                        "thread/fork" => {
                            requests.push(request.params.unwrap());
                            if retry && requests.len() == 1 {
                                json!({"error": {"code": -32602, "message": "unknown field excludeTurns"}})
                            } else {
                                // Stop after capturing the request; runtime coverage uses a real server.
                                json!({"error": {"code": -32603, "message": "captured fork"}})
                            }
                        }
                        method => panic!("unexpected request: {method}"),
                    };
                    reply["id"] = json!(request.id);
                    socket.send(Message::Text(reply.to_string().into())).await?;
                }
                Ok::<_, color_eyre::Report>(requests)
            });
            let home = tempfile::tempdir()?;
            let config = ConfigBuilder::default()
                .codex_home(home.path().to_path_buf())
                .cli_overrides(vec![
                    ("features.multi_agent".into(), toml::Value::Boolean(true)),
                    ("features.multi_agent_v2".into(), toml::Value::Boolean(true)),
                    (
                        "features.shell_snapshot".into(),
                        toml::Value::Boolean(false),
                    ),
                ])
                .build()
                .await?;
            let launch_overrides = config_request_overrides_from_config(&config, mode);
            let mut session =
                AppServerSession::new(crate::connect_remote_app_server(endpoint).await?, mode);
            let local_settings = LocalSettings::from(&config);
            let thread_id = ThreadId::new();
            let result = match presentation {
                ForkPresentation::SideConversation => {
                    session
                        .fork_side_thread(
                            &local_settings,
                            config.clone(),
                            thread_id,
                            /*selected_profile*/ None,
                        )
                        .await
                }
                ForkPresentation::Regular => {
                    session
                        .fork_thread(&local_settings, config.clone(), thread_id)
                        .await
                }
            };
            assert!(format!("{:#}", result.unwrap_err()).contains("captured fork"));
            session.shutdown().await?;
            let requests = server.await??;
            assert_eq!(requests.len(), if retry { 2 } else { 1 });
            for request in &requests {
                let overrides = &request["config"];
                let agents_enabled = presentation == ForkPresentation::Regular;
                assert_eq!(
                    overrides["features"],
                    json!({"multi_agent": agents_enabled, "multi_agent_v2": agents_enabled, "shell_snapshot": false})
                );
                let actual = [
                    overrides.get("features.multi_agent"),
                    overrides.get("features.multi_agent_v2"),
                    overrides.get("agents.enabled"),
                ];
                let disabled = Value::Bool(false);
                let expected = match presentation {
                    ForkPresentation::SideConversation => [Some(&disabled); 3],
                    ForkPresentation::Regular => [None; 3],
                };
                assert_eq!(actual, expected);
            }
            if retry {
                assert_eq!(requests[0]["excludeTurns"], json!(true));
                let mut fallback = requests[0].clone();
                // The wire protocol omits this field when false.
                fallback
                    .as_object_mut()
                    .expect("fork params")
                    .remove("excludeTurns");
                assert_eq!(requests[1], fallback);
            }
            assert_eq!(
                config_request_overrides_from_config(&config, mode),
                launch_overrides
            );
        }
    }
    Ok(())
}
