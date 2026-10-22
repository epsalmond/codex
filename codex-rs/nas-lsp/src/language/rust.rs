//! Rust profile; see `.codex/design/nas-lsp-languages.md`.
//! Fail-closed scaffold; the language lane supplies qualification and behavior.

use crate::config::WorkspaceConfig;
use crate::freshness::Snapshot;
use crate::lsp::{ReadinessPermit, RequestError, Session};
use serde_json::Value;
use std::path::Path;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) struct Adapter;
impl Adapter {
    pub(super) async fn prepare(
        _: &WorkspaceConfig,
        _: &Snapshot,
        _: Instant,
        _: CancellationToken,
    ) -> Result<Self, RequestError> {
        Err(RequestError::Failure("rust_not_qualified"))
    }
    pub(super) fn initialization_options(&self) -> Value {
        Value::Null
    }
    pub(super) fn validate_query(&self, _: &Path) -> Result<(), &'static str> {
        Ok(())
    }
    pub(super) async fn validate_inputs(
        &self,
        _: Instant,
        _: CancellationToken,
    ) -> Result<(), RequestError> {
        Ok(())
    }
    pub(super) async fn ready(
        &self,
        _: &mut Session,
        _: Instant,
        _: CancellationToken,
    ) -> Result<ReadinessPermit, RequestError> {
        Err(RequestError::Failure("rust_not_qualified"))
    }
}
