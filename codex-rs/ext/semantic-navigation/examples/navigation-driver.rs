//! Native executor acceptance driver. See the [design](../../../../.codex/design/nas-lsp-broker.md).
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use codex_exec_server::LocalFileSystem;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FileSystemEnvironmentAccessor;
use codex_lsp_protocol::MAX_RESPONSE_BYTES;
use codex_lsp_protocol::Operation;
use codex_lsp_protocol::Position;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::TruncationPolicy;
use codex_tools::LocalNavigationAccess;
use codex_tools::ToolCall;
use codex_tools::ToolCallSource;
use codex_tools::ToolEnvironment;
use codex_tools::ToolPayload;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    socket_path: PathBuf,
    cwd: PathBuf,
    path: String,
    operation: Operation,
    position: Position,
    source: String,
    budget: usize,
    access: String,
    cancel_after_ms: Option<u64>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    std::io::stdin().take(65_537).read_to_string(&mut input)?;
    let request: Request = serde_json::from_str(&input)?;
    let mut builder = ExtensionRegistryBuilder::<()>::new();
    codex_semantic_navigation::install_with_socket(&mut builder, request.socket_path);
    let registry = builder.build();
    let state = ExtensionData::new("driver");
    let tools = registry
        .tool_contributors()
        .iter()
        .flat_map(|c| c.tools(&state, &state))
        .collect::<Vec<_>>();
    let tool = tools
        .first()
        .ok_or("socket must be within the reserved daemon directory")?;
    let filesystem: Arc<dyn ExecutorFileSystem> = Arc::new(LocalFileSystem::unsandboxed());
    let accessor = FileSystemEnvironmentAccessor::unrestricted(&filesystem);
    let access = match request.access.as_str() {
        "allowed" => LocalNavigationAccess::Allowed,
        "remote" => LocalNavigationAccess::Remote,
        "restricted" => LocalNavigationAccess::Restricted,
        _ => return Err("unknown access".into()),
    };
    let source = match request.source.as_str() {
        "direct" => ToolCallSource::Direct,
        "code_mode" => ToolCallSource::CodeMode {
            cell_id: "driver".to_owned(),
            runtime_tool_call_id: "driver".to_owned(),
        },
        _ => return Err("unknown source".into()),
    };
    let cancellation_token = CancellationToken::new();
    if let Some(delay) = request.cancel_after_ms {
        let token = cancellation_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            token.cancel();
        });
    }
    let call = ToolCall {
        turn_id: "driver".to_owned(), call_id: "driver".to_owned(), tool_name: tool.tool_name(),
        model: "acceptance".to_owned(), codex_turn_metadata: None,
        truncation_policy: TruncationPolicy::Bytes(request.budget), source, cancellation_token,
        conversation_history: Default::default(), turn_item_emitter: Arc::new(codex_tools::NoopTurnItemEmitter),
        environments: vec![ToolEnvironment::new("driver".to_owned(), PathUri::from_host_native_path(request.cwd)?, &accessor).with_local_navigation_access(access)],
        payload: ToolPayload::Function { arguments: json!({ "operation":request.operation, "path":request.path, "line":request.position.line, "column":request.position.column }).to_string() },
    };
    let effective_response_budget = call.response_byte_budget(MAX_RESPONSE_BYTES);
    let payload = call.payload.clone();
    let started = Instant::now();
    let output = tool.handle(call).await.map_err(|_| "executor failed")?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let ResponseInputItem::FunctionCallOutput {
        output: response, ..
    } = output.to_response_item("driver", &payload)
    else {
        return Err("unexpected output".into());
    };
    let content = response.body.to_text().unwrap_or_default();
    println!(
        "{}",
        json!({ "content":content, "serialized_bytes":content.len(), "elapsed_ms":elapsed_ms, "effective_response_budget":effective_response_budget, "output":output.code_mode_result(&payload) })
    );
    Ok(())
}
