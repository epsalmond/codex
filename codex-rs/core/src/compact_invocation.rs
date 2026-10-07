use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;

/// Attribution for an inline compaction, shared by every implementation.
pub(crate) struct CompactionInvocation {
    pub(crate) trigger: CompactionTrigger,
    pub(crate) reason: CompactionReason,
    pub(crate) phase: CompactionPhase,
}

impl CompactionInvocation {
    pub(crate) fn automatic(reason: CompactionReason, phase: CompactionPhase) -> Self {
        Self {
            trigger: CompactionTrigger::Auto,
            reason,
            phase,
        }
    }
}
