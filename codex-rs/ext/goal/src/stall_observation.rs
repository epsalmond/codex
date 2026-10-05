//! Shared borrowed observation boundary for live hooks and rollout replay.
//! Retains digests and bounded correlation keys, never tool payloads or transcripts.

use crate::stall::Digest;
use crate::stall::FinalKind;
use crate::stall::TurnObservation;
use crate::stall::digest;
use crate::stall::normalized;
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) const MAX_CALLS: usize = 256;
pub(crate) const MAX_ID_BYTES: usize = 512;
pub(crate) const MAX_NAME_BYTES: usize = 256;
use crate::stall_settings::StallSettings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallKind {
    Leaf,
    Wrapper,
}

#[derive(Clone, Copy)]
pub(crate) enum CallParent<'a> {
    Direct,
    Nested(&'a str),
}

#[derive(Debug)]
struct Call {
    action: Digest,
    outcome: Option<Digest>,
    kind: CallKind,
    parent: Option<String>,
}

#[derive(Debug)]
pub(crate) struct TurnObserver {
    settings: StallSettings,
    calls: BTreeMap<String, Call>,
    automatic: bool,
    fresh_input: bool,
    external_result: Option<Digest>,
    activity: bool,
    final_kind: FinalKind,
    incomplete: bool,
    capped: bool,
}

impl Default for TurnObserver {
    fn default() -> Self {
        Self::new(StallSettings::default())
    }
}

impl TurnObserver {
    pub(crate) fn new(settings: StallSettings) -> Self {
        Self {
            incomplete: settings.profile_version == 0,
            settings,
            calls: BTreeMap::new(),
            automatic: false,
            fresh_input: false,
            external_result: None,
            activity: false,
            final_kind: FinalKind::default(),
            capped: false,
        }
    }
    pub(crate) fn automatic(&mut self) {
        self.automatic = true;
    }
    pub(crate) fn fresh_input(&mut self) {
        self.fresh_input = true;
    }
    pub(crate) fn external_result(&mut self, content: &Value) {
        let text = match content {
            Value::Array(parts) => {
                let texts: Option<Vec<_>> = parts
                    .iter()
                    .map(|part| match part["type"].as_str() {
                        Some("input_text" | "output_text") => part["text"].as_str(),
                        _ => None,
                    })
                    .collect();
                let Some(texts) = texts else {
                    self.unknown();
                    return;
                };
                texts.join("\n")
            }
            Value::String(text) => text.clone(),
            _ => {
                self.unknown();
                return;
            }
        };
        if text.trim().is_empty()
            || text.starts_with("Message Type:")
                && text
                    .split_once("\nPayload:\n")
                    .is_none_or(|(_, payload)| payload.trim().is_empty())
        {
            self.unknown();
            return;
        }
        self.external_result = Some(digest((self.external_result, normalized(&text))));
    }
    pub(crate) fn final_text(&mut self, text: &str) {
        self.final_kind = FinalKind::from_text(text, &self.settings);
    }
    pub(crate) fn commentary_text(&mut self, text: &str) {
        self.activity |= FinalKind::from_text(text, &self.settings) == FinalKind::Other;
    }
    pub(crate) fn unknown(&mut self) {
        self.incomplete = true;
    }

    pub(crate) fn call(
        &mut self,
        call_id: &str,
        name: &str,
        arguments: &Value,
        kind: CallKind,
        parent: CallParent<'_>,
    ) {
        let name = name.strip_prefix("functions.").unwrap_or(name);
        let parent = match parent {
            CallParent::Direct => None,
            CallParent::Nested(id) => Some(id),
        };
        if self.calls.len() >= MAX_CALLS {
            self.capped = true;
            self.unknown();
            return;
        }
        if name.len() > MAX_NAME_BYTES
            || call_id.len() > MAX_ID_BYTES
            || parent.is_some_and(|id| id.len() > MAX_ID_BYTES)
            || self.calls.contains_key(call_id)
        {
            self.unknown();
            return;
        }
        let kind = if self.settings.unlinked_timer_recognition
            && kind == CallKind::Wrapper
            && matches!(name, "exec" | "functions.exec")
            && arguments
                .as_str()
                .or_else(|| arguments.get("code").and_then(Value::as_str))
                .is_some_and(timer_only)
        {
            CallKind::Leaf
        } else {
            kind
        };
        self.calls.insert(
            call_id.to_owned(),
            Call {
                action: digest((
                    name,
                    canonical(name, arguments, Side::Arguments).to_string(),
                )),
                outcome: None,
                kind,
                parent: parent.map(str::to_owned),
            },
        );
    }

    pub(crate) fn outcome(&mut self, call_id: &str, name: &str, result: &Value) {
        let name = name.strip_prefix("functions.").unwrap_or(name);
        if !observable_outcome(result) {
            self.unknown();
            return;
        }
        let Some(call) = self.calls.get_mut(call_id) else {
            self.unknown();
            return;
        };
        if call.outcome.is_some() {
            self.unknown();
            return;
        }
        call.outcome = Some(digest(canonical(name, result, Side::Result).to_string()));
    }

    pub(crate) fn finish(self) -> TurnObservation {
        let complete = !self.incomplete
            && self.calls.iter().all(|(id, call)| {
                call.outcome.is_some()
                    && match call.kind {
                        CallKind::Leaf => call.parent.as_ref().is_none_or(|parent| {
                            self.calls
                                .get(parent)
                                .is_some_and(|outer| outer.kind == CallKind::Wrapper)
                        }),
                        CallKind::Wrapper => self.calls.values().any(|leaf| {
                            leaf.kind == CallKind::Leaf && leaf.parent.as_deref() == Some(id)
                        }),
                    }
            });
        let mut actions: Vec<_> = self
            .calls
            .values()
            .map(|call| (call.action, call.outcome))
            .collect();
        actions.sort_unstable();
        TurnObservation {
            automatic: self.automatic,
            complete,
            capped: self.capped,
            fresh_input: self.fresh_input,
            external_result: self.external_result,
            activity: self.activity,
            has_tools: !self.calls.is_empty(),
            final_kind: self.final_kind,
            actions: (complete && !actions.is_empty()).then(|| digest(actions)),
        }
    }
}

fn observable_outcome(result: &Value) -> bool {
    match result {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(parts) => !parts.is_empty() && parts.iter().all(observable_outcome),
        Value::Object(fields) => match fields.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text") => fields
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty()),
            Some("encrypted_content" | "input_image" | "input_audio") => false,
            _ => !fields.is_empty(),
        },
        Value::Bool(_) | Value::Number(_) => true,
    }
}

#[derive(Clone, Copy)]
enum Side {
    Arguments,
    Result,
}

fn canonical(name: &str, value: &Value, side: Side) -> Value {
    // Only these tool-owned envelope fields are transport bookkeeping. In particular,
    // never recursively strip id/duration/status from command output or user objects.
    let terminal = matches!(
        name,
        "exec_command" | "write_stdin" | "functions.exec_command" | "functions.write_stdin"
    );
    if terminal && let Value::Object(fields) = value {
        let mut fields = fields.clone();
        for key in match side {
            Side::Arguments => &["session_id", "yield_time_ms"][..],
            Side::Result => &[
                "chunk_id",
                "session_id",
                "process_id",
                "wall_time_seconds",
                "handler_duration_ms",
            ][..],
        } {
            fields.remove(*key);
        }
        return Value::Object(fields);
    }
    if matches!(name, "exec" | "functions.exec")
        && matches!(side, Side::Result)
        && let Value::Array(parts) = value
    {
        let mut parts = parts.clone();
        if let Some(first) = parts.first_mut()
            && let Some(text) = first.get("text").and_then(Value::as_str)
            && let Some(body) = text.strip_prefix("Script completed\nWall time ")
            && let Some((duration, output)) = body.split_once("\nOutput:\n")
            && valid_code_mode_duration(duration)
        {
            first["text"] = Value::String(format!("Script completed\nOutput:\n{output}"));
        }
        return Value::Array(parts);
    }
    value.clone()
}

// A complete, small self-contained cell has an observable leaf outcome even in old
// rollouts without nested tool tracing. All other unlinked cells remain unknown.
fn timer_only(code: &str) -> bool {
    let code = code.trim();
    let code = if code.starts_with("// @exec:") {
        let Some((header, body)) = code.split_once('\n') else {
            return false;
        };
        if serde_json::from_str::<Value>(header.trim_start_matches("// @exec:").trim()).is_err() {
            return false;
        }
        body.trim()
    } else {
        code
    };
    let Some(body) = code.strip_prefix("await new Promise(") else {
        return false;
    };
    let Some((binding, body)) = body.split_once("=>") else {
        return false;
    };
    let binding = binding.trim();
    if !binding
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        || !binding
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return false;
    }
    let Some(body) = body.trim().strip_prefix("setTimeout(") else {
        return false;
    };
    let Some((argument, body)) = body.split_once(',') else {
        return false;
    };
    if argument.trim() != binding {
        return false;
    }
    let Some((duration, remainder)) = body.split_once("));") else {
        return false;
    };
    if duration.trim().parse::<u64>().is_err() {
        return false;
    }
    let remainder = remainder.trim();
    if remainder.is_empty() {
        return true;
    }
    remainder
        .strip_prefix("text(")
        .and_then(|body| body.strip_suffix(");"))
        .is_some_and(|literal| serde_json::from_str::<String>(literal.trim()).is_ok())
}

fn valid_code_mode_duration(line: &str) -> bool {
    let parts: Vec<_> = line.split_whitespace().collect();
    match parts.as_slice() {
        [total, "seconds"] => total.parse::<f64>().is_ok(),
        [
            total,
            "seconds",
            "(code-mode",
            host,
            "seconds;",
            "overhead",
            overhead,
            "seconds)",
        ] => [total, host, overhead]
            .iter()
            .all(|value| value.parse::<f64>().is_ok()),
        _ => false,
    }
}
