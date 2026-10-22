//! Language worker lane scaffold. See `.codex/design/nas-lsp-broker.md`.

use crate::config::WorkspaceConfig;
use codex_lsp_protocol::NavigationResponse;
use codex_lsp_protocol::Operation;
use codex_lsp_protocol::Position;
use codex_lsp_protocol::Status;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub struct Worker;

pub struct WorkerQuery {
    pub operation: Operation,
    pub path: PathBuf,
    pub position: Position,
    pub max_results: usize,
}

impl Worker {
    pub async fn start(
        _config: Arc<WorkspaceConfig>,
        _deadline: Instant,
        _cancel: CancellationToken,
    ) -> Result<Self, NavigationResponse> {
        Err(NavigationResponse::status(
            Status::Unavailable,
            "implementation_pending",
        ))
    }

    pub async fn query(
        &mut self,
        _query: WorkerQuery,
        _deadline: Instant,
        _cancel: CancellationToken,
    ) -> NavigationResponse {
        NavigationResponse::status(Status::Unavailable, "implementation_pending")
    }

    pub async fn shutdown(&mut self) {}

    pub fn process_id(&self) -> Option<u32> {
        None
    }
}
