//! Bounded observational hook payloads for Code Mode control calls.
//!
//! Hooks observe `exec` cells and `wait` calls without gating them; see
//! `CoreToolRuntime::observational_pre_tool_use_payload`.

use serde_json::Value;
use serde_json::json;
use sha1::Digest;
use sha1::Sha1;

use crate::tools::context::ToolInvocation;
use crate::tools::flat_tool_name;
use crate::tools::hook_names::HookToolName;

pub(super) fn code_mode_hook_tool_name(invocation: &ToolInvocation) -> HookToolName {
    HookToolName::new(flat_tool_name(&invocation.tool_name).into_owned())
}

/// Hook view of an `exec` cell: source size and digest plus pragma limits,
/// never the source itself. The digest lets observers spot a repeated cell.
pub(super) fn exec_observation_input(source: &str) -> Value {
    let mut input = json!({
        "source_bytes": source.len(),
        "source_sha1": format!("{:x}", Sha1::digest(source.as_bytes())),
    });
    if let (Ok(parsed), Value::Object(fields)) =
        (codex_code_mode::parse_exec_source(source), &mut input)
    {
        if let Some(yield_time_ms) = parsed.yield_time_ms {
            fields.insert("yield_time_ms".to_string(), json!(yield_time_ms));
        }
        if let Some(max_output_tokens) = parsed.max_output_tokens {
            fields.insert("max_output_tokens".to_string(), json!(max_output_tokens));
        }
    }
    input
}

#[cfg(test)]
mod tests {
    use super::exec_observation_input;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn exec_observation_input_hashes_source_and_keeps_pragma_limits() {
        let source =
            "// @exec: {\"yield_time_ms\": 10000, \"max_output_tokens\": 500}\ntext(\"hi\");\n";

        let input = exec_observation_input(source);

        assert_eq!(
            input,
            json!({
                "source_bytes": source.len(),
                "source_sha1": input["source_sha1"].clone(),
                "yield_time_ms": 10_000,
                "max_output_tokens": 500,
            })
        );
        assert_eq!(input["source_sha1"].as_str().map(str::len), Some(40));
        assert_eq!(exec_observation_input(source), input);
        assert!(!input.to_string().contains("text("));
    }

    #[test]
    fn exec_observation_input_without_pragma_has_no_limits() {
        let input = exec_observation_input("text(\"a\");");

        assert_eq!(input["source_bytes"], 10);
        assert_eq!(input.get("yield_time_ms"), None);
        assert_ne!(input, exec_observation_input("text(\"b\");"));
    }
}
