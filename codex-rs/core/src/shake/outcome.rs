use super::ShakeResult;

/// Result of applying a reduction to live history, distinct from transform counts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShakeOutcome {
    Applied(ShakeResult),
    /// No content was reduced; a verified scan boundary may still have advanced.
    Noop,
    Skipped(ShakeSkipReason),
    Stale,
    Failed(ShakeFailure),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShakeSkipReason {
    EphemeralThread,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShakeFailure {
    InvalidSeal,
    ReplacementRefused,
    ArtifactRecovery,
}
