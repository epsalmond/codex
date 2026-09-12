use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(super) const READ_ARTIFACT_TOOL_NAME: &str = "read_artifact";

pub(super) fn create_read_artifact_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "artifact".to_string(),
        JsonSchema::string(Some(
            "The artifact:// URI from a shaken history placeholder.".to_string(),
        )),
    );
    properties.insert(
        "start_byte".to_string(),
        JsonSchema::integer(Some(
            "Byte offset for continuing a bounded page; omit it for the first page.".to_string(),
        )),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: READ_ARTIFACT_TOOL_NAME.to_string(),
        description: "Recover a bounded UTF-8 text page from an artifact:// URI left by /shake. If the result contains a start_byte continuation, call this tool again with that offset. The placeholder also carries the artifact's absolute file path, which can be searched with shell tools (grep, awk, ...) instead of this tool when only part of the output is needed.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["artifact".to_string()]),
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: None,
    })
}
