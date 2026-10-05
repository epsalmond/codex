//! Goal-owned continuation policy; callers serialize database effects with the goal permit.

use std::num::NonZeroU32;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use codex_core::config::GoalContinuationGuardMode;
use codex_protocol::ThreadId;
use codex_state::GoalContinuationDisposition;
use codex_state::GoalStore;
use codex_state::ThreadGoal;
use serde::Deserialize;
use serde::Serialize;

use crate::stall::Assessment;
use crate::stall::Digest;
use crate::stall::StallDetector;
use crate::stall::digest;
use crate::stall_live::LiveTurn;
use crate::stall_settings;

pub(crate) struct ContinuationGuard {
    pub(crate) mode: GoalContinuationGuardMode,
    threshold: NonZeroU32,
    pub(crate) observation_settings: stall_settings::StallSettings,
    settings: Digest,
    state: Mutex<GuardState>,
}

#[derive(Default)]
struct GuardState {
    snapshot: Option<Snapshot>,
    turn: Option<GuardTurn>,
}

struct GuardTurn {
    id: String,
    goal_id: String,
    objective: Digest,
    live: LiveTurn,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    goal: Digest,
    objective: Digest,
    settings: Digest,
    detector: StallDetector,
    reports: Vec<Digest>,
    warned: bool,
}

impl ContinuationGuard {
    pub(crate) fn new(
        mode: GoalContinuationGuardMode,
        threshold: NonZeroU32,
        overrides: &codex_core::config::GoalsToml,
    ) -> Self {
        let (mode, effective) = match stall_settings::settings(overrides) {
            Ok(effective) => (mode, effective),
            Err(error) => {
                tracing::warn!(%error, "goal stall settings unavailable; retaining legacy empty-response guard");
                (
                    GoalContinuationGuardMode::Off,
                    stall_settings::StallSettings::default(),
                )
            }
        };
        Self {
            mode,
            threshold,
            settings: digest((
                format!("{mode:?}"),
                threshold.get(),
                effective.profile_version,
                &effective.prefixes,
                effective.waiting_text_max_chars.get(),
                effective.unlinked_timer_recognition,
            )),
            observation_settings: effective,
            state: Mutex::new(GuardState::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, GuardState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn empty_snapshot(&self, goal: &ThreadGoal) -> Snapshot {
        Snapshot {
            version: 1,
            goal: digest(&goal.goal_id),
            objective: digest(&goal.objective),
            settings: self.settings,
            detector: StallDetector::default(),
            reports: Vec::new(),
            warned: false,
        }
    }

    /// Invalid snapshots cannot supply evidence. Their independent hold remains
    /// until an authorized recovery; known objective/settings changes clear it.
    pub(crate) fn restore(&self, goal: &ThreadGoal, serialized: Option<&str>) -> bool {
        let snapshot = serialized
            .filter(|text| text.len() <= 4096)
            .and_then(|text| serde_json::from_str::<Snapshot>(text).ok());
        let valid = snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.version == 1 && snapshot.reports.len() <= 32);
        let changed = valid
            && snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.goal != digest(&goal.goal_id)
                    || snapshot.objective != digest(&goal.objective)
                    || snapshot.settings != self.settings
            });
        self.state().snapshot = if valid && !changed {
            snapshot
        } else {
            Some(self.empty_snapshot(goal))
        };
        // History may have changed through an older binary that understands the
        // hold but never records observations. A new runtime is an Unknown
        // boundary: retain identity deduplication and the independent hold,
        // rather than reusing an unprovably current pre-load suspicion streak.
        if let Some(snapshot) = self.state().snapshot.as_mut() {
            snapshot.detector.reset();
            snapshot.warned = false;
        }
        changed || self.mode != GoalContinuationGuardMode::Defer
    }

    pub(crate) fn begin(&self, turn_id: &str, goal: &ThreadGoal) {
        if self.mode == GoalContinuationGuardMode::Off {
            return;
        }
        self.state().turn = Some(GuardTurn {
            id: turn_id.to_owned(),
            goal_id: goal.goal_id.clone(),
            objective: digest(&goal.objective),
            live: LiveTurn::new(self.observation_settings.clone()),
        });
    }

    pub(crate) fn with_turn(&self, turn_id: &str, callback: impl FnOnce(&mut LiveTurn)) {
        if let Some(turn) = self.state().turn.as_mut().filter(|turn| turn.id == turn_id) {
            callback(&mut turn.live);
        }
    }

    pub(crate) fn admitted(&self, turn_id: &str) {
        self.with_turn(turn_id, |turn| turn.observer.automatic());
    }

    pub(crate) async fn recover(
        &self,
        store: &GoalStore,
        goal: &ThreadGoal,
        report: Option<Digest>,
    ) -> Result<bool, String> {
        let (snapshot, encoded) = {
            let mut state = self.state();
            if state.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.goal != digest(&goal.goal_id)
                    || snapshot.objective != digest(&goal.objective)
            }) {
                state.snapshot = None;
            }
            let mut snapshot = state
                .snapshot
                .clone()
                .unwrap_or_else(|| self.empty_snapshot(goal));
            if let Some(report) = report {
                if snapshot.reports.contains(&report) {
                    return Ok(false);
                }
                if snapshot.reports.len() == 32 {
                    snapshot.reports.remove(/*index*/ 0);
                }
                snapshot.reports.push(report);
            }
            snapshot.detector.reset();
            snapshot.warned = false;
            let encoded = serde_json::to_string(&snapshot).map_err(|err| err.to_string())?;
            (snapshot, encoded)
        };
        // Keep accepted-receipt identities and release the hold in one transaction.
        if !store
            .recover_thread_goal_continuation_guard(goal.thread_id, &goal.goal_id, &encoded)
            .await
            .map_err(|err| err.to_string())?
        {
            return Ok(false);
        }
        self.state().snapshot = Some(snapshot);
        Ok(true)
    }

    pub(crate) async fn finish(
        &self,
        store: &GoalStore,
        thread_id: ThreadId,
        turn_id: &str,
        goal: &ThreadGoal,
    ) -> Result<bool, String> {
        if self.mode == GoalContinuationGuardMode::Off {
            return Ok(false);
        }
        let (snapshot, encoded, defer, warn) = {
            let mut state = self.state();
            if state.turn.as_ref().is_none_or(|turn| {
                turn.id != turn_id
                    || turn.goal_id != goal.goal_id
                    || turn.objective != digest(&goal.objective)
            }) {
                return Ok(false);
            }
            let Some(turn) = state.turn.take() else {
                return Ok(false);
            };
            if state.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.goal != digest(&goal.goal_id)
                    || snapshot.objective != digest(&goal.objective)
            }) {
                state.snapshot = None;
            }
            let mut snapshot = state
                .snapshot
                .clone()
                .unwrap_or_else(|| self.empty_snapshot(goal));
            let assessment =
                snapshot
                    .detector
                    .observe(turn_id, turn.live.observer.finish(), self.threshold);
            let suspect = matches!(
                assessment,
                Assessment::Suspected {
                    threshold_reached: true,
                    ..
                }
            );
            let warn = suspect && !snapshot.warned;
            if !matches!(assessment, Assessment::Suspected { .. }) {
                snapshot.warned = false;
            } else if suspect {
                snapshot.warned = true;
            }
            let encoded = serde_json::to_string(&snapshot).map_err(|err| err.to_string())?;
            (
                snapshot,
                encoded,
                suspect && self.mode == GoalContinuationGuardMode::Defer,
                warn,
            )
        };
        let newly_held = store
            .record_thread_goal_continuation_guard(
                thread_id,
                &goal.goal_id,
                &encoded,
                if defer {
                    GoalContinuationDisposition::Defer
                } else {
                    GoalContinuationDisposition::Continue
                },
            )
            .await
            .map_err(|err| {
                if let Some(snapshot) = self.state().snapshot.as_mut() {
                    snapshot.detector.reset();
                    snapshot.warned = false;
                }
                err.to_string()
            })?;
        self.state().snapshot = Some(snapshot);
        Ok(if defer { newly_held } else { warn })
    }
}
