//! Test seam that parks a turn start at a chosen point between reserving the active-turn slot
//! and installing its task. Integration tests use it to order an automatic wake against explicit
//! input inside that window. Production code never registers a hold, so each check is a map
//! lookup that misses.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;

use codex_protocol::ThreadId;
use tokio::sync::oneshot;

/// Where a turn start parks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurnStartHoldPoint {
    /// An automatic wake has reserved the slot and has not yet bound its assignment.
    WakeBeforeBind,
    /// An automatic wake has bound its assignment and has not yet installed its task.
    WakeBeforeInstall,
    /// Input has reserved the slot and has not yet bound its assignment.
    InputBeforeBind,
    /// Input has bound its assignment and has not yet installed its task.
    InputBeforeInstall,
}

struct HoldPoint {
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
    finished: oneshot::Sender<()>,
}

static HOLDS: LazyLock<Mutex<HashMap<(ThreadId, TurnStartHoldPoint), HoldPoint>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A registered hold on the next turn start of one thread at one point. Dropping it releases a
/// held start, so a failing test cannot leave it parked.
pub struct TurnStartHold {
    reached: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
    finished: oneshot::Receiver<()>,
}

impl TurnStartHold {
    /// Waits until the start is parked at its hold point.
    pub async fn reached(&mut self) {
        let _ = (&mut self.reached).await;
    }

    /// Lets the parked start continue.
    pub fn release(self) {
        let _ = self.release.send(());
    }

    /// Lets the parked start continue and waits until its start attempt returns.
    pub async fn release_and_wait(self) {
        let _ = self.release.send(());
        let _ = self.finished.await;
    }
}

/// Parks the next turn start on `thread_id` that reaches `point` until the returned hold is
/// released.
pub(crate) fn hold_next_turn_start(
    thread_id: ThreadId,
    point: TurnStartHoldPoint,
) -> TurnStartHold {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            (thread_id, point),
            HoldPoint {
                reached: reached_tx,
                release: release_rx,
                finished: finished_tx,
            },
        );
    TurnStartHold {
        reached: reached_rx,
        release: release_tx,
        finished: finished_rx,
    }
}

/// Signals the hold's owner when the held start attempt returns. Keep it alive for the rest of
/// the start attempt.
#[must_use]
pub(crate) struct HeldTurnStart(Option<oneshot::Sender<()>>);

impl Drop for HeldTurnStart {
    fn drop(&mut self) {
        if let Some(finished) = self.0.take() {
            let _ = finished.send(());
        }
    }
}

pub(crate) async fn wait_if_held(thread_id: ThreadId, point: TurnStartHoldPoint) -> HeldTurnStart {
    let hold = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(thread_id, point));
    let Some(HoldPoint {
        reached,
        release,
        finished,
    }) = hold
    else {
        return HeldTurnStart(None);
    };
    let _ = reached.send(());
    let _ = release.await;
    HeldTurnStart(Some(finished))
}
