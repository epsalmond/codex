//! The same reducer consumes live observations and synthetic replay incidents.

use std::collections::HashMap;

use codex_protocol::models::ResponseInputItem;

use super::observation::Check;
use super::observation::Fingerprint;
use super::observation::Intent;
use super::observation::Kind;
use super::observation::Start;
use super::observation::outcome;

const MAX_IN_FLIGHT: usize = 16;
const COMPLETED_CHECKS: u8 = 3;

#[derive(Default)]
pub(super) struct Episode {
    pending: HashMap<Fingerprint, Pending>,
    // Intent precedes the execution gate. Unmatched rejected intents suppress the
    // rest of this turn rather than guessing that a failed call completed.
    dispatched: HashMap<Fingerprint, Intent>,
    candidate: Option<(Check, Fingerprint, u8)>,
    generation: u64,
    attempted: bool,
    disabled: bool,
    overlap: bool,
}

struct Pending {
    kind: Kind,
    generation: u64,
    success: bool,
    cell: Option<Fingerprint>,
    saw_leaf: bool,
}

impl Episode {
    pub(super) fn dispatch(&mut self, key: Option<Fingerprint>, intent: Intent) {
        let Some(key) = key else {
            self.disable();
            return;
        };
        if self.disabled {
            return;
        }
        if self.dispatched.len() >= MAX_IN_FLIGHT || self.dispatched.contains_key(&key) {
            self.disable();
            return;
        }
        // Capture overlap while the earlier handler is still running, even when
        // this handler waits behind its execution lock. Wrappers are validated
        // separately at start against the exact originating item and cell.
        if (matches!(intent, Intent::Wrapper | Intent::Status) && !self.dispatched.is_empty())
            || self
                .dispatched
                .values()
                .any(|intent| !matches!(intent, Intent::Wrapper))
        {
            self.overlap = true;
            self.reset();
        } else if matches!(intent, Intent::Unknown) {
            self.reset();
        }
        self.dispatched.insert(key, intent);
    }

    pub(super) fn reset(&mut self) {
        self.candidate = None;
        self.generation = self.generation.saturating_add(1);
    }

    pub(super) fn start(&mut self, start: Start) -> bool {
        let Some(key) = start.key else {
            self.disable();
            return false;
        };
        if self.disabled {
            return false;
        }
        if self.pending.len() >= MAX_IN_FLIGHT || self.pending.contains_key(&key) {
            self.disable();
            return false;
        }
        let mut kind = start.kind;
        if !self.dispatched.contains_key(&key) {
            kind = Kind::Unknown;
        }
        let wrapper = if let Some((origin, cell)) = start.nested {
            let wrapper = self.pending.iter_mut().find(|(_, pending)| {
                matches!(pending.kind, Kind::Wrapper { origin: candidate, .. } if candidate == origin)
            });
            if let Some((key, pending)) = wrapper {
                if pending.cell.is_none_or(|previous| previous == cell) {
                    pending.cell = Some(cell);
                    pending.saw_leaf = true;
                    Some(*key)
                } else {
                    kind = Kind::Unknown;
                    None
                }
            } else {
                kind = Kind::Unknown;
                None
            }
        } else {
            None
        };
        if self
            .dispatched
            .keys()
            .any(|other| *other != key && Some(*other) != wrapper)
            || self.pending.keys().any(|other| Some(*other) != wrapper)
        {
            self.overlap = true;
            self.reset();
        }
        let intervene = matches!(kind, Kind::Check(check) if self.candidate.is_some_and(|(previous, _, count)| previous == check && count >= COMPLETED_CHECKS))
            && !self.overlap
            && !self.attempted;
        if intervene {
            self.attempted = true;
            self.reset();
        }
        if matches!(kind, Kind::Unknown) {
            self.reset();
        }
        self.pending.insert(
            key,
            Pending {
                kind,
                generation: self.generation,
                success: false,
                cell: None,
                saw_leaf: false,
            },
        );
        intervene
    }

    pub(super) fn finish(&mut self, key: Option<Fingerprint>, success: bool) {
        if success {
            if let Some(pending) = key.and_then(|key| self.pending.get_mut(&key)) {
                pending.success = true;
            }
        } else {
            if let Some(key) = key {
                self.pending.remove(&key);
            }
            if let Some(key) = key {
                self.dispatched.remove(&key);
            }
            self.reset();
            if self.pending.is_empty() && self.dispatched.is_empty() {
                self.overlap = false;
            }
        }
    }

    pub(super) fn output(&mut self, key: Option<Fingerprint>, output: &ResponseInputItem) {
        if let Some(key) = key {
            self.dispatched.remove(&key);
        }
        let pending = key.and_then(|key| self.pending.remove(&key));
        if let Some(pending) = pending {
            if !pending.success || pending.generation != self.generation || self.overlap {
                self.reset();
            } else {
                match pending.kind {
                    Kind::Check(check) => {
                        if let Some(result) = outcome(check, output) {
                            let count = match self.candidate {
                                Some((previous, outcome, count))
                                    if check == previous && result == outcome =>
                                {
                                    count.saturating_add(1)
                                }
                                _ => 1,
                            };
                            self.candidate = Some((check, result, count));
                        } else {
                            self.reset();
                        }
                    }
                    Kind::Wrapper { timer, .. } if pending.saw_leaf || timer => {}
                    Kind::Wrapper { .. } | Kind::Unknown => self.reset(),
                }
            }
        } else {
            self.reset();
        }
        if self.pending.is_empty() && self.dispatched.is_empty() {
            self.overlap = false;
        }
    }

    fn disable(&mut self) {
        self.reset();
        self.pending.clear();
        self.dispatched.clear();
        self.disabled = true;
    }
}

#[cfg(test)]
#[path = "reducer_tests.rs"]
mod tests;
