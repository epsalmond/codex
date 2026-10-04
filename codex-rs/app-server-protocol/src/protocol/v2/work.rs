//! Experimental lifecycle observation for one root agent thread.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Parameters for subscribing to a root thread's work state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadWorkSubscribeParams {
    pub thread_id: String,
}

/// A bounded snapshot of work owned by one root thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadWorkSnapshot {
    /// Opaque coordinator-incarnation and revision token. Pass it back unchanged to guard close.
    pub revision: String,
    pub outstanding_work: u32,
    pub running_finite_work: u32,
    pub pending_notifications: u32,
    pub active_root_turns: u32,
    pub pending_terminal_outputs: u32,
    pub output_forwarding_observed: bool,
    pub closed: bool,
    pub quiescent: bool,
}

/// Capability result returned by `thread/subscribeWorkState`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadWorkSubscribeResponse {
    pub outcome: ThreadWorkSubscribeOutcome,
    pub snapshot: Option<ThreadWorkSnapshot>,
}

/// Whether this root backend can provide authoritative work observation.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadWorkSubscribeOutcome {
    Subscribed,
    Unavailable,
}

/// Parameters for atomically closing a drained root work scope.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadWorkShutdownIfQuiescentParams {
    pub thread_id: String,
    /// Opaque revision returned by the latest lifecycle snapshot.
    pub revision: String,
}

/// Result of a revision-guarded root shutdown request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadWorkShutdownIfQuiescentResponse {
    pub outcome: ThreadWorkShutdownOutcome,
    pub snapshot: ThreadWorkSnapshot,
}

/// Possible outcomes of `thread/shutdownIfQuiescent`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadWorkShutdownOutcome {
    Closed,
    AlreadyClosed,
    ObservationInactive,
    StaleRevision,
    NotQuiescent,
}

/// Lifecycle snapshot published when observed root work changes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadWorkUpdatedNotification {
    pub thread_id: String,
    pub snapshot: ThreadWorkSnapshot,
}

#[cfg(test)]
#[path = "work_tests.rs"]
mod tests;
