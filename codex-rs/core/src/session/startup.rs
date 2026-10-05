//! Retains persistence acquisition and resources throughout managed startup.
//! The thread manager drops initialization, then joins acquisition and resource cleanup here.

use std::sync::Arc;
use std::sync::OnceLock;

use codex_protocol::protocol::Op;
use codex_thread_store::LiveThreadInitGuard;
use tokio::sync::Mutex;

use super::SessionIo;
use super::session::Session;

#[derive(Default)]
pub(crate) struct SessionStartup {
    pub(crate) persistence: Mutex<LiveThreadInitGuard>,
    pub(crate) session: OnceLock<Arc<Session>>,
    pub(crate) io: OnceLock<SessionIo>,
}

impl SessionStartup {
    pub(crate) async fn cleanup(&self) {
        if let Some(io) = self.io.get() {
            // The session loop owns persistence now. Preserve its shutdown semantics even
            // if registration or the caller's handoff was interrupted after the loop started.
            self.persistence.lock().await.commit();
            let _ = io.submit(Op::Interrupt).await;
            let _ = io.shutdown_and_wait().await;
        } else {
            if let Some(session) = self.session.get() {
                super::handlers::shutdown_session_runtime(session).await;
            }
            let mut persistence = std::mem::take(&mut *self.persistence.lock().await);
            persistence.discard().await;
        }
    }
}

/// Keeps generation cleanup observable if child initialization is dropped before publication.
pub(crate) struct GenerationStartup {
    pub(crate) startup: Arc<SessionStartup>,
    operation: Option<Arc<crate::agent::control::GenerationOperation>>,
    threads: Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<codex_protocol::ThreadId, Arc<crate::CodexThread>>,
        >,
    >,
}

impl GenerationStartup {
    pub(crate) fn new(
        operation: Arc<crate::agent::control::GenerationOperation>,
        threads: Arc<
            tokio::sync::RwLock<
                std::collections::HashMap<codex_protocol::ThreadId, Arc<crate::CodexThread>>,
            >,
        >,
    ) -> Self {
        Self {
            startup: Arc::default(),
            operation: Some(operation),
            threads,
        }
    }

    pub(crate) async fn disarm(mut self) {
        // Successful startup transfers the managed writer to the session loop.
        self.startup.persistence.lock().await.commit();
        self.operation = None;
    }
}

impl Drop for GenerationStartup {
    fn drop(&mut self) {
        let Some(operation) = self.operation.take() else {
            return;
        };
        let startup = Arc::clone(&self.startup);
        let threads = Arc::clone(&self.threads);
        tokio::spawn(async move {
            let _operation = operation;
            startup.cleanup().await;
            if let Some(session) = startup.session.get() {
                let mut threads = threads.write().await;
                if threads
                    .get(&session.thread_id())
                    .is_some_and(|thread| Arc::ptr_eq(&thread.session, session))
                {
                    threads.remove(&session.thread_id());
                }
            }
        });
    }
}
