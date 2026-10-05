//! Conservative turn-level suspicion, independent of continuation policy.

use crate::stall_settings::StallSettings;
use serde::Deserialize;
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::num::NonZeroU32;

pub(crate) type Digest = [u64; 2];

pub(crate) fn digest(value: impl Hash) -> Digest {
    std::array::from_fn(|domain| {
        let mut hasher = DefaultHasher::new();
        domain.hash(&mut hasher);
        value.hash(&mut hasher);
        hasher.finish()
    })
}

pub(crate) fn normalized(text: &str) -> String {
    text.replace(['‘', '’'], "'")
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FinalKind {
    #[default]
    Empty,
    Waiting,
    Other,
}

impl FinalKind {
    pub(crate) fn from_text(text: &str, settings: &StallSettings) -> Self {
        let text: String = normalized(text).chars().take(513).collect();
        if text.is_empty() {
            Self::Empty
        } else if text.chars().count() <= settings.waiting_text_max_chars.get() as usize
            && settings
                .prefixes
                .iter()
                .any(|prefix| text.starts_with(prefix.as_str()))
        {
            Self::Waiting
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Suspicion {
    EmptyFinal,
    WaitingFinal,
    RepeatedActions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Assessment {
    Unclassified,
    NotSuspected,
    Suspected {
        reason: Suspicion,
        streak: u32,
        threshold_reached: bool,
    },
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TurnObservation {
    pub(crate) automatic: bool,
    pub(crate) complete: bool,
    pub(crate) capped: bool,
    pub(crate) fresh_input: bool,
    pub(crate) external_result: Option<Digest>,
    pub(crate) activity: bool,
    pub(crate) has_tools: bool,
    pub(crate) final_kind: FinalKind,
    pub(crate) actions: Option<Digest>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct StallDetector {
    previous_actions: Option<Digest>,
    previous_external_result: Option<Digest>,
    streak: u32,
    last_turn: Option<(Digest, Assessment)>,
}

impl StallDetector {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn observe(
        &mut self,
        turn_id: &str,
        observation: TurnObservation,
        threshold: NonZeroU32,
    ) -> Assessment {
        let id = digest(turn_id);
        if let Some((last_id, assessment)) = self.last_turn
            && last_id == id
        {
            return assessment;
        }
        let fresh_result = observation.external_result.is_some()
            && observation.external_result != self.previous_external_result;
        if observation.external_result.is_some() {
            self.previous_external_result = observation.external_result;
        }
        let previous_actions = self.previous_actions.take();
        let assessment = if !observation.automatic {
            self.streak = 0;
            Assessment::NotSuspected
        } else if !observation.complete {
            self.streak = 0;
            Assessment::Unclassified
        } else if observation.fresh_input
            || fresh_result
            || observation.activity
            || observation.final_kind == FinalKind::Other
        {
            self.streak = 0;
            Assessment::NotSuspected
        } else {
            let reason = if !observation.has_tools
                && !observation.activity
                && observation.final_kind == FinalKind::Empty
            {
                Some(Suspicion::EmptyFinal)
            } else if !observation.has_tools && observation.final_kind == FinalKind::Waiting {
                Some(Suspicion::WaitingFinal)
            } else if observation.actions.is_some() && observation.actions == previous_actions {
                Some(Suspicion::RepeatedActions)
            } else {
                None
            };
            self.previous_actions = observation.actions;
            if let Some(reason) = reason {
                self.streak = self.streak.saturating_add(1);
                Assessment::Suspected {
                    reason,
                    streak: self.streak,
                    threshold_reached: self.streak >= threshold.get(),
                }
            } else {
                self.streak = 0;
                Assessment::NotSuspected
            }
        };
        self.last_turn = Some((id, assessment));
        assessment
    }
}
