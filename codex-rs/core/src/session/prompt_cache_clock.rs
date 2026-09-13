//! Tracks how long a thread has sat idle between sampling requests, which is
//! the input to auto-shake's cold-resume (prompt-cache-expiry) trigger.
//!
//! WHICH TIMESTAMP THIS IS. Auto-shake needs "how long since this thread last
//! talked to the model", because that is what decides whether the provider's
//! prompt cache still holds the thread's prefix. The fork has three candidate
//! sources, and this is why the *sampling request* is the one used:
//!
//!   * the timestamp of each sampling request — recorded here, in memory, at
//!     the moment `try_run_sampling_request` calls `stream()`. This is exactly
//!     the instant the provider's cache entry for this thread was (re)written,
//!     so it is the most faithful answer to "is the cache still warm?";
//!   * the last assistant message — what oh-my-pi's
//!     `runCacheExpiredPrePromptShakeIfNeeded` keys on. It differs from the
//!     request timestamp only by the duration of one response, which is
//!     irrelevant against TTLs measured in minutes-to-hours, and reconstructing
//!     it means walking history on every pre-sampling check;
//!   * the persisted rollout line timestamps. Those would additionally survive
//!     a process restart, but the resume boundary drops them for most resume
//!     paths: `ResumedHistory` (`codex-rs/history/src/lib.rs`) carries only
//!     `Vec<RolloutItem>`, not `Vec<RolloutLine>`, so the per-line `timestamp`
//!     is gone by the time a `Session` is built.
//!
//! SEEDING ON RESUME. `ResumedHistory` carries an additional
//! `last_activity_at: Option<DateTime<Utc>>` used only to seed this clock so
//! the *first* turn after a process restart can still see an expired TTL.
//! The value is `StoredThread::updated_at` at every resume site that has a
//! `StoredThread` in hand (thread-store resume, multi-agent resume, thread
//! fork) — it is already loaded as part of the resume, requires no extra
//! read, and is refreshed on every turn (see
//! `codex-rs/thread-store/src/thread_metadata_sync.rs`), so it tracks "last
//! request" closely enough for a TTL measured in minutes-to-hours. The one
//! resume path without a `StoredThread` — `Recorder::get_rollout_history`,
//! which parses a rollout file directly by path — instead carries forward the
//! timestamp of the last `RolloutLine` it parses, since that loop already
//! walks every line and the cost of remembering the last one is free. Fresh
//! threads and forks built from an in-memory snapshot with no backing store
//! record (`fork_prepared_thread`) seed nothing: their first turn is
//! warm-by-definition, same as before this change.
//!
//! Seeding only ever *primes* the clock before the first real request: once
//! `record_sampling_request` runs, it overwrites the seed like any other
//! request, so dedupe (`cold_resume_already_decided`) behaves identically
//! whether the current idle window opened from a seed or a live request.
//!
//! REPRESENTATION. The clock still stores a monotonic `Instant` internally —
//! `record_sampling_request` and comparisons against "now" are unaffected by
//! wall-clock adjustments. Seeding converts the wall-clock timestamp to an
//! `Instant` once, at seed time: it computes `elapsed = Utc::now() -
//! last_activity_at` and stores `Instant::now() - elapsed`. This keeps every
//! later comparison monotonic, at the cost of baking in whatever
//! wall-clock-vs-monotonic skew existed at the moment of seeding (a step of
//! the wall clock between the persisted write and process start would offset
//! the computed idle time by the size of the step — acceptable for a TTL
//! measured in minutes-to-hours, and the same kind of skew any use of a
//! persisted timestamp already accepts). A negative or unrepresentable
//! `elapsed` (clock skew, or a timestamp older than the monotonic clock can
//! represent) falls back to "no idle time", i.e. cache-warm — a safe default
//! that just misses this one trigger rather than misfiring.

use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use chrono::DateTime;
use chrono::Utc;

/// Per-thread idle clock. Monotonic (`Instant`), so a wall-clock adjustment
/// while the session is idle cannot make the thread look arbitrarily stale or
/// arbitrarily fresh.
#[derive(Debug, Default)]
pub(crate) struct PromptCacheClock {
    inner: Mutex<PromptCacheClockInner>,
}

#[derive(Debug, Default)]
struct PromptCacheClockInner {
    /// When this thread last issued a sampling request, or `None` before the
    /// first one.
    last_request_at: Option<Instant>,
    /// The `last_request_at` value a cold-resume shake was already decided
    /// against. Comparing against the live value is the dedupe key: it stays
    /// equal for the whole idle window and is superseded as soon as the next
    /// sampling request advances the clock.
    cold_resume_decided_for: Option<Instant>,
}

impl PromptCacheClock {
    /// Called once per sampling request, immediately before the request goes
    /// out. Opens a fresh idle window.
    pub(crate) fn record_sampling_request(&self) {
        self.record_sampling_request_at(Instant::now());
    }

    /// Called once, at session construction, when resuming a thread whose
    /// last known activity is `last_activity_at`. Primes the idle window so
    /// the first turn after a process restart can still see an expired TTL.
    ///
    /// A no-op if a sampling request has already been recorded (should not
    /// happen at construction time, but keeps this safe to call more than
    /// once and never lets a seed clobber a real request).
    pub(crate) fn seed_from_last_activity(&self, last_activity_at: DateTime<Utc>) {
        self.seed_from_last_activity_at(last_activity_at, Utc::now(), Instant::now());
    }

    fn seed_from_last_activity_at(
        &self,
        last_activity_at: DateTime<Utc>,
        wall_now: DateTime<Utc>,
        monotonic_now: Instant,
    ) {
        let mut inner = self.lock();
        if inner.last_request_at.is_some() {
            return;
        }
        let elapsed = wall_now
            .signed_duration_since(last_activity_at)
            .to_std()
            .unwrap_or(Duration::ZERO);
        let seeded = monotonic_now.checked_sub(elapsed).unwrap_or(monotonic_now);
        inner.last_request_at = Some(seeded);
    }

    fn record_sampling_request_at(&self, at: Instant) {
        let mut inner = self.lock();
        inner.last_request_at = Some(at);
    }

    /// Time since the last sampling request, or `None` when this thread has not
    /// made one in this process yet.
    pub(crate) fn idle_since_last_request(&self) -> Option<Duration> {
        self.idle_since_last_request_at(Instant::now())
    }

    fn idle_since_last_request_at(&self, now: Instant) -> Option<Duration> {
        self.lock()
            .last_request_at
            .map(|last| now.saturating_duration_since(last))
    }

    /// True when a cold-resume shake was already decided for the current idle
    /// window.
    pub(crate) fn cold_resume_already_decided(&self) -> bool {
        let inner = self.lock();
        inner.last_request_at.is_some() && inner.cold_resume_decided_for == inner.last_request_at
    }

    /// Record that a cold-resume shake was decided for the current idle window,
    /// so it is not decided again until the next sampling request.
    pub(crate) fn mark_cold_resume_decided(&self) {
        let mut inner = self.lock();
        inner.cold_resume_decided_for = inner.last_request_at;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PromptCacheClockInner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            // Nothing here can leave a torn value: the guarded state is two
            // `Option<Instant>` fields written whole.
            poisoned.into_inner()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_with_no_request_yet_has_no_idle_window() {
        let clock = PromptCacheClock::default();
        assert_eq!(clock.idle_since_last_request(), None);
        assert!(!clock.cold_resume_already_decided());
    }

    #[test]
    fn idle_is_measured_from_the_last_sampling_request() {
        let clock = PromptCacheClock::default();
        let start = Instant::now();
        clock.record_sampling_request_at(start);
        assert_eq!(
            clock.idle_since_last_request_at(start + Duration::from_secs(90)),
            Some(Duration::from_secs(90))
        );
        // A later request restarts the window.
        clock.record_sampling_request_at(start + Duration::from_secs(60));
        assert_eq!(
            clock.idle_since_last_request_at(start + Duration::from_secs(90)),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn the_dedupe_key_clears_when_the_next_request_goes_out() {
        let clock = PromptCacheClock::default();
        let start = Instant::now();
        clock.record_sampling_request_at(start);
        assert!(!clock.cold_resume_already_decided());

        clock.mark_cold_resume_decided();
        assert!(
            clock.cold_resume_already_decided(),
            "a second pre-sampling check in the same idle window must not re-decide"
        );

        clock.record_sampling_request_at(start + Duration::from_secs(1));
        assert!(
            !clock.cold_resume_already_decided(),
            "a new sampling request opens a new idle window"
        );
    }

    #[test]
    fn seeding_from_a_stale_last_activity_reads_as_idle() {
        let clock = PromptCacheClock::default();
        let wall_now = Utc::now();
        let last_activity_at = wall_now - chrono::Duration::hours(2);
        let monotonic_now = Instant::now();
        clock.seed_from_last_activity_at(last_activity_at, wall_now, monotonic_now);
        let idle = clock.idle_since_last_request_at(monotonic_now);
        assert_eq!(idle, Some(Duration::from_secs(2 * 60 * 60)));
    }

    #[test]
    fn seeding_from_a_recent_last_activity_reads_as_nearly_warm() {
        let clock = PromptCacheClock::default();
        let wall_now = Utc::now();
        let last_activity_at = wall_now - chrono::Duration::seconds(5);
        let monotonic_now = Instant::now();
        clock.seed_from_last_activity_at(last_activity_at, wall_now, monotonic_now);
        let idle = clock.idle_since_last_request_at(monotonic_now);
        assert_eq!(idle, Some(Duration::from_secs(5)));
    }

    #[test]
    fn a_real_request_recorded_after_seeding_wins_and_the_seed_never_reapplies() {
        let clock = PromptCacheClock::default();
        let wall_now = Utc::now();
        let last_activity_at = wall_now - chrono::Duration::hours(2);
        let monotonic_now = Instant::now();
        clock.seed_from_last_activity_at(last_activity_at, wall_now, monotonic_now);
        // A real sampling request supersedes the seed.
        clock.record_sampling_request_at(monotonic_now);
        assert_eq!(
            clock.idle_since_last_request_at(monotonic_now),
            Some(Duration::ZERO)
        );
        // Seeding again (e.g. a defensive second call) must not clobber the
        // real request.
        clock.seed_from_last_activity_at(last_activity_at, wall_now, monotonic_now);
        assert_eq!(
            clock.idle_since_last_request_at(monotonic_now),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn a_last_activity_timestamp_in_the_future_seeds_as_warm_not_negative() {
        let clock = PromptCacheClock::default();
        let wall_now = Utc::now();
        let last_activity_at = wall_now + chrono::Duration::hours(1);
        let monotonic_now = Instant::now();
        clock.seed_from_last_activity_at(last_activity_at, wall_now, monotonic_now);
        assert_eq!(
            clock.idle_since_last_request_at(monotonic_now),
            Some(Duration::ZERO)
        );
    }
}
