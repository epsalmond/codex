use anyhow::Result;
use codex_core::StartIfIdleSubmission;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::config::Config;
use codex_extension_api::ConfigContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::sync::oneshot;
use tokio::time::timeout;

const INITIAL_MODEL: &str = "gpt-5.4";
pub(super) const COMMITTED_MODEL: &str = "gpt-5.2";
const TIMEOUT: Duration = Duration::from_secs(10);

fn assert_checkpoints(test: &TestCodex, expected: &[ThreadSettingsSnapshot]) -> Result<()> {
    let rollout_path = test.codex.rollout_path().expect("rollout path");
    let rollout: Vec<RolloutLine> = std::fs::read_to_string(rollout_path)?
        .lines()
        .map(codex_rollout::parse_rollout_line)
        .collect::<std::result::Result<_, _>>()?;
    let snapshots: Vec<_> = rollout
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(applied))
                if applied.thread_id == Some(test.session_configured.thread_id) =>
            {
                Some(applied.thread_settings)
            }
            _ => None,
        })
        .collect();
    assert_eq!(snapshots, expected);
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test]
async fn initial_plugin_ids_use_turn_context_without_extra_settings_checkpoints(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .build_with_auto_env(&server)
        .await?;
    let selected = vec!["slack@openai".to_string()];
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            disabled_plugin_ids: Some(selected.clone()),
            ..Default::default()
        },
    )
    .await?;
    let response = responses::mount_sse_once(&server, responses::sse_completed("first turn")).await;
    let submission = test
        .codex
        .start_turn_if_idle(TurnInputRequest::user_input(Vec::new()))
        .await?;
    let StartIfIdleSubmission::Started { turn_id } = submission else {
        panic!("expected an accepted first turn, got {submission:?}");
    };
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(completed) if completed.turn_id == turn_id),
    )
    .await;
    test.codex.flush_rollout().await?;
    assert_checkpoints(&test, &[])?;
    let rollout_path = test.codex.rollout_path().expect("rollout path");
    let (items, _, parse_errors) =
        codex_rollout::RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(parse_errors, 0);
    let context = items.iter().find_map(|item| match item {
        RolloutItem::TurnContext(context)
            if context.turn_id.as_deref() == Some(turn_id.as_str()) =>
        {
            Some(context)
        }
        _ => None,
    });
    assert_eq!(
        context
            .expect("inputless first turn context")
            .disabled_plugin_ids,
        Some(selected)
    );
    assert_eq!(response.requests().len(), 1);

    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            disabled_plugin_ids: Some(vec![]),
            ..Default::default()
        },
    )
    .await?;
    let expected = vec![test.codex.thread_settings_snapshot().await];
    assert!(expected[0].disabled_plugin_ids.is_empty());
    test.codex.flush_rollout().await?;
    assert_checkpoints(&test, &expected)?;
    let response =
        responses::mount_sse_once(&server, responses::sse_completed("second turn")).await;
    test.submit_text_turn("second turn").await?;
    test.codex.flush_rollout().await?;
    assert_eq!(response.requests().len(), 1);
    assert_checkpoints(&test, &expected)?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

pub(super) struct PauseAfterCommit {
    pub(super) gate: Mutex<Option<(oneshot::Sender<()>, mpsc::Receiver<()>)>>,
}

impl ConfigContributor<Config> for PauseAfterCommit {
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
        _previous_config: &Config,
        new_config: &Config,
    ) {
        if new_config.model.as_deref() != Some(COMMITTED_MODEL) {
            return;
        }
        let Some((entered, release)) = self.gate.lock().expect("commit gate lock").take() else {
            return;
        };
        entered.send(()).expect("test is waiting for the commit");
        // The callback is synchronous, so this test uses a second runtime worker.
        release
            .recv_timeout(TIMEOUT)
            .expect("test releases the committed update");
    }
}

#[derive(Clone, Copy)]
enum SettingsOperation {
    TurnStart,
    Standalone,
}

#[test_case(SettingsOperation::TurnStart; "turn start retains its committed settings and notification")]
#[test_case(SettingsOperation::Standalone; "standalone notification retains its committed settings")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_notifications_keep_their_commit_across_postcommit_work(
    operation: SettingsOperation,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("response"),
            responses::ev_completed("response"),
        ]),
    )
    .await;
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.config_contributor(Arc::new(PauseAfterCommit {
        gate: Mutex::new(Some((entered_tx, release_rx))),
    }));
    let test = test_codex()
        .with_model(INITIAL_MODEL)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    let mut initial = test.codex.restorable_thread_settings().await;
    // Restore only runtime model settings, without overwriting the committed plugin selection.
    initial.disabled_plugin_ids = None;
    let thread_settings = ThreadSettingsOverrides {
        model: Some(COMMITTED_MODEL.to_string()),
        disabled_plugin_ids: Some(vec!["slack@openai".to_string()]),
        context_settings: Some(
            codex_protocol::context_settings::ContextSettingsUpdate::Patch {
                overrides: codex_protocol::context_settings::ContextSettingsOverrides {
                    shake_threshold: Some(codex_protocol::context_settings::ShakeThreshold::Off),
                    ..Default::default()
                },
            },
        ),
        ..Default::default()
    };
    let submission = tokio::spawn({
        let codex = Arc::clone(&test.codex);
        async move {
            match operation {
                SettingsOperation::TurnStart => {
                    let result = codex
                        .start_or_steer_turn(
                            TurnInputRequest::user_input(vec![UserInput::Text {
                                text: "use the committed model".to_string(),
                                text_elements: Vec::new(),
                            }])
                            .with_thread_settings(thread_settings),
                        )
                        .await?;
                    let TurnInputSubmission::Started { turn_id } = result else {
                        panic!("expected a new turn, got {result:?}");
                    };
                    Ok(turn_id)
                }
                SettingsOperation::Standalone => {
                    codex
                        .submit(Op::ThreadSettings {
                            thread_settings,
                            reply: None,
                        })
                        .await
                }
            }
        }
    });

    timeout(TIMEOUT, entered_rx).await??;
    let expected = test.codex.thread_settings_snapshot().await;
    assert_eq!(expected.model, COMMITTED_MODEL);
    assert_eq!(expected.disabled_plugin_ids, vec!["slack@openai"]);
    // Submitted operations are serialized. Runtime restoration is an existing
    // direct writer, so it can overlap the first operation's post-commit work.
    timeout(TIMEOUT, test.codex.restore_thread_settings(initial)).await??;
    let restored = test.codex.thread_settings_snapshot().await;
    assert_eq!(restored.model, INITIAL_MODEL);
    assert_eq!(restored.disabled_plugin_ids, vec!["slack@openai"]);
    release_tx.send(())?;
    let submission_id = timeout(TIMEOUT, submission).await???;

    let applied = timeout(TIMEOUT, async {
        loop {
            let event = test.codex.next_event().await?;
            match event.msg {
                EventMsg::ThreadSettingsApplied(applied) if event.id == submission_id => {
                    return Ok::<_, anyhow::Error>((applied.thread_id, applied.thread_settings));
                }
                EventMsg::Error(error) => {
                    anyhow::bail!("settings update failed: {}", error.message)
                }
                _ => {}
            }
        }
    })
    .await??;
    assert_eq!(applied, (Some(test.session_configured.thread_id), expected));

    let expected_request_model = match operation {
        SettingsOperation::TurnStart => COMMITTED_MODEL,
        SettingsOperation::Standalone => {
            assert!(response.requests().is_empty());
            test.codex
                .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                    text: "use the restored settings".to_string(),
                    text_elements: Vec::new(),
                }]))
                .await?;
            INITIAL_MODEL
        }
    };
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(
        response.single_request().body_json()["model"],
        expected_request_model
    );
    assert_eq!(test.codex.thread_settings_snapshot().await, restored);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_checkpoints_settings_changed_during_its_model_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let (release_compaction, compaction_gate) = oneshot::channel();
    let (server, _completions) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("turn"),
                responses::ev_completed("turn"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: Some(compaction_gate),
            body: responses::sse(vec![
                responses::ev_response_created("compact"),
                responses::ev_assistant_message("summary", "compacted history"),
                responses::ev_completed("compact"),
            ]),
        }],
    ])
    .await;
    let test = test_codex()
        .with_model(INITIAL_MODEL)
        .with_config(|config| {
            // Local compaction lets the SSE gate hold its response in flight.
            config.model_provider.name = "OpenAI (test)".to_string();
        })
        .build_with_streaming_server(&server)
        .await?;
    test.submit_text_turn("before compaction").await?;
    test.codex.submit(Op::Compact).await?;
    timeout(TIMEOUT, server.wait_for_request_count(/*count*/ 2)).await?;

    let request: serde_json::Value = serde_json::from_slice(&server.requests().await[1])?;
    assert_eq!(request["model"], INITIAL_MODEL);
    let updated_cwd = TempDir::new()?;
    let updated_cwd_path = AbsolutePathBuf::try_from(updated_cwd.path())?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            environments: Some(local_selections(updated_cwd_path.clone())),
            model: Some(COMMITTED_MODEL.to_string()),
            disabled_plugin_ids: Some(vec!["slack@openai".to_string()]),
            context_settings: Some(
                codex_protocol::context_settings::ContextSettingsUpdate::Patch {
                    overrides: codex_protocol::context_settings::ContextSettingsOverrides {
                        shake_threshold: Some(
                            codex_protocol::context_settings::ShakeThreshold::Off,
                        ),
                        ..Default::default()
                    },
                },
            ),
            ..Default::default()
        },
    )
    .await?;
    let expected = test.codex.thread_settings_snapshot().await;
    assert_eq!(
        (&expected.cwd, expected.model.as_str()),
        (&updated_cwd_path, COMMITTED_MODEL)
    );
    release_compaction.send(()).expect("compaction is waiting");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.codex.shutdown_and_wait().await?;

    let rollout_path = test.session_configured.rollout_path.expect("rollout path");
    let rollout: Vec<RolloutLine> = std::fs::read_to_string(rollout_path)?
        .lines()
        .map(codex_rollout::parse_rollout_line)
        .collect::<std::result::Result<_, _>>()?;
    let checkpoint = rollout
        .iter()
        .skip_while(|line| !matches!(line.item, RolloutItem::Compacted(_)))
        .find_map(|line| match &line.item {
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(applied)) => {
                Some((applied.thread_id, &applied.thread_settings))
            }
            _ => None,
        });
    assert_eq!(
        checkpoint,
        Some((Some(test.session_configured.thread_id), &expected))
    );
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn context_settings_commit_reset_validation_and_restore_are_atomic() -> Result<()> {
    use codex_protocol::context_settings::CompactionThreshold;
    use codex_protocol::context_settings::ContextSettingsOverrides;
    use codex_protocol::context_settings::ContextSettingsUpdate;
    use codex_protocol::context_settings::ShakeThreshold;
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_model("gpt-5.6")
        .with_config(|config| {
            config.model_auto_compact_token_limit = Some(12345);
        })
        .build_with_auto_env(&server)
        .await?;
    let initial = test.codex.thread_settings_snapshot().await;
    let initial_view = test.codex.context_settings().await;
    let mut legacy = serde_json::to_value(&initial)?;
    legacy.as_object_mut().unwrap().remove("context_settings");
    let mut legacy_expected = initial.clone();
    legacy_expected.context_settings = None;
    assert_eq!(
        serde_json::from_value::<ThreadSettingsSnapshot>(legacy)?,
        legacy_expected
    );
    let selection = ContextSettingsOverrides {
        shake_threshold: Some(ShakeThreshold::Percent { percent: 37 }),
        shake_cold_resume: Some(false),
        shake_min_elidable_percent: Some(9),
        shake_min_savings_tokens: Some(123),
        compaction_threshold: Some(CompactionThreshold::ModelDefault),
        post_turn_compaction_percent: Some(0),
        child_reduction_enabled: Some(false),
        child_reduction_threshold_tokens: Some(98765),
        ..Default::default()
    };
    test.codex
        .update_thread_settings(ThreadSettingsOverrides {
            context_settings: Some(ContextSettingsUpdate::Patch {
                overrides: selection.clone(),
            }),
            ..Default::default()
        })
        .await?;
    let mut expected = initial.clone();
    expected.context_settings.as_mut().unwrap().overrides = selection.clone();
    assert_eq!(test.codex.thread_settings_snapshot().await, expected);
    let view = test.codex.context_settings().await;
    assert_eq!(view.requested, selection);
    assert_eq!(
        view.shake_threshold_tokens,
        view.resolved_context_window.map(|window| window * 37 / 100)
    );
    assert_ne!(view.compaction_scope_token_limit, Some(12345));
    assert_eq!(view.inherited_child_active_cap, None);
    let saved = codex_core::CodexThreadSettingsOverrides {
        restored_context_settings: expected.context_settings.clone(),
        ..Default::default()
    };
    let invalid = test
        .codex
        .update_thread_settings(ThreadSettingsOverrides {
            model: Some(COMMITTED_MODEL.into()),
            context_settings: Some(ContextSettingsUpdate::Patch {
                overrides: ContextSettingsOverrides {
                    shake_threshold: Some(ShakeThreshold::Percent { percent: 0 }),
                    ..Default::default()
                },
            }),
            ..Default::default()
        })
        .await;
    assert!(invalid.is_err());
    assert_eq!(test.codex.thread_settings_snapshot().await, expected);
    test.codex.checkpoint_thread_settings().await?;
    test.codex
        .update_thread_settings(ThreadSettingsOverrides {
            context_settings: Some(ContextSettingsUpdate::Reset),
            ..Default::default()
        })
        .await?;
    assert_eq!(test.codex.thread_settings_snapshot().await, initial);
    assert_eq!(test.codex.context_settings().await, initial_view);
    test.codex.restore_thread_settings(saved).await?;
    assert_eq!(test.codex.thread_settings_snapshot().await, expected);
    // An empty restore is replacement, rather than leaving the previous patch behind.
    let empty = codex_core::CodexThreadSettingsOverrides {
        restored_context_settings: initial.context_settings.clone(),
        ..Default::default()
    };
    test.codex.restore_thread_settings(empty).await?;
    assert_eq!(test.codex.thread_settings_snapshot().await, initial);
    Ok(())
}

#[tokio::test]
async fn context_patch_and_reset_preserve_automatic_continuation_identity() -> Result<()> {
    use codex_protocol::context_settings::ContextSettingsOverrides;
    use codex_protocol::context_settings::ContextSettingsUpdate;
    use codex_protocol::context_settings::ShakeThreshold;
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    responses::mount_sse_once(&server, responses::sse_completed("initial")).await;
    let TurnInputSubmission::Started { mut turn_id } = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Work".into(),
            text_elements: vec![],
        }]))
        .await?
    else {
        panic!("expected started turn")
    };
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    for update in [
        ContextSettingsUpdate::Patch {
            overrides: ContextSettingsOverrides {
                shake_threshold: Some(ShakeThreshold::Off),
                ..Default::default()
            },
        },
        ContextSettingsUpdate::Reset,
    ] {
        test.codex
            .update_thread_settings(ThreadSettingsOverrides {
                context_settings: Some(update),
                ..Default::default()
            })
            .await?;
        responses::mount_sse_once(&server, responses::sse_completed("continue")).await;
        let submission = test
            .codex
            .continue_turn_if_idle(
                TurnInputRequest::new(codex_core::TurnInput::ResponseItem(
                    responses::user_message_item("Continue"),
                )),
                turn_id.clone(),
            )
            .await?;
        let TurnInputSubmission::Started { turn_id: next } = submission else {
            panic!("continuation was superseded: {submission:?}")
        };
        turn_id = next;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    test.codex
        .update_thread_settings(ThreadSettingsOverrides {
            model: Some(INITIAL_MODEL.into()),
            context_settings: Some(ContextSettingsUpdate::Reset),
            ..Default::default()
        })
        .await?;
    assert!(matches!(
        test.codex
            .continue_turn_if_idle(
                TurnInputRequest::new(codex_core::TurnInput::ResponseItem(
                    responses::user_message_item("Continue")
                )),
                turn_id
            )
            .await?,
        TurnInputSubmission::NotSubmitted { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn context_settings_checkpoint_failure_does_not_revert_consumed_selection() -> Result<()> {
    use codex_protocol::context_settings::ContextSettingsOverrides;
    use codex_protocol::context_settings::ContextSettingsUpdate;
    use codex_protocol::context_settings::ShakeThreshold;
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    test.codex
        .update_thread_settings(ThreadSettingsOverrides {
            context_settings: Some(ContextSettingsUpdate::Patch {
                overrides: ContextSettingsOverrides {
                    shake_threshold: Some(ShakeThreshold::Off),
                    ..Default::default()
                },
            }),
            ..Default::default()
        })
        .await?;
    responses::mount_sse_once(&server, responses::sse_completed("consumed")).await;
    test.submit_text_turn("Consume the accepted settings")
        .await?;
    let expected = test.codex.thread_settings_snapshot().await;
    // Use the existing store's owned writer teardown, rather than a new failure-injection API.
    test.thread_store
        .shutdown_thread(test.session_configured.thread_id)
        .await?;
    assert!(test.codex.checkpoint_thread_settings().await.is_err());
    assert_eq!(test.codex.thread_settings_snapshot().await, expected);
    Ok(())
}

#[tokio::test]
async fn context_settings_direct_child_start_retains_basis_and_owned_selection() -> Result<()> {
    use codex_protocol::context_settings::ContextSettingsOverrides;
    use codex_protocol::context_settings::ContextSettingsState;
    use codex_protocol::context_settings::ContextSettingsUpdate;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_session_source(SessionSource::SubAgent(SubAgentSource::Other(
            "context-start".into(),
        )))
        .with_config(|config| {
            config.model_auto_compact_token_limit = Some(90000);
            config.subagent_context_reduction.threshold_tokens = 30000;
            config.context_settings = Some(ContextSettingsState {
                overrides: ContextSettingsOverrides {
                    child_reduction_threshold_tokens: Some(70000),
                    ..Default::default()
                },
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    let initial = test.codex.context_settings().await;
    assert_eq!(
        (
            initial.requested.child_reduction_threshold_tokens,
            initial.inherited_child_default_tokens,
            initial.inherited_child_active_cap,
            initial.compaction_scope_token_limit,
        ),
        (Some(70000), Some(30000), Some(70000), Some(70000))
    );
    test.codex
        .update_thread_settings(ThreadSettingsOverrides {
            context_settings: Some(ContextSettingsUpdate::Reset),
            ..Default::default()
        })
        .await?;
    let reset = test.codex.context_settings().await;
    assert_eq!(
        (reset.requested, reset.compaction_scope_token_limit),
        (ContextSettingsOverrides::default(), Some(30000))
    );
    Ok(())
}
