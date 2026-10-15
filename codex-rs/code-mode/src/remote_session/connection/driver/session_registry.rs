use std::collections::HashMap;
use std::sync::Arc;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CellTerminalReady;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;

use super::cell_ids::public_cell_id;
use super::cleanup::SessionCleanup;
use super::types::RemoteSession;

pub(super) struct CellOwner {
    pub(super) session_id: SessionId,
    pub(super) cell_id: CellId,
    pub(super) delegate: Arc<dyn CodeModeSessionDelegate>,
    terminal_ready: TerminalReadyLatch,
}

pub(super) type ReadyCell = (CellTerminalReady, Arc<dyn CodeModeSessionDelegate>);

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum InitialDisposition {
    #[default]
    Pending,
    Yielded,
    /// Delivered inline, failed, or abandoned; readiness is never reported.
    Settled,
}

/// Holds early readiness until the initial response proves the cell yielded.
#[derive(Default)]
struct TerminalReadyLatch {
    initial: InitialDisposition,
    ready: Option<CellTerminalReady>,
    reported: bool,
}

impl TerminalReadyLatch {
    fn admit(&mut self, ready: CellTerminalReady) {
        if !self.reported && self.ready.is_none() && self.initial != InitialDisposition::Settled {
            self.ready = Some(ready);
        }
    }

    fn resolve_initial(&mut self, yielded: bool) {
        if self.initial == InitialDisposition::Pending {
            self.initial = if yielded {
                InitialDisposition::Yielded
            } else {
                InitialDisposition::Settled
            };
        }
        if self.initial == InitialDisposition::Settled {
            self.ready = None;
        }
    }

    fn settle(&mut self) {
        self.initial = InitialDisposition::Settled;
        self.ready = None;
    }

    fn take(&mut self) -> Option<CellTerminalReady> {
        if self.initial != InitialDisposition::Yielded || self.reported {
            return None;
        }
        let ready = self.ready.take()?;
        self.reported = true;
        Some(ready)
    }
}

impl CellOwner {
    fn take_terminal_ready(&mut self) -> Option<ReadyCell> {
        let mut ready = self.terminal_ready.take()?;
        ready.cell_id = self.cell_id.clone();
        Some((ready, Arc::clone(&self.delegate)))
    }
}

pub(super) struct DelegateTarget {
    pub(super) session_id: SessionId,
    pub(super) cell_id: CellId,
    pub(super) delegate: Arc<dyn CodeModeSessionDelegate>,
}

pub(super) struct FailedSession {
    pub(super) cleanup: SessionCleanup,
    pub(super) cells: Vec<CellOwner>,
}

pub(super) enum CellAdmissionError {
    MissingSession,
    DuplicateCell,
}

struct SessionRecord {
    remote: RemoteSession,
    cleanup: SessionCleanup,
    phase: SessionPhase,
    cells: HashMap<WireCellId, CellOwner>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SessionPhase {
    Ready,
    Closing,
}

pub(super) struct SessionRegistry {
    records: HashMap<SessionId, SessionRecord>,
}

impl SessionRegistry {
    pub(super) fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    pub(super) fn contains(&self, session_id: &SessionId) -> bool {
        self.records.contains_key(session_id)
    }

    pub(super) fn contains_cell(&self, session_id: &SessionId, cell_id: &WireCellId) -> bool {
        self.records
            .get(session_id)
            .is_some_and(|session| session.cells.contains_key(cell_id))
    }

    pub(super) fn insert_ready(&mut self, session: RemoteSession, cleanup: SessionCleanup) {
        self.records.insert(
            session.id.clone(),
            SessionRecord {
                remote: session,
                cleanup,
                phase: SessionPhase::Ready,
                cells: HashMap::new(),
            },
        );
    }

    pub(super) fn require_ready(&self, session: &RemoteSession) -> Result<(), String> {
        let record = self
            .records
            .get(&session.id)
            .ok_or_else(|| format!("unknown code-mode session {}", session.id))?;
        if record.remote != *session {
            return Err("stale code-mode session generation".to_string());
        }
        if record.phase != SessionPhase::Ready {
            return Err("code-mode session is shutting down".to_string());
        }
        Ok(())
    }

    pub(super) fn begin_shutdown(&mut self, session: &RemoteSession) -> Result<(), String> {
        let record = self
            .records
            .get_mut(&session.id)
            .ok_or_else(|| format!("unknown code-mode session {}", session.id))?;
        if record.remote != *session {
            return Err("stale code-mode session generation".to_string());
        }
        if record.phase == SessionPhase::Closing {
            return Err("code-mode session is already closing".to_string());
        }
        record.phase = SessionPhase::Closing;
        Ok(())
    }

    pub(super) fn begin_abandoned_shutdown(&mut self, session_id: &SessionId) -> Option<bool> {
        let record = self.records.get_mut(session_id)?;
        if record.phase == SessionPhase::Closing {
            return Some(false);
        }
        record.phase = SessionPhase::Closing;
        Some(true)
    }

    pub(super) fn is_closing(&self, session_id: &SessionId) -> Option<bool> {
        self.records
            .get(session_id)
            .map(|record| record.phase == SessionPhase::Closing)
    }

    pub(super) fn admit_cell(
        &mut self,
        session: &RemoteSession,
        cell_id: WireCellId,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> Result<CellId, CellAdmissionError> {
        let Some(record) = self.records.get_mut(&session.id) else {
            return Err(CellAdmissionError::MissingSession);
        };
        if record.cells.contains_key(&cell_id) {
            return Err(CellAdmissionError::DuplicateCell);
        }
        let public_id = public_cell_id(session.generation, &cell_id);
        record.cells.insert(
            cell_id,
            CellOwner {
                session_id: session.id.clone(),
                cell_id: public_id.clone(),
                delegate,
                terminal_ready: TerminalReadyLatch::default(),
            },
        );
        Ok(public_id)
    }

    pub(super) fn delegate_target(
        &self,
        session_id: &SessionId,
        cell_id: &WireCellId,
    ) -> Result<DelegateTarget, String> {
        let session = self
            .records
            .get(session_id)
            .ok_or_else(|| format!("code-mode host delegated for unknown session {session_id}"))?;
        let owner = session.cells.get(cell_id).ok_or_else(|| {
            format!(
                "code-mode host delegated for unknown cell {} in session {session_id}",
                cell_id.as_str()
            )
        })?;
        Ok(DelegateTarget {
            session_id: session_id.clone(),
            cell_id: owner.cell_id.clone(),
            delegate: Arc::clone(&owner.delegate),
        })
    }

    /// Latches host readiness; reports it once the initial response yielded.
    pub(super) fn admit_terminal_ready(
        &mut self,
        session_id: &SessionId,
        cell_id: &WireCellId,
        ready: CellTerminalReady,
    ) -> Option<ReadyCell> {
        // Readiness can race local abandonment or closure; it is advisory, so drop it.
        let owner = self.cell_mut(session_id, cell_id)?;
        owner.terminal_ready.admit(ready);
        owner.take_terminal_ready()
    }

    /// Records the initial disposition and releases any latched readiness.
    pub(super) fn resolve_initial_response(
        &mut self,
        session_id: &SessionId,
        cell_id: &WireCellId,
        yielded: bool,
    ) -> Option<ReadyCell> {
        let owner = self.cell_mut(session_id, cell_id)?;
        owner.terminal_ready.resolve_initial(yielded);
        owner.take_terminal_ready()
    }

    /// Stops readiness for a cell whose caller abandoned it.
    pub(super) fn settle_terminal_ready(&mut self, session_id: &SessionId, cell_id: &WireCellId) {
        if let Some(owner) = self.cell_mut(session_id, cell_id) {
            owner.terminal_ready.settle();
        }
    }

    fn cell_mut(&mut self, session_id: &SessionId, cell_id: &WireCellId) -> Option<&mut CellOwner> {
        self.records.get_mut(session_id)?.cells.get_mut(cell_id)
    }

    pub(super) fn remove_cell(
        &mut self,
        session_id: &SessionId,
        cell_id: &WireCellId,
    ) -> Result<CellOwner, String> {
        let session = self.records.get_mut(session_id).ok_or_else(|| {
            format!(
                "code-mode host closed cell {} in unknown session {session_id}",
                cell_id.as_str()
            )
        })?;
        session
            .cells
            .remove(cell_id)
            .ok_or_else(|| format!("code-mode host closed unknown cell in session {session_id}"))
    }

    pub(super) fn remove_session(&mut self, session_id: &SessionId) -> Vec<CellOwner> {
        let Some(session) = self.records.remove(session_id) else {
            return Vec::new();
        };
        session.cells.into_values().collect()
    }

    pub(super) fn drain(&mut self) -> Vec<FailedSession> {
        let sessions = std::mem::take(&mut self.records);
        sessions
            .into_values()
            .map(|session| {
                let cells = session.cells.into_values().collect();
                FailedSession {
                    cleanup: session.cleanup,
                    cells,
                }
            })
            .collect()
    }
}
