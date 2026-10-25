//! Native saved-file navigation. See the [design contract](../../../../.codex/design/nas-lsp-broker.md).

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolContributor;
use codex_lsp_protocol::DEFAULT_TIMEOUT_MS;
use codex_lsp_protocol::MAX_PATH_BYTES;
use codex_lsp_protocol::MAX_REQUEST_BYTES;
use codex_lsp_protocol::MAX_RESPONSE_BYTES;
use codex_lsp_protocol::MAX_RESULTS;
use codex_lsp_protocol::NavigationRequest;
use codex_lsp_protocol::NavigationResponse;
use codex_lsp_protocol::Operation;
use codex_lsp_protocol::PROTOCOL_VERSION;
use codex_lsp_protocol::Position;
use codex_lsp_protocol::Status;
use codex_lsp_protocol::fit_response;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_tools::JsonToolOutput;
use codex_tools::LocalNavigationAccess;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolExecutorFuture;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolPayload;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

const TOOL_NAME: &str = "semantic_navigation";

/// Snapshot the operator endpoint at host installation. No broker or worker is started.
pub fn install<C: Sync>(registry: &mut ExtensionRegistryBuilder<C>) {
    #[cfg(unix)]
    if let Ok(directory) = codex_uds::shared_daemon_socket_directory() {
        let configured = std::env::var_os("CODEX_SEMANTIC_NAVIGATION_SOCKET");
        let socket = configured
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| directory.join("semantic-navigation.sock"));
        if configured.is_some() || socket.exists() {
            install_with_socket(registry, socket);
        }
    }
}

/// Host-selected endpoint, also used by the native acceptance driver. It must remain
/// inside the reserved directory hidden by Codex sandboxes. Never a model argument.
pub fn install_with_socket<C: Sync>(registry: &mut ExtensionRegistryBuilder<C>, socket: PathBuf) {
    #[cfg(unix)]
    if endpoint_is_reserved(&socket) {
        registry.tool_contributor(Arc::new(NavigationTool {
            socket,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        }));
    }
    #[cfg(not(unix))]
    let _ = (registry, socket);
}

#[derive(Clone)]
struct NavigationTool {
    socket: PathBuf,
    timeout: Duration,
}

impl ToolContributor for NavigationTool {
    fn tools(
        &self,
        _session: &ExtensionData,
        _thread: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        vec![Arc::new(self.clone())]
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    operation: Operation,
    path: String,
    line: u32,
    column: u32,
}

impl<'call> ToolExecutor<ToolCall<'call>> for NavigationTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    #[expect(
        clippy::expect_used,
        reason = "The navigation input schema is a valid static JSON literal"
    )]
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_owned(),
            description: "Find saved-file definitions or references (including the declaration). Paths are relative to the registered workspace; line and column are 1-based Unicode characters. Returns locations and an explicit availability/freshness status. If unavailable, continue with bounded search and file reads.".to_owned(),
            strict: true,
            parameters: codex_tools::parse_tool_input_schema(&json!({
                "type": "object",
                "properties": {
                    "operation": {"type": "string", "enum": ["definition", "references"]},
                    "path": {"type": "string"},
                    "line": {"type": "integer", "minimum": 1},
                    "column": {"type": "integer", "minimum": 1},
                },
                "required": ["operation", "path", "line", "column"],
                "additionalProperties": false,
            }))
            .expect("static navigation schema"),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let budget = call.response_byte_budget(MAX_RESPONSE_BYTES);
            let response = tokio::select! {
                biased;
                _ = call.cancellation_token.cancelled() => status(Status::Cancelled, "cancelled"),
                result = tokio::time::timeout(self.timeout, self.query(&call, budget)) => {
                    result.unwrap_or_else(|_| status(Status::Timeout, "deadline_exceeded"))
                }
            };
            Ok(bounded_output(response, budget))
        })
    }
}

impl NavigationTool {
    async fn query(&self, call: &ToolCall<'_>, budget: usize) -> NavigationResponse {
        // Authorization precedes filesystem access and all socket inspection/connection.
        let [environment] = call.environments.as_slice() else {
            return status(Status::Unavailable, "ambiguous_environment");
        };
        match environment.local_navigation_access() {
            LocalNavigationAccess::Remote => {
                return status(Status::Unavailable, "remote_environment");
            }
            LocalNavigationAccess::Restricted => {
                return status(Status::Unavailable, "restricted_reads");
            }
            LocalNavigationAccess::Allowed => {}
        }
        let Ok(arguments) = call.function_arguments() else {
            return status(Status::InvalidRequest, "invalid_arguments");
        };
        if arguments.len() > MAX_REQUEST_BYTES {
            return status(Status::InvalidRequest, "invalid_arguments");
        }
        let Ok(args) = serde_json::from_str::<Arguments>(arguments) else {
            return status(Status::InvalidRequest, "invalid_arguments");
        };
        if args.line == 0 || args.column == 0 || !valid_relative_path(&args.path) {
            return status(Status::InvalidRequest, "invalid_arguments");
        }
        let Ok(cwd) = environment.fs().canonicalize(&environment.cwd).await else {
            return status(Status::Unavailable, "cwd_unavailable");
        };
        // Only a host-approved local environment may convert its canonical URI to a native path.
        let Ok(cwd) = cwd.to_abs_path() else {
            return status(Status::Unavailable, "cwd_unavailable");
        };
        let request = NavigationRequest {
            version: PROTOCOL_VERSION,
            cwd: cwd.to_path_buf(),
            path: args.path,
            operation: args.operation,
            position: Position {
                line: args.line,
                column: args.column,
            },
            max_results: MAX_RESULTS,
            max_response_bytes: budget,
            timeout_ms: self.timeout.as_millis().min(DEFAULT_TIMEOUT_MS as u128) as u64,
        };
        self.exchange(request).await
    }

    #[cfg(unix)]
    async fn exchange(&self, request: NavigationRequest) -> NavigationResponse {
        if !secure_endpoint(&self.socket).await {
            return status(Status::Unavailable, "unsafe_or_missing_endpoint");
        }
        let Ok(mut stream) = tokio::net::UnixStream::connect(&self.socket).await else {
            return status(Status::Unavailable, "broker_unavailable");
        };
        #[cfg(target_os = "linux")]
        if !stream
            .peer_cred()
            .is_ok_and(|peer| peer.uid() == unsafe { libc::geteuid() })
        {
            return status(Status::Unavailable, "invalid_peer");
        }
        let Ok(bytes) = serde_json::to_vec(&request) else {
            return status(Status::InvalidRequest, "invalid_request");
        };
        if codex_lsp_protocol::write_frame(&mut stream, &bytes, MAX_REQUEST_BYTES)
            .await
            .is_err()
        {
            return status(Status::Unavailable, "broker_write_failed");
        }
        let Ok(bytes) = codex_lsp_protocol::read_frame(&mut stream, MAX_RESPONSE_BYTES).await
        else {
            return status(Status::Unavailable, "invalid_broker_frame");
        };
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| status(Status::Unavailable, "invalid_broker_response"))
    }

    #[cfg(not(unix))]
    async fn exchange(&self, _request: NavigationRequest) -> NavigationResponse {
        status(Status::Unavailable, "unsupported_platform")
    }
}

fn status(status: Status, reason: &str) -> NavigationResponse {
    NavigationResponse::status(status, reason)
}

fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.chars().any(char::is_control)
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

#[cfg(unix)]
fn endpoint_is_reserved(socket: &Path) -> bool {
    codex_uds::shared_daemon_socket_directory().is_ok_and(|directory| {
        socket.parent() == Some(directory.as_path())
            && socket.file_name().is_some()
            && socket
                .components()
                .all(|part| !matches!(part, Component::ParentDir | Component::CurDir))
    })
}

#[cfg(unix)]
async fn secure_endpoint(socket: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    if !endpoint_is_reserved(socket) {
        return false;
    }
    let Some(parent) = socket.parent() else {
        return false;
    };
    let Ok(directory) = tokio::fs::symlink_metadata(parent).await else {
        return false;
    };
    let uid = unsafe { libc::geteuid() };
    if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o777 != 0o700 {
        return false;
    }
    let Ok(metadata) = tokio::fs::symlink_metadata(socket).await else {
        return false;
    };
    metadata.file_type().is_socket() && metadata.uid() == uid && metadata.mode() & 0o777 == 0o600
}

fn bounded_output(response: NavigationResponse, budget: usize) -> Box<dyn ToolOutput> {
    let success = response.status == Status::Ok;
    match fit_response(response, budget) {
        Ok(response) => Box::new(JsonToolOutput::with_success(json!(response), Some(success))),
        Err(_) if budget >= 4 => Box::new(JsonToolOutput::with_success(
            serde_json::Value::Null,
            Some(false),
        )),
        Err(_) if budget >= 1 => Box::new(JsonToolOutput::with_success(json!(0), Some(false))),
        Err(_) => Box::new(EmptyOutput),
    }
}

/// A JSON empty string would take two bytes; a zero-byte allowance needs raw text.
struct EmptyOutput;
impl ToolOutput for EmptyOutput {
    fn log_output(&self) -> String {
        String::new()
    }
    fn success_for_logging(&self) -> bool {
        false
    }
    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_owned(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(String::new()),
                success: Some(false),
            },
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
