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
//!     a process restart, but the resume boundary drops them:
//!     `ResumedHistory` (`codex-rs/history/src/lib.rs`) carries only
//!     `Vec<RolloutItem>`, not `Vec<RolloutLine>`, so the per-line `timestamp`
//!     is gone by the time a `Session` is built. `StoredThread::updated_at`
//!     still holds it one layer up, so plumbing it through is possible — see
//!     the note below.
//!
//! CONSEQUENCE. The clock is per-process: a session that resumes a thread from
//! disk starts with no recorded request, so the first turn after a resume is
//! treated as "cache warm" and does not cold-resume-shake, even though its
//! prompt cache is certainly cold. The trigger therefore covers the common
//! interactive case — a live session left idle past the TTL — and not a process
//! restart. Extending it to restarts needs `StoredThread::updated_at` threaded
//! through `ResumedHistory` into `Session::new`; that is deliberately out of
//! scope here.

use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

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
}
