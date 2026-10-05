//! Borrowed host callbacks translated into the shared, payload-free observer.

use std::collections::BTreeMap;

use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolOutputInput;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolStartInput;
use codex_protocol::models::ResponseInputItem;
use serde_json::Value;

use crate::stall::Digest;
use crate::stall::digest;
use crate::stall_observation::CallKind;
use crate::stall_observation::CallParent;
use crate::stall_observation::TurnObserver;

#[derive(Default)]
pub(crate) struct LiveTurn {
    pub(crate) observer: TurnObserver,
    wrappers: BTreeMap<Digest, String>,
}

impl LiveTurn {
    pub(crate) fn new(settings: crate::stall_settings::StallSettings) -> Self {
        Self {
            observer: TurnObserver::new(settings),
            wrappers: BTreeMap::new(),
        }
    }
    pub(crate) fn start(&mut self, input: &ToolStartInput<'_>) {
        let name = input.tool_name.to_string();
        let kind = if input.tool_name.name == "exec"
            && input
                .tool_name
                .namespace
                .as_deref()
                .is_none_or(|namespace| namespace == "functions")
        {
            CallKind::Wrapper
        } else {
            CallKind::Leaf
        };
        let parent = match input.source {
            ToolCallSource::Direct => None,
            ToolCallSource::CodeMode { .. } => {
                let parent = input
                    .originating_item_id
                    .and_then(|id| self.wrappers.get(&digest(id.as_str())))
                    .cloned();
                if parent.is_none() {
                    self.observer.unknown();
                }
                parent
            }
        };
        let arguments = match input.payload {
            ToolPayload::Function { arguments } => serde_json::from_str(arguments).ok(),
            ToolPayload::Custom { input } => Some(Value::String(input.clone())),
            ToolPayload::ToolSearch { arguments } => serde_json::to_value(arguments).ok(),
        };
        if let Some(arguments) = arguments {
            self.observer.call(
                input.call_id,
                &name,
                &arguments,
                kind,
                parent
                    .as_deref()
                    .map_or(CallParent::Direct, CallParent::Nested),
            );
        } else {
            self.observer.unknown();
        }
        if kind == CallKind::Wrapper
            && let Some(id) = input.originating_item_id
        {
            if self.wrappers.len() < crate::stall_observation::MAX_CALLS
                && input.call_id.len() <= crate::stall_observation::MAX_ID_BYTES
            {
                self.wrappers
                    .insert(digest(id.as_str()), input.call_id.to_owned());
            } else {
                self.observer.unknown();
            }
        }
    }

    pub(crate) fn output(&mut self, input: &ToolOutputInput<'_>) {
        let name = input.tool_name.to_string();
        if let Some(output) = output_value(&name, input.output) {
            self.observer.outcome(input.call_id, &name, &output);
        } else {
            self.observer.unknown();
        }
    }
}

pub(crate) fn output_value(name: &str, output: &ResponseInputItem) -> Option<Value> {
    match output {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            serde_json::to_value(&output.body).ok().map(|body| {
                crate::stall_observation::model_output(
                    name,
                    crate::stall_observation::OutputBodyKind::Function,
                    body,
                )
            })
        }
        ResponseInputItem::CustomToolCallOutput { output, .. } => {
            let body = serde_json::to_value(&output.body).ok()?;
            Some(crate::stall_observation::model_output(
                name,
                crate::stall_observation::OutputBodyKind::Custom,
                body,
            ))
        }
        ResponseInputItem::McpToolCallOutput { output, .. } => {
            // Private MCP metadata must never make an opaque result observable.
            serde_json::to_value(&output.content).ok()
        }
        ResponseInputItem::ToolSearchOutput { tools, .. } => serde_json::to_value(tools).ok(),
        ResponseInputItem::Message { .. } => None,
    }
}
