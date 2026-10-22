//! Native executor safety tests. See the [design](../../../../.codex/design/nas-lsp-broker.md).
use super::*;
use codex_exec_server::LocalFileSystem;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FileSystemEnvironmentAccessor;
use codex_lsp_protocol::Location;
use codex_protocol::protocol::TruncationPolicy;
use codex_tools::ToolCallSource;
use codex_tools::ToolEnvironment;
use codex_utils_path_uri::PathUri;
use std::os::unix::fs::PermissionsExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

fn socket() -> (PathBuf, tempfile::TempPath) {
    let directory =
        codex_uds::prepare_shared_daemon_socket_directory().expect("reserved directory");
    let path = tempfile::Builder::new()
        .prefix("navigation-test-")
        .tempfile_in(directory)
        .expect("socket name")
        .into_temp_path();
    std::fs::remove_file(&path).expect("remove name placeholder");
    (path.to_path_buf(), path)
}

fn call<'a>(
    fs: &'a dyn codex_file_system::EnvironmentAccess,
    access: LocalNavigationAccess,
    budget: usize,
    source: ToolCallSource,
) -> ToolCall<'a> {
    ToolCall {
        turn_id: "test".into(),
        call_id: "test".into(),
        tool_name: ToolName::plain(TOOL_NAME),
        model: "test".into(),
        codex_turn_metadata: None,
        truncation_policy: TruncationPolicy::Bytes(budget),
        source,
        cancellation_token: CancellationToken::new(),
        conversation_history: Default::default(),
        turn_item_emitter: Arc::new(codex_tools::NoopTurnItemEmitter),
        environments: vec![
            ToolEnvironment::new(
                "local-looking-id".into(),
                PathUri::from_host_native_path("/does-not-exist-for-navigation").expect("URI"),
                fs,
            )
            .with_local_navigation_access(access),
        ],
        payload: ToolPayload::Function {
            arguments: json!({"operation":"references","path":"test.py","line":1,"column":1})
                .to_string(),
        },
    }
}

fn content(output: &dyn ToolOutput, payload: &ToolPayload) -> String {
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item("test", payload)
    else {
        panic!("function output");
    };
    output.body.to_text().expect("text")
}

fn code_mode() -> ToolCallSource {
    ToolCallSource::CodeMode {
        cell_id: "cell".into(),
        runtime_tool_call_id: "call".into(),
    }
}

#[tokio::test]
async fn default_authority_is_restricted_and_remote_restricted_never_connect() {
    let filesystem: Arc<dyn ExecutorFileSystem> = Arc::new(LocalFileSystem::unsandboxed());
    let accessor = FileSystemEnvironmentAccessor::unrestricted(&filesystem);
    assert_eq!(
        ToolEnvironment::new(
            "local".into(),
            PathUri::from_host_native_path("/").unwrap(),
            &accessor
        )
        .local_navigation_access(),
        LocalNavigationAccess::Restricted
    );
    let (path, _guard) = socket();
    let listener = UnixListener::bind(&path).unwrap();
    let tool = NavigationTool {
        socket: path,
        timeout: Duration::from_secs(1),
    };
    for (access, reason) in [
        (LocalNavigationAccess::Remote, "remote_environment"),
        (LocalNavigationAccess::Restricted, "restricted_reads"),
    ] {
        let call = call(
            &accessor,
            access,
            MAX_RESPONSE_BYTES,
            ToolCallSource::Direct,
        );
        let payload = call.payload.clone();
        let output = tool.handle(call).await.unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&content(output.as_ref(), &payload)).unwrap();
        // Nonexistent cwd proves even canonicalization was skipped.
        assert_eq!(value["reason"], reason);
    }
    let cancelled = call(
        &accessor,
        LocalNavigationAccess::Allowed,
        MAX_RESPONSE_BYTES,
        ToolCallSource::Direct,
    );
    cancelled.cancellation_token.cancel();
    let payload = cancelled.payload.clone();
    let output = tool.handle(cancelled).await.unwrap();
    let value: NavigationResponse =
        serde_json::from_str(&content(output.as_ref(), &payload)).unwrap();
    assert_eq!(value.status, Status::Cancelled);
    for count in [0, 2] {
        let mut ambiguous = call(
            &accessor,
            LocalNavigationAccess::Allowed,
            MAX_RESPONSE_BYTES,
            ToolCallSource::Direct,
        );
        ambiguous.environments = vec![ambiguous.environments[0].clone(); count];
        let payload = ambiguous.payload.clone();
        let output = tool.handle(ambiguous).await.unwrap();
        let value: NavigationResponse =
            serde_json::from_str(&content(output.as_ref(), &payload)).unwrap();
        assert_eq!(value.reason.as_deref(), Some("ambiguous_environment"));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn malformed_arguments_and_tiny_direct_budgets_are_fitted() {
    let filesystem: Arc<dyn ExecutorFileSystem> = Arc::new(LocalFileSystem::unsandboxed());
    let accessor = FileSystemEnvironmentAccessor::unrestricted(&filesystem);
    let tool = NavigationTool {
        socket: PathBuf::from("/outside"),
        timeout: Duration::from_secs(1),
    };
    for raw_budget in [0, 1, 2, 3, 4, 32, 256] {
        for arguments in [
            "{malformed",
            "{}",
            r#"{"operation":"definition","path":"../escape","line":1,"column":1}"#,
            r#"{"operation":"definition","path":"test.py","line":0,"column":1}"#,
            r#"{"operation":"definition","path":"test.py","line":1,"column":1,"cwd":"/"}"#,
        ] {
            let mut call = call(
                &accessor,
                LocalNavigationAccess::Allowed,
                raw_budget,
                ToolCallSource::Direct,
            );
            call.payload = ToolPayload::Function {
                arguments: arguments.to_owned(),
            };
            let budget = call.response_byte_budget(MAX_RESPONSE_BYTES);
            let payload = call.payload.clone();
            let output = tool.handle(call).await.unwrap();
            let bytes = content(output.as_ref(), &payload);
            assert!(bytes.len() <= budget);
            if budget == 0 {
                assert_eq!(bytes, "");
            } else if budget < 4 {
                assert_eq!(bytes, "0");
            } else if budget < 32 {
                assert_eq!(bytes, "null");
            } else if budget >= 256 {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&bytes).unwrap()["status"],
                    "invalid_request"
                );
            }
        }
    }
}

#[test]
fn complete_serialization_and_count_fit_for_direct_and_code_mode() {
    let filesystem: Arc<dyn ExecutorFileSystem> = Arc::new(LocalFileSystem::unsandboxed());
    let accessor = FileSystemEnvironmentAccessor::unrestricted(&filesystem);
    let location = Location {
        path: "escaped\\\"".repeat(100),
        start: Position { line: 1, column: 1 },
        end: Position { line: 1, column: 2 },
    };
    for source in [ToolCallSource::Direct, code_mode()] {
        let call = call(&accessor, LocalNavigationAccess::Allowed, 1024, source);
        let budget = call.response_byte_budget(MAX_RESPONSE_BYTES);
        let output = bounded_output(NavigationResponse::ok(vec![location.clone(); 200]), budget);
        let bytes = content(output.as_ref(), &call.payload);
        assert!(bytes.len() <= budget);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&bytes).unwrap(),
            output.code_mode_result(&call.payload)
        );
        let value: NavigationResponse = serde_json::from_str(&bytes).unwrap();
        assert!(value.truncated);
        assert!(value.locations.len() <= MAX_RESULTS);
    }
    assert!(!endpoint_is_reserved(Path::new("/tmp/arbitrary.sock")));
}

#[tokio::test]
async fn installed_executor_handles_real_frames_and_disappearing_socket() {
    for source in [ToolCallSource::Direct, code_mode()] {
        let (path, _guard) = socket();
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let bytes = codex_lsp_protocol::read_frame(&mut stream, MAX_REQUEST_BYTES)
                .await
                .unwrap();
            let request: NavigationRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request.cwd, PathBuf::from("/"));
            assert_eq!(request.operation, Operation::References);
            let location = Location {
                path: "x".into(),
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            };
            let bytes = serde_json::to_vec(&NavigationResponse::ok(vec![location; 150])).unwrap();
            codex_lsp_protocol::write_frame(&mut stream, &bytes, MAX_RESPONSE_BYTES)
                .await
                .unwrap();
        });
        let mut builder = ExtensionRegistryBuilder::<()>::new();
        install_with_socket(&mut builder, path.clone());
        let registry = builder.build();
        let state = ExtensionData::new("test");
        let tools = registry.tool_contributors()[0].tools(&state, &state);
        let filesystem: Arc<dyn ExecutorFileSystem> = Arc::new(LocalFileSystem::unsandboxed());
        let accessor = FileSystemEnvironmentAccessor::unrestricted(&filesystem);
        let mut call = call(&accessor, LocalNavigationAccess::Allowed, 1024, source);
        call.environments[0].cwd = PathUri::from_host_native_path("/").unwrap();
        let budget = call.response_byte_budget(MAX_RESPONSE_BYTES);
        let payload = call.payload.clone();
        let output = tools[0].handle(call.clone()).await.unwrap();
        let bytes = content(output.as_ref(), &payload);
        assert!(bytes.len() <= budget);
        let value: NavigationResponse = serde_json::from_str(&bytes).unwrap();
        assert_eq!(value.status, Status::Ok);
        assert!(value.truncated);
        assert!(value.locations.len() <= MAX_RESULTS);
        server.await.unwrap();
        std::fs::remove_file(&path).unwrap();
        let output = tools[0].handle(call).await.unwrap();
        let value: NavigationResponse =
            serde_json::from_str(&content(output.as_ref(), &payload)).unwrap();
        assert_eq!(value.status, Status::Unavailable);
    }
}

#[tokio::test]
async fn oversized_malformed_replies_timeout_and_cancellation_are_bounded() {
    for source in [ToolCallSource::Direct, code_mode()] {
        for mode in ["oversized", "malformed", "timeout", "cancelled"] {
            let (path, _guard) = socket();
            let listener = UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = codex_lsp_protocol::read_frame(&mut stream, MAX_REQUEST_BYTES)
                    .await
                    .unwrap();
                match mode {
                    "oversized" => stream
                        .write_u32(MAX_RESPONSE_BYTES as u32 + 1)
                        .await
                        .unwrap(),
                    "malformed" => codex_lsp_protocol::write_frame(
                        &mut stream,
                        b"{invalid",
                        MAX_RESPONSE_BYTES,
                    )
                    .await
                    .unwrap(),
                    _ => {
                        let mut bytes = [0];
                        assert_eq!(
                            tokio::io::AsyncReadExt::read(&mut stream, &mut bytes)
                                .await
                                .unwrap(),
                            0
                        );
                    }
                }
            });
            let tool = NavigationTool {
                socket: path,
                timeout: Duration::from_millis(100),
            };
            let filesystem: Arc<dyn ExecutorFileSystem> = Arc::new(LocalFileSystem::unsandboxed());
            let accessor = FileSystemEnvironmentAccessor::unrestricted(&filesystem);
            let mut call = call(
                &accessor,
                LocalNavigationAccess::Allowed,
                1024,
                source.clone(),
            );
            call.environments[0].cwd = PathUri::from_host_native_path("/").unwrap();
            if mode == "cancelled" {
                let token = call.cancellation_token.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    token.cancel();
                });
            }
            let payload = call.payload.clone();
            let start = std::time::Instant::now();
            let output = tool.handle(call).await.unwrap();
            assert!(start.elapsed() < Duration::from_millis(500));
            let bytes = content(output.as_ref(), &payload);
            assert!(bytes.len() <= 1229);
            let value: NavigationResponse = serde_json::from_str(&bytes).unwrap();
            assert_eq!(
                value.status,
                match mode {
                    "timeout" => Status::Timeout,
                    "cancelled" => Status::Cancelled,
                    _ => Status::Unavailable,
                }
            );
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn rejects_socket_symlinks_and_unsafe_modes_without_connecting() {
    let (path, _guard) = socket();
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(!secure_endpoint(&path).await);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(secure_endpoint(&path).await);
    let (alias, _alias_guard) = socket();
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    assert!(!secure_endpoint(&alias).await);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}
