//! Shared borrowed observation boundary for live hooks and rollout replay.
//! Retains digests and bounded correlation keys, never tool payloads or transcripts.

use crate::stall::Digest;
use crate::stall::FinalKind;
use crate::stall::TurnObservation;
use crate::stall::digest;
use crate::stall::normalized;
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_CALLS: usize = 256;
const MAX_ID_BYTES: usize = 512;

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

#[derive(Debug, Default)]
pub(crate) struct TurnObserver {
    calls: BTreeMap<String, Call>,
    automatic: bool,
    fresh_input: bool,
    external_result: Option<Digest>,
    activity: bool,
    final_kind: FinalKind,
    incomplete: bool,
    capped: bool,
}

impl TurnObserver {
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
    pub(crate) fn final_text(&mut self, text: &str, waiting_prefixes: &[&str]) {
        self.final_kind = FinalKind::from_text(text, waiting_prefixes);
    }
    pub(crate) fn activity(&mut self) {
        self.activity = true;
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
        let parent = match parent {
            CallParent::Direct => None,
            CallParent::Nested(id) => Some(id),
        };
        if self.calls.len() >= MAX_CALLS {
            self.capped = true;
            self.unknown();
            return;
        }
        if call_id.len() > MAX_ID_BYTES
            || parent.is_some_and(|id| id.len() > MAX_ID_BYTES)
            || self.calls.contains_key(call_id)
        {
            self.unknown();
            return;
        }
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
    value.clone()
}
