//! Rollout adapter; labels and episode selection never enter feature extraction.

use crate::stall::Assessment;
use crate::stall::StallDetector;
use crate::stall::WAITING_PREFIXES;
use crate::stall_observation::CallKind;
use crate::stall_observation::CallParent;
use crate::stall_observation::TurnObserver;
use serde_json::Value;
use std::num::NonZeroU32;

#[derive(Default)]
pub(crate) struct ReplayStats {
    pub(crate) turns: usize,
    pub(crate) automatic: usize,
    pub(crate) unknown: usize,
    pub(crate) capped: usize,
    pub(crate) max_streak: u32,
}

#[derive(Default)]
pub(crate) struct Replay {
    observer: Option<TurnObserver>,
    turn_id: String,
    detector: StallDetector,
    names: std::collections::BTreeMap<String, String>,
    counterfactual_auto: bool,
    admission_replaced: bool,
    pub(crate) stats: ReplayStats,
}

impl Replay {
    pub(crate) fn counterfactual_auto() -> Self {
        Self {
            counterfactual_auto: true,
            ..Default::default()
        }
    }
    pub(crate) fn record(&mut self, record: &Value, threshold: NonZeroU32) -> Option<Assessment> {
        let payload = &record["payload"];
        match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("task_started" | "turn_started")) => {
                if self.observer.is_some() {
                    self.detector.reset();
                }
                self.turn_id = payload["turn_id"].as_str()?.to_owned();
                self.observer = Some(TurnObserver::default());
                if self.counterfactual_auto {
                    self.observer.as_mut()?.automatic();
                }
                self.admission_replaced = false;
                self.names.clear();
            }
            (Some("response_item"), _) => {
                let observer = self.observer.as_mut()?;
                if self.names.len() > 256 {
                    observer.unknown();
                    return None;
                }
                match payload["type"].as_str() {
                    Some("message") => {
                        let Some(content) = payload["content"].as_array() else {
                            observer.unknown();
                            return None;
                        };
                        if content.iter().any(|part| {
                            !matches!(part["type"].as_str(), Some("input_text" | "output_text"))
                                || part["text"].as_str().is_none()
                        }) {
                            observer.unknown();
                        }
                        let text = content
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if payload["role"] == "user" {
                            let kinds = &payload["internal_chat_message_metadata_passthrough"]["content_item_kinds"];
                            if kinds.as_array().is_some_and(|kinds| kinds.len() == 1 && kinds[0] == "goal.internal_context")
                                && text.starts_with("<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal.") {
                                observer.automatic();
                            } else if kinds.as_array().is_none_or(|kinds| kinds.iter().any(|kind| kind.as_str().is_some_and(|kind| kind.starts_with("user.")))) {
                                if self.counterfactual_auto && !self.admission_replaced {self.admission_replaced=true;}
                                else {observer.fresh_input();}
                            }
                        } else if payload["role"] == "assistant" {
                            if payload["phase"] == "commentary" {
                                observer.commentary_text(&text, WAITING_PREFIXES);
                            } else {
                                observer.final_text(&text, WAITING_PREFIXES);
                            }
                        }
                    }
                    Some("agent_message") => {
                        let task_admission = payload["content"]
                            .as_array()
                            .and_then(|parts| parts.first())
                            .and_then(|part| part["text"].as_str())
                            .is_some_and(|text| text.starts_with("Message Type: NEW_TASK\n"));
                        if self.counterfactual_auto && !self.admission_replaced && task_admission {
                            self.admission_replaced = true;
                        } else {
                            observer.external_result(&payload["content"]);
                        }
                    }
                    Some("function_call" | "custom_tool_call") => {
                        let (Some(id), Some(name), Some(argument)) = (
                            payload["call_id"].as_str(),
                            payload["name"].as_str(),
                            payload.get("arguments").or_else(|| payload.get("input")),
                        ) else {
                            observer.unknown();
                            return None;
                        };
                        if id.len() > 512 || name.len() > 256 {
                            observer.unknown();
                            return None;
                        }
                        let arguments = argument
                            .as_str()
                            .and_then(|text| serde_json::from_str(text).ok())
                            .unwrap_or_else(|| argument.clone());
                        let kind = if matches!(name, "exec" | "functions.exec")
                            && payload["namespace"]
                                .as_str()
                                .is_none_or(|namespace| namespace == "functions")
                        {
                            CallKind::Wrapper
                        } else {
                            CallKind::Leaf
                        };
                        let parent = match payload["parent_call_id"].as_str() {
                            Some(id) => CallParent::Nested(id),
                            None => CallParent::Direct,
                        };
                        let name = match payload["namespace"].as_str() {
                            Some(namespace) => format!("{namespace}.{name}"),
                            None => name.to_owned(),
                        };
                        observer.call(id, &name, &arguments, kind, parent);
                        if name.len() > 256 {
                            observer.unknown();
                            return None;
                        }
                        self.names.insert(id.to_owned(), name);
                    }
                    Some("function_call_output" | "custom_tool_call_output") => {
                        let Some(id) = payload["call_id"].as_str() else {
                            observer.unknown();
                            return None;
                        };
                        if payload.get("output").is_none() {
                            observer.unknown();
                            return None;
                        }
                        if let Some(name) = self.names.remove(id) {
                            let output = &payload["output"];
                            let result = output
                                .as_str()
                                .and_then(|text| serde_json::from_str(text).ok())
                                .unwrap_or_else(|| output.clone());
                            observer.outcome(id, &name, &result);
                        } else {
                            observer.unknown();
                        }
                    }
                    Some("reasoning") => {}
                    _ => observer.unknown(),
                }
            }
            (Some("event_msg"), Some("task_complete" | "turn_complete")) => {
                if payload["turn_id"].as_str()? != self.turn_id {
                    return None;
                }
                let mut observer = self.observer.take()?;
                if let Some(text) = payload["last_agent_message"].as_str() {
                    observer.final_text(text, WAITING_PREFIXES);
                }
                if !payload["error"].is_null() {
                    observer.unknown();
                }
                let observation = observer.finish();
                self.stats.turns += 1;
                self.stats.automatic += usize::from(observation.automatic);
                self.stats.unknown += usize::from(!observation.complete);
                self.stats.capped += usize::from(observation.capped);
                let assessment = self.detector.observe(&self.turn_id, observation, threshold);
                if let Assessment::Suspected { streak, .. } = assessment {
                    self.stats.max_streak = self.stats.max_streak.max(streak);
                }
                return Some(assessment);
            }
            (Some("compacted"), _) => {
                if let Some(observer) = self.observer.as_mut() {
                    observer.unknown();
                }
                self.detector.reset();
            }
            (Some("event_msg"), Some("turn_aborted")) => {
                self.observer = None;
                self.detector.reset();
                self.names.clear();
            }
            _ => {}
        }
        None
    }
}
