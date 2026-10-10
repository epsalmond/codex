//! Test seam that pauses an automatic wake after it reserves the active-turn slot and binds its
//! assignment, but before it installs its task. Integration tests use it to submit input inside
//! that window. Production code never registers a hold, so the check is a map lookup that misses.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;

use codex_protocol::ThreadId;
use tokio::sync::oneshot;

struct HoldPoint {
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

static HOLDS: LazyLock<Mutex<HashMap<ThreadId, HoldPoint>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A registered hold on the next automatic wake start of one thread. Dropping it releases a
/// held wake, so a failing test cannot leave the wake parked.
pub struct WakeStartHold {
    reached: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
}

impl WakeStartHold {
    /// Waits until the wake is parked with the slot reserved and its task not yet installed.
    pub async fn reached(&mut self) {
        let _ = (&mut self.reached).await;
    }

    /// Lets the parked wake install its task.
    pub fn release(self) {
        let _ = self.release.send(());
    }
}

/// Parks the next automatic wake start on `thread_id` until the returned hold is released.
pub(crate) fn hold_next_wake_start(thread_id: ThreadId) -> WakeStartHold {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            thread_id,
            HoldPoint {
                reached: reached_tx,
                release: release_rx,
            },
        );
    WakeStartHold {
        reached: reached_rx,
        release: release_tx,
    }
}

pub(super) async fn wait_if_held(thread_id: ThreadId) {
    let hold = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&thread_id);
    if let Some(HoldPoint { reached, release }) = hold {
        let _ = reached.send(());
        let _ = release.await;
    }
}
