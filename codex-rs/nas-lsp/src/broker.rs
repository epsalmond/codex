//! Broker lane scaffold. See `.codex/design/nas-lsp-broker.md`.

use crate::config::BrokerConfig;
use codex_lsp_protocol::NavigationRequest;
use codex_lsp_protocol::NavigationResponse;
use codex_lsp_protocol::Status;
use std::io;
use tokio_util::sync::CancellationToken;

pub struct Broker;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BrokerError(pub String);

impl Broker {
    pub fn new(_config: BrokerConfig) -> Result<Self, BrokerError> {
        Ok(Self)
    }

    pub async fn query(
        &self,
        _request: NavigationRequest,
        _cancel: CancellationToken,
    ) -> NavigationResponse {
        NavigationResponse::status(Status::Unavailable, "implementation_pending")
    }

    pub async fn serve(self, _shutdown: CancellationToken) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "broker implementation pending",
        ))
    }
}
