//! Bounded grammar for exact host observations. Raw arguments/results are never retained.

use codex_extension_api::ToolDispatchInput;
use codex_extension_api::ToolStartInput;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AgentStatus;
use codex_tools::ToolCallSource;
use codex_tools::ToolPayload;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;

pub(super) type Fingerprint = [u8; 32];
pub(super) const MAX_BYTES: usize = 8192;
const MAX_ID_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Check {
    Process(i32),
    Agents(Fingerprint),
}

pub(super) enum Kind {
    Check(Check),
    Wrapper { origin: Fingerprint, timer: bool },
    Unknown,
}

#[derive(Clone, Copy)]
pub(super) enum Intent {
    Status,
    NestedStatus,
    Wrapper,
    Unknown,
}

pub(super) fn intent(input: &ToolDispatchInput<'_>) -> Intent {
    match (
        input.tool_name.namespace.as_deref(),
        input.tool_name.name.as_str(),
    ) {
        (None | Some("") | Some("functions"), "exec")
            if matches!(input.source, ToolCallSource::Direct) =>
        {
            Intent::Wrapper
        }
        (None | Some("") | Some("functions"), "write_stdin")
        | (None | Some("") | Some("functions") | Some("collaboration"), "list_agents") => {
            match input.source {
                ToolCallSource::Direct => Intent::Status,
                ToolCallSource::CodeMode { .. } => Intent::NestedStatus,
            }
        }
        _ => Intent::Unknown,
    }
}

pub(super) struct Start {
    pub key: Option<Fingerprint>,
    pub kind: Kind,
    pub nested: Option<(Fingerprint, Fingerprint)>,
}

pub(super) fn digest(bytes: &[u8]) -> Fingerprint {
    Sha256::digest(bytes).into()
}

pub(super) fn call_key(call_id: &str, source: &ToolCallSource) -> Option<Fingerprint> {
    if call_id.is_empty() || call_id.len() > MAX_ID_BYTES {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(call_id.len().to_le_bytes());
    hash.update(call_id.as_bytes());
    match source {
        ToolCallSource::Direct => hash.update([0]),
        ToolCallSource::CodeMode {
            cell_id,
            runtime_tool_call_id,
        } => {
            if cell_id.is_empty()
                || cell_id.len() > MAX_ID_BYTES
                || runtime_tool_call_id.is_empty()
                || runtime_tool_call_id.len() > MAX_ID_BYTES
            {
                return None;
            }
            hash.update([1]);
            hash.update(cell_id.len().to_le_bytes());
            hash.update(cell_id.as_bytes());
            hash.update([0]);
            hash.update(runtime_tool_call_id.as_bytes());
        }
    }
    Some(hash.finalize().into())
}

pub(super) fn start(input: &ToolStartInput<'_>) -> Start {
    let origin = input
        .originating_item_id
        .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
        .map(|id| digest(id.as_bytes()));
    let nested = match &input.source {
        ToolCallSource::Direct => None,
        ToolCallSource::CodeMode { cell_id, .. } if cell_id.len() <= MAX_ID_BYTES => {
            origin.map(|origin| (origin, digest(cell_id.as_bytes())))
        }
        ToolCallSource::CodeMode { .. } => None,
    };
    let trusted_namespace = input.tool_name.is_default_namespace()
        || (input.tool_name.namespace.as_deref() == Some("collaboration")
            && input.tool_name.name == "list_agents");
    let source_known = matches!(input.source, ToolCallSource::Direct) || nested.is_some();
    let kind = if input.mcp_tool.is_none() && trusted_namespace && source_known {
        match (input.tool_name.name.as_str(), input.payload) {
            ("exec", ToolPayload::Custom { input: code })
                if code.len() <= MAX_BYTES && nested.is_none() =>
            {
                origin
                    .map(|origin| Kind::Wrapper {
                        origin,
                        timer: timer_only(code),
                    })
                    .unwrap_or(Kind::Unknown)
            }
            (name, ToolPayload::Function { arguments }) if arguments.len() <= MAX_BYTES => {
                check(name, arguments)
                    .map(Kind::Check)
                    .unwrap_or(Kind::Unknown)
            }
            _ => Kind::Unknown,
        }
    } else {
        Kind::Unknown
    };
    Start {
        key: call_key(input.call_id, &input.source),
        kind,
        nested,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessArgs {
    session_id: i32,
    #[serde(default)]
    chars: String,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentArgs {
    path_prefix: Option<String>,
}

pub(super) fn check(name: &str, arguments: &str) -> Option<Check> {
    if arguments.len() > MAX_BYTES {
        return None;
    }
    match name {
        "write_stdin" => {
            let args: ProcessArgs = serde_json::from_str(arguments).ok()?;
            // Read-only checks with bounded waits; a single long helper is not evidence.
            (args.chars.is_empty()
                && args.yield_time_ms.unwrap_or(5000) <= 60_000
                && args.max_output_tokens.is_none_or(|limit| limit > 0))
            .then_some(Check::Process(args.session_id))
        }
        "list_agents" => {
            let args: AgentArgs = serde_json::from_str(arguments).ok()?;
            let prefix = args.path_prefix.unwrap_or_default();
            (prefix.len() <= MAX_ID_BYTES).then(|| Check::Agents(digest(prefix.as_bytes())))
        }
        _ => None,
    }
}

// Deliberately one exact timer-only grammar, not a JavaScript classifier.
fn timer_only(code: &str) -> bool {
    code.trim()
        .strip_prefix("await new Promise(resolve => setTimeout(resolve, ")
        .and_then(|tail| tail.strip_suffix("));"))
        .filter(|duration| {
            !duration.is_empty() && duration.bytes().all(|byte| byte.is_ascii_digit())
        })
        .and_then(|duration| duration.parse::<u32>().ok())
        .is_some_and(|duration| duration <= 60_000)
}

pub(super) fn output_text(output: &ResponseInputItem) -> Option<&str> {
    let ResponseInputItem::FunctionCallOutput { output, .. } = output else {
        return None;
    };
    if output.success != Some(true) {
        return None;
    }
    let text = match &output.body {
        FunctionCallOutputBody::Text(text) => text,
        FunctionCallOutputBody::ContentItems(items) => match items.as_slice() {
            [FunctionCallOutputContentItem::InputText { text }] => text,
            _ => return None,
        },
    };
    (text.len() <= MAX_BYTES).then_some(text.as_str())
}

pub(super) fn outcome(check: Check, output: &ResponseInputItem) -> Option<Fingerprint> {
    let text = output_text(output)?;
    match check {
        Check::Process(session) => {
            let mut lines = text.lines();
            let chunk = lines.next()?.strip_prefix("Chunk ID: ")?;
            if chunk.is_empty() || chunk.len() > MAX_ID_BYTES {
                return None;
            }
            let duration = lines
                .next()?
                .strip_prefix("Wall time: ")?
                .strip_suffix(" seconds")?
                .parse::<f64>()
                .ok()?;
            if !duration.is_finite() || duration < 0.0 {
                return None;
            }
            let target = lines
                .next()?
                .strip_prefix("Process running with session ID ")?
                .parse::<i32>()
                .ok()?;
            if target != session {
                return None;
            }
            let line = lines.next()?;
            let output_label = if let Some(count) = line.strip_prefix("Original token count: ") {
                if count != "0" {
                    return None;
                }
                lines.next()?
            } else {
                line
            };
            // Any actual output (including truncation notices) or exit resets the episode.
            (output_label == "Output:" && lines.next().is_none()).then(|| digest(b"running-empty"))
        }
        Check::Agents(_) => {
            let value: serde_json::Value = serde_json::from_str(text).ok()?;
            let object = value.as_object()?;
            if object.len() != 1 {
                return None;
            }
            let agents = object.get("agents")?.as_array()?;
            if agents.len() > 32 {
                return None;
            }
            let mut lifecycle = Vec::with_capacity(agents.len());
            for agent in agents {
                let object = agent.as_object()?;
                if object.len() != 3 || !object.contains_key("context") {
                    return None;
                }
                let name = object.get("agent_name")?.as_str()?;
                if name.is_empty() || name.len() > MAX_ID_BYTES {
                    return None;
                }
                let _: AgentStatus =
                    serde_json::from_value(object.get("agent_status")?.clone()).ok()?;
                if !object["context"].is_null() && !object["context"].is_object() {
                    return None;
                }
                lifecycle.push((name, &object["agent_status"]));
            }
            // Context token estimates/timestamps advance merely from checking status.
            // Compare structured lifecycle, preserving completion/error content.
            lifecycle.sort_unstable_by_key(|(name, _)| *name);
            if lifecycle.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return None;
            }
            serde_json::to_vec(&lifecycle)
                .ok()
                .map(|bytes| digest(&bytes))
        }
    }
}
