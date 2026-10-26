use std::sync::Arc;
use std::sync::Weak;

use codex_history::ResponseItemEnvelope;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_tools::ConversationHistory;
use codex_tools::ExtensionTurnItem;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolCall as ExtensionToolCall;
use codex_tools::ToolEnvironment;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use codex_tools::TurnItemEmissionFuture;
use codex_tools::TurnItemEmitter;
use codex_utils_string::to_ascii_json_string;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::lifecycle::extension_tool_call_source;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::turn_metadata::ExecutionMetadata;

pub(crate) struct ExtensionToolAdapter(
    Arc<dyn for<'call> codex_tools::ToolExecutor<ExtensionToolCall<'call>>>,
);

impl ExtensionToolAdapter {
    pub(crate) fn new(
        executor: Arc<dyn for<'call> codex_tools::ToolExecutor<ExtensionToolCall<'call>>>,
    ) -> Self {
        Self(executor)
    }
}

impl ToolExecutor<ToolInvocation> for ExtensionToolAdapter {
    fn tool_name(&self) -> ToolName {
        self.0.tool_name()
    }

    fn spec(&self) -> ToolSpec {
        self.0.spec()
    }

    fn exposure(&self) -> crate::tools::registry::ToolExposure {
        self.0.exposure()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.0.supports_parallel_tool_calls()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.0.search_info()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let environment_accessors = invocation.step_context.extension_environments();
            let environments = environment_accessors
                .iter()
                .map(|(environment, fs, access)| {
                    ToolEnvironment::new(
                        environment.selection.environment_id.clone(),
                        environment.cwd().clone(),
                        fs,
                    )
                    .with_local_navigation_access(*access)
                })
                .collect();
            self.0
                .handle(to_extension_call(&invocation, environments).await)
                .await
        })
    }
}

impl CoreToolRuntime for ExtensionToolAdapter {
    fn is_builtin_control_tool(&self) -> bool {
        let tool_name = self.0.tool_name();
        if tool_name.is_default_namespace() {
            return matches!(
                tool_name.name.as_str(),
                "get_goal" | "create_goal" | "update_goal"
            );
        }
        matches!(
            (tool_name.namespace.as_deref(), tool_name.name.as_str()),
            (
                Some("notes"),
                "list_files_by_prefix"
                    | "read_file"
                    | "search_contents"
                    | "append_to_file"
                    | "write_file"
            ) | (
                Some("history"),
                "list_windows" | "list_items" | "read_item" | "search_contents"
            )
        )
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        match payload {
            ToolPayload::Function { .. } => true,
            ToolPayload::Custom { .. } => match self.0.spec() {
                ToolSpec::Freeform(_) => true,
                ToolSpec::Namespace(namespace) => namespace.tools.iter().any(|tool| {
                    matches!(
                        tool,
                        ResponsesApiNamespaceTool::Custom(tool)
                            if tool.name == self.0.tool_name().name
                    )
                }),
                ToolSpec::Function(_)
                | ToolSpec::ToolSearch { .. }
                | ToolSpec::WebSearch { .. } => false,
            },
            ToolPayload::ToolSearch { .. } => false,
        }
    }
}

struct CoreTurnItemEmitter {
    session: Weak<Session>,
    turn: Weak<TurnContext>,
}

async fn emit_legacy_events(session: &Session, turn: &TurnContext, legacy_events: Vec<EventMsg>) {
    for msg in legacy_events {
        session
            .send_event_raw(Event {
                id: turn.sub_id.clone(),
                msg,
            })
            .await;
    }
}

impl TurnItemEmitter for CoreTurnItemEmitter {
    fn emit_started<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let ExtensionTurnItem {
                item,
                legacy_events,
            } = item;
            let item = TurnItem::Extension(item);
            session.emit_turn_item_started(turn.as_ref(), &item).await;
            emit_legacy_events(session.as_ref(), turn.as_ref(), legacy_events).await;
        })
    }

    fn emit_completed<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let ExtensionTurnItem {
                item,
                legacy_events,
            } = item;
            let item = TurnItem::Extension(item);
            session.emit_turn_item_completed(turn.as_ref(), item).await;
            emit_legacy_events(session.as_ref(), turn.as_ref(), legacy_events).await;
        })
    }
}

async fn to_extension_call<'a>(
    invocation: &ToolInvocation,
    environments: Vec<ToolEnvironment<'a>>,
) -> ExtensionToolCall<'a> {
    let history = invocation
        .session
        .clone_history()
        .await
        .into_shared_annotated_items();
    let conversation_history = ConversationHistory::new_deferred(move || {
        Arc::unwrap_or_clone(history)
            .into_iter()
            .map(ResponseItemEnvelope::into_item)
            .collect()
    });
    let settings = &invocation.step_context.settings;
    let codex_turn_metadata = invocation
        .turn
        .turn_metadata_state
        .current_meta_value_for_extension_request(ExecutionMetadata::from_settings(
            &invocation.step_context.settings,
        ))
        .and_then(|metadata| to_ascii_json_string(&metadata).ok());
    ExtensionToolCall {
        turn_id: invocation.turn.sub_id.clone(),
        call_id: invocation.call_id.clone(),
        tool_name: invocation.tool_name.clone(),
        model: settings.model_info.slug.clone(),
        codex_turn_metadata,
        truncation_policy: settings.model_info.truncation_policy.into(),
        source: extension_tool_call_source(invocation.source.clone()),
        cancellation_token: invocation.cancellation_token.clone(),
        conversation_history,
        turn_item_emitter: Arc::new(CoreTurnItemEmitter {
            session: Arc::downgrade(&invocation.session),
            turn: Arc::downgrade(&invocation.turn),
        }),
        attachment_store: Arc::clone(&invocation.session.services.image_store),
        ephemeral: invocation.turn.config.ephemeral,
        environments,
        payload: invocation.payload.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_extension_items::ExtensionItem;
    use codex_extension_items::image_generation::GeneratedImageRef;
    use codex_extension_items::image_generation::ImageGenerationItem;
    use codex_extension_items::web_search::WebSearchItem;
    use codex_protocol::items::TurnItem;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ImageGenerationBeginEvent;
    use codex_protocol::protocol::ImageGenerationEndEvent;
    use codex_tools::ExtensionTurnItem;
    use codex_tools::ToolCallSource as ExtensionToolCallSource;
    use codex_utils_absolute_path::test_support::PathExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use codex_utils_path_uri::PathUri;
    use core_test_support::responses::strip_response_item_id;
    use core_test_support::responses::strip_response_item_ids;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::CoreTurnItemEmitter;
    use super::ExtensionToolAdapter;
    use crate::session::step_context::StepContext;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::hook_names::HookToolName;
    use crate::tools::registry::CoreToolRuntime;
    use crate::tools::registry::PostToolUsePayload;
    use crate::tools::registry::PreToolUsePayload;
    use crate::turn_diff_tracker::TurnDiffTracker;

    struct StubExtensionExecutor;

    impl<'call> codex_extension_api::ToolExecutor<codex_tools::ToolCall<'call>>
        for StubExtensionExecutor
    {
        fn tool_name(&self) -> codex_tools::ToolName {
            codex_tools::ToolName::plain("extension_echo")
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: "extension_echo".to_string(),
                description: "Echoes arguments.".to_string(),
                strict: true,
                parameters: codex_tools::parse_tool_input_schema(&json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                    },
                    "required": ["message"],
                    "additionalProperties": false,
                }))
                .expect("extension schema should parse"),
                output_schema: None,
                defer_loading: None,
            })
        }

        fn handle<'a>(
            &'a self,
            _call: codex_tools::ToolCall<'call>,
        ) -> codex_tools::ToolExecutorFuture<'a>
        where
            'call: 'a,
        {
            Box::pin(async {
                Ok(
                    Box::new(codex_tools::JsonToolOutput::new(json!({ "ok": true })))
                        as Box<dyn codex_tools::ToolOutput>,
                )
            })
        }
    }

    struct CapturingExtensionExecutor {
        captured_call: Arc<Mutex<Option<codex_tools::ToolCall<'static>>>>,
        captured_sandbox_cwds: Arc<Mutex<Vec<PathUri>>>,
    }

    impl<'call> codex_extension_api::ToolExecutor<codex_tools::ToolCall<'call>>
        for CapturingExtensionExecutor
    {
        fn tool_name(&self) -> codex_tools::ToolName {
            codex_tools::ToolName::plain("extension_echo")
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: "extension_echo".to_string(),
                description: "Captures arguments.".to_string(),
                strict: false,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
                defer_loading: None,
            })
        }

        fn handle<'a>(
            &'a self,
            call: codex_tools::ToolCall<'call>,
        ) -> codex_tools::ToolExecutorFuture<'a>
        where
            'call: 'a,
        {
            Box::pin(self.handle_call(call))
        }
    }

    impl CapturingExtensionExecutor {
        async fn handle_call(
            &self,
            call: codex_tools::ToolCall<'_>,
        ) -> Result<Box<dyn codex_tools::ToolOutput>, codex_tools::FunctionCallError> {
            call.turn_item_emitter
                .emit_started(ExtensionTurnItem {
                    item: ExtensionItem::WebSearch(WebSearchItem {
                        id: call.call_id.clone(),
                        query: String::new(),
                        action: None,
                        results: None,
                    }),
                    legacy_events: Vec::new(),
                })
                .await;
            // Record owned metadata only; the invocation lifetime belongs to this callback.
            *self.captured_sandbox_cwds.lock().await = call
                .environments
                .iter()
                .map(|environment| environment.cwd.clone())
                .collect();
            let call = codex_tools::ToolCall {
                environments: Vec::new(),
                ..call
            };
            *self.captured_call.lock().await = Some(call);
            Ok(
                Box::new(codex_tools::JsonToolOutput::new(json!({ "ok": true })))
                    as Box<dyn codex_tools::ToolOutput>,
            )
        }
    }

    #[test]
    fn function_extensions_reject_custom_payloads() {
        let handler = ExtensionToolAdapter::new(Arc::new(StubExtensionExecutor));

        assert!(handler.matches_kind(&ToolPayload::Function {
            arguments: "{}".to_string(),
        }));
        assert!(!handler.matches_kind(&ToolPayload::Custom {
            input: "raw input".to_string(),
        }));
    }

    #[tokio::test]
    async fn exposes_generic_hook_payloads() {
        let handler = ExtensionToolAdapter::new(Arc::new(StubExtensionExecutor));
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let turn = Arc::new(turn);
        let invocation = ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "call-extension".to_string(),
            tool_name: codex_tools::ToolName::plain("extension_echo"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({ "message": "hello" }).to_string(),
            },
        };
        let output = codex_tools::JsonToolOutput::new(json!({ "ok": true }));

        assert_eq!(
            CoreToolRuntime::pre_tool_use_payload(&handler, &invocation),
            Some(PreToolUsePayload {
                tool_name: HookToolName::new("extension_echo"),
                tool_input: json!({ "message": "hello" }),
            })
        );
        assert_eq!(
            CoreToolRuntime::post_tool_use_payload(&handler, &invocation, &output),
            Some(PostToolUsePayload {
                tool_name: HookToolName::new("extension_echo"),
                tool_use_id: "call-extension".to_string(),
                tool_input: json!({ "message": "hello" }),
                tool_response: json!({ "ok": true }),
            })
        );
    }

    #[tokio::test]
    async fn passes_step_node_repl_flag_and_scoped_turn_item_emitter_to_extension_call() {
        let captured_call = Arc::new(Mutex::new(None));
        let captured_sandbox_cwds = Arc::new(Mutex::new(Vec::new()));
        let handler = ExtensionToolAdapter::new(Arc::new(CapturingExtensionExecutor {
            captured_call: Arc::clone(&captured_call),
            captured_sandbox_cwds: Arc::clone(&captured_sandbox_cwds),
        }));
        let (session, turn, rx) = crate::session::tests::make_session_and_context_with_rx().await;
        let weak_session = Arc::downgrade(&session);
        let weak_turn = Arc::downgrade(&turn);
        let turn_id = turn.sub_id.clone();
        let model = turn.model_info().slug.clone();
        let truncation_policy = turn.model_info().truncation_policy.into();
        let expected_sandbox_cwds = turn
            .initial_environments
            .turn_environments()
            .map(|environment| environment.cwd().clone())
            .collect::<Vec<_>>();
        let history_item = ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "extension history".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        session
            .record_conversation_items(
                &turn,
                turn.model_info(),
                std::slice::from_ref(&history_item),
            )
            .await;
        let expected_history_item = strip_response_item_id(
            session
                .clone_history()
                .await
                .raw_items()
                .next()
                .expect("history item")
                .clone(),
        );
        let raw_history_event = rx.recv().await.expect("history raw response item event");
        let EventMsg::RawResponseItem(raw_history_item) = raw_history_event.msg else {
            panic!("expected raw response item event");
        };
        assert_eq!(
            strip_response_item_id(raw_history_item.item),
            expected_history_item
        );
        let mut step_context = StepContext::for_test(Arc::clone(&turn));
        let settings = Arc::make_mut(
            &mut Arc::get_mut(&mut step_context)
                .expect("unshared step")
                .settings,
        );
        let model_info = Arc::make_mut(&mut settings.model_info);
        model_info.node_repl_disabled = !turn.model_info().node_repl_disabled;
        let node_repl_disabled = model_info.node_repl_disabled;
        let cancellation_token = tokio_util::sync::CancellationToken::new();
        let invocation = ToolInvocation {
            session,
            step_context,
            turn,
            cancellation_token: cancellation_token.clone(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "call-extension".to_string(),
            tool_name: codex_tools::ToolName::plain("extension_echo"),
            source: ToolCallSource::CodeMode {
                cell_id: "cell-1".to_string(),
                runtime_tool_call_id: "nested-call-1".to_string(),
            },
            payload: ToolPayload::Function {
                arguments: json!({ "message": "hello" }).to_string(),
            },
        };

        crate::tools::registry::ToolExecutor::handle(&handler, invocation)
            .await
            .expect("extension call should succeed");

        let captured_call = captured_call.lock().await.clone().expect("captured call");
        assert!(weak_session.upgrade().is_none());
        assert!(weak_turn.upgrade().is_none());
        cancellation_token.cancel();
        assert!(captured_call.cancellation_token.is_cancelled());
        assert_eq!(captured_call.turn_id, turn_id);
        assert_eq!(captured_call.call_id, "call-extension");
        assert_eq!(
            captured_call.tool_name,
            codex_tools::ToolName::plain("extension_echo")
        );
        assert_eq!(captured_call.model, model);
        assert_eq!(captured_call.truncation_policy, truncation_policy);
        let metadata: serde_json::Value = serde_json::from_str(
            captured_call
                .codex_turn_metadata
                .as_deref()
                .expect("turn metadata"),
        )
        .expect("metadata JSON");
        assert_eq!(metadata["node_repl_disabled"], json!(node_repl_disabled));
        assert_eq!(
            captured_call.source,
            ExtensionToolCallSource::CodeMode {
                cell_id: "cell-1".to_string(),
                runtime_tool_call_id: "nested-call-1".to_string(),
            }
        );
        assert_eq!(*captured_sandbox_cwds.lock().await, expected_sandbox_cwds);
        assert_eq!(
            strip_response_item_ids(captured_call.conversation_history.items()),
            vec![expected_history_item]
        );
        match captured_call.payload {
            ToolPayload::Function { arguments } => {
                assert_eq!(arguments, json!({ "message": "hello" }).to_string());
            }
            payload => panic!("expected function payload, got {payload:?}"),
        }

        let started = rx.recv().await.expect("item started event");
        let EventMsg::ItemStarted(started) = started.msg else {
            panic!("expected item started event");
        };
        let TurnItem::Extension(ExtensionItem::WebSearch(started_item)) = started.item else {
            panic!("expected extension web search item");
        };
        assert_eq!(
            started_item,
            WebSearchItem {
                id: "call-extension".to_string(),
                query: String::new(),
                action: None,
                results: None,
            }
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn installed_semantic_navigation_through_adapter_preserves_authority_and_source_budget() {
        use std::os::unix::fs::PermissionsExt;

        use codex_extension_api::ExtensionData;
        use codex_extension_api::ExtensionRegistryBuilder;
        use codex_lsp_protocol::Location;
        use codex_lsp_protocol::MAX_REQUEST_BYTES;
        use codex_lsp_protocol::MAX_RESPONSE_BYTES;
        use codex_lsp_protocol::NavigationRequest;
        use codex_lsp_protocol::NavigationResponse;
        use codex_lsp_protocol::Operation;
        use codex_lsp_protocol::Position;
        use codex_lsp_protocol::Status;
        use codex_protocol::models::AdditionalPermissionProfile;
        use codex_protocol::models::FileSystemPermissions;
        use codex_protocol::models::PermissionProfile;
        use codex_protocol::models::PermissionProfileSnapshot;
        use codex_protocol::models::ResponseInputItem;
        use codex_protocol::openai_models::TruncationPolicyConfig;
        use codex_protocol::permissions::FileSystemAccessMode;
        use codex_protocol::permissions::FileSystemPath;
        use codex_protocol::permissions::FileSystemSandboxEntry;
        use codex_protocol::permissions::FileSystemSpecialPath;

        use crate::environment_selection::TurnEnvironmentState;

        for (source, expected_budget) in [
            (ToolCallSource::Direct, 480),
            (
                ToolCallSource::CodeMode {
                    cell_id: "navigation-cell".into(),
                    runtime_tool_call_id: "navigation-nested-call".into(),
                },
                MAX_RESPONSE_BYTES,
            ),
        ] {
            let directory = codex_uds::prepare_shared_daemon_socket_directory().unwrap();
            let socket = tempfile::Builder::new()
                .prefix("adapter-navigation-")
                .tempfile_in(directory)
                .unwrap()
                .into_temp_path();
            std::fs::remove_file(&socket).unwrap();
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
            listener.set_nonblocking(true).unwrap();

            // Resolve the real installed executor, rather than a capturing substitute.
            let mut builder = ExtensionRegistryBuilder::<()>::new();
            codex_semantic_navigation::install_with_socket(&mut builder, socket.to_path_buf());
            let registry = builder.build();
            let state = ExtensionData::new("adapter-navigation");
            let executor = registry
                .tool_contributors()
                .iter()
                .flat_map(|contributor| contributor.tools(&state, &state))
                .find(|executor| {
                    executor.tool_name() == codex_tools::ToolName::plain("semantic_navigation")
                })
                .expect("installed semantic navigation executor");
            let handler = ExtensionToolAdapter::new(executor);
            let (session, turn) = crate::session::tests::make_session_and_context().await;
            let session = Arc::new(session);
            let turn = Arc::new(turn);
            let mut step = StepContext::for_test(Arc::clone(&turn));
            let step_mut = Arc::get_mut(&mut step).unwrap();
            Arc::make_mut(&mut Arc::make_mut(&mut step_mut.settings).model_info)
                .truncation_policy = TruncationPolicyConfig::bytes(400);
            let workspace = tempfile::tempdir().unwrap();
            let cwd = workspace.path().canonicalize().unwrap();
            let mut environment = step_mut
                .environments
                .turn_environments()
                .next()
                .unwrap()
                .clone();
            environment.selection.cwd = PathUri::from_host_native_path(&cwd).unwrap();
            environment.config_mut().permission_profile =
                PermissionProfileSnapshot::legacy(PermissionProfile::default());
            let environment_id = environment.selection.environment_id.clone();
            step_mut.environments.environments =
                vec![TurnEnvironmentState::Ready(environment.clone())];

            let invocation = |step_context| ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn),
                step_context,
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "adapter-navigation".into(),
                tool_name: codex_tools::ToolName::plain("semantic_navigation"),
                source: source.clone(),
                payload: ToolPayload::Function {
                    arguments:
                        json!({"operation":"references","path":"saved.py","line":2,"column":3})
                            .to_string(),
                },
            };
            let full_read_grant = || AdditionalPermissionProfile {
                file_system: Some(FileSystemPermissions {
                    entries: vec![FileSystemSandboxEntry::new(
                        FileSystemPath::Special {
                            value: FileSystemSpecialPath::Root,
                        },
                        FileSystemAccessMode::Read,
                    )],
                    ..Default::default()
                }),
                ..Default::default()
            };

            // Real grant lookup must remain scoped to the selected executor id.
            turn.record_granted_permissions("other-environment", full_read_grant(), false);
            for reason in ["restricted_reads", "restricted_reads", "remote_environment"] {
                if reason == "remote_environment" {
                    let mut remote = environment.clone();
                    remote.environment = Arc::new(
                        codex_exec_server::Environment::create_for_tests(Some(
                            "ws://127.0.0.1:8765".into(),
                        ))
                        .unwrap(),
                    );
                    Arc::get_mut(&mut step).unwrap().environments.environments =
                        vec![TurnEnvironmentState::Ready(remote)];
                }
                let call = invocation(Arc::clone(&step));
                let payload = call.payload.clone();
                let output = crate::tools::registry::ToolExecutor::handle(&handler, call)
                    .await
                    .unwrap();
                assert_eq!(
                    output.code_mode_result(&payload),
                    serde_json::to_value(NavigationResponse::status(Status::Unavailable, reason))
                        .unwrap()
                );
                assert_eq!(
                    listener.accept().unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                // A literal filesystem path grant still cannot authorize the broader index.
                turn.record_granted_permissions(
                    &environment_id,
                    AdditionalPermissionProfile {
                        file_system: Some(FileSystemPermissions::from_read_write_path_uris(
                            Some(vec![PathUri::from_host_native_path("/").unwrap()]),
                            None,
                        )),
                        ..Default::default()
                    },
                    false,
                );
            }
            turn.record_granted_permissions(&environment_id, full_read_grant(), false);
            // A failed second selection remains part of the scope, even with only one ready executor.
            Arc::get_mut(&mut step).unwrap().environments.environments = vec![
                TurnEnvironmentState::Ready(environment.clone()),
                TurnEnvironmentState::Failed {
                    selection: environment.selection.clone(),
                    required_skills: Vec::new(),
                    error: "test unavailable environment".into(),
                },
            ];
            let call = invocation(Arc::clone(&step));
            let payload = call.payload.clone();
            let output = crate::tools::registry::ToolExecutor::handle(&handler, call)
                .await
                .unwrap();
            assert_eq!(
                output.code_mode_result(&payload)["reason"],
                "restricted_reads"
            );
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            Arc::get_mut(&mut step).unwrap().environments.environments =
                vec![TurnEnvironmentState::Ready(environment)];

            let location = |path: String| Location {
                path,
                start: Position { line: 2, column: 3 },
                end: Position { line: 2, column: 4 },
            };
            let short = location("saved.py".into());
            let escaped = location("a\\\"".repeat(300));
            let oversized = location("\\\"".repeat(513));
            let reply = serde_json::to_vec(&NavigationResponse::ok(vec![
                short.clone(),
                escaped.clone(),
                oversized,
            ]))
            .unwrap();
            assert!(reply.len() > 480 && reply.len() <= MAX_RESPONSE_BYTES);
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            let broker = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let bytes = codex_lsp_protocol::read_frame(&mut stream, MAX_REQUEST_BYTES)
                    .await
                    .unwrap();
                let request: NavigationRequest = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(request.version, 1);
                assert_eq!(request.cwd, cwd);
                assert_eq!(request.path, "saved.py");
                assert_eq!(request.operation, Operation::References);
                assert_eq!(request.position, Position { line: 2, column: 3 });
                assert_eq!(request.max_response_bytes, expected_budget);
                assert_eq!(request.max_results, codex_lsp_protocol::MAX_RESULTS);
                codex_lsp_protocol::write_frame(&mut stream, &reply, MAX_RESPONSE_BYTES)
                    .await
                    .unwrap();
            });
            let call = invocation(step);
            let payload = call.payload.clone();
            let output = crate::tools::registry::ToolExecutor::handle(&handler, call)
                .await
                .unwrap();
            let ResponseInputItem::FunctionCallOutput {
                output: response, ..
            } = output.to_response_item("adapter-navigation", &payload)
            else {
                panic!("function output")
            };
            let text = response.body.to_text().unwrap();
            assert!(text.len() <= expected_budget);
            let expected = NavigationResponse {
                status: Status::Ok,
                locations: if expected_budget == 480 {
                    vec![short]
                } else {
                    vec![short, escaped]
                },
                truncated: true,
                reason: None,
            };
            assert_eq!(
                serde_json::from_str::<NavigationResponse>(&text).unwrap(),
                expected
            );
            assert_eq!(
                output.code_mode_result(&payload),
                serde_json::to_value(expected).unwrap()
            );
            broker.await.unwrap();
        }
    }

    #[tokio::test]
    async fn image_generation_publication_preserves_extension_saved_path() {
        let (session, turn, rx) = crate::session::tests::make_session_and_context_with_rx().await;
        let expected_path = test_path_buf("/tmp/extension-claimed.png").abs();
        let emitter = CoreTurnItemEmitter {
            session: Arc::downgrade(&session),
            turn: Arc::downgrade(&turn),
        };
        let expected_started_item = ExtensionItem::ImageGeneration(ImageGenerationItem {
            id: "call-image".to_string(),
            status: "in_progress".to_string(),
            revised_prompt: None,
            image: GeneratedImageRef::Inline {
                result: String::new(),
            },
            transparent_background: None,
            failure: None,
            saved_path: None,
            input_images: Vec::new(),
            imagegen_request_id: None,
            generation_id: None,
        });
        let expected_completed_item = ExtensionItem::ImageGeneration(ImageGenerationItem {
            id: "call-image".to_string(),
            status: "completed".to_string(),
            revised_prompt: Some("A tiny blue square".to_string()),
            image: GeneratedImageRef::Inline {
                result: "cG5n".to_string(),
            },
            transparent_background: Some(true),
            failure: None,
            saved_path: Some(expected_path.clone()),
            input_images: Vec::new(),
            imagegen_request_id: None,
            generation_id: None,
        });
        codex_tools::TurnItemEmitter::emit_started(
            &emitter,
            ExtensionTurnItem {
                item: expected_started_item.clone(),
                legacy_events: vec![EventMsg::ImageGenerationBegin(ImageGenerationBeginEvent {
                    call_id: "call-image".to_string(),
                })],
            },
        )
        .await;
        codex_tools::TurnItemEmitter::emit_completed(
            &emitter,
            ExtensionTurnItem {
                item: expected_completed_item.clone(),
                legacy_events: vec![EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
                    call_id: "call-image".to_string(),
                    status: "completed".to_string(),
                    revised_prompt: Some("A tiny blue square".to_string()),
                    image: GeneratedImageRef::Inline {
                        result: "cG5n".to_string(),
                    },
                    transparent_background: Some(true),
                    failure: None,
                    saved_path: Some(expected_path.clone()),
                    input_images: Vec::new(),
                })],
            },
        )
        .await;

        let started = rx.recv().await.expect("item started event");
        let EventMsg::ItemStarted(started) = started.msg else {
            panic!("expected item started event");
        };
        let TurnItem::Extension(started_item) = started.item else {
            panic!("expected extension item");
        };
        let begin = rx.recv().await.expect("legacy image start event");
        assert!(matches!(begin.msg, EventMsg::ImageGenerationBegin(_)));
        let completed = rx.recv().await.expect("item completed event");
        let EventMsg::ItemCompleted(completed) = completed.msg else {
            panic!("expected item completed event");
        };
        let TurnItem::Extension(completed_item) = completed.item else {
            panic!("expected extension item");
        };
        let end = rx.recv().await.expect("legacy image end event");
        assert!(matches!(end.msg, EventMsg::ImageGenerationEnd(_)));

        assert_eq!(started_item, expected_started_item);
        assert_eq!(completed_item, expected_completed_item);
    }
}
