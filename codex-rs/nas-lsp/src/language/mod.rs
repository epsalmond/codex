//! Language profiles; see `.codex/design/nas-lsp-languages.md`.

mod python;
mod rust;
mod typescript;

use crate::config::Readiness;
use crate::config::WorkspaceConfig;
use crate::freshness::Snapshot;
use crate::lsp::ReadinessPermit;
use crate::lsp::RequestError;
use crate::lsp::Session;
use serde_json::Value;
use std::path::Path;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) enum Adapter {
    Python,
    TypeScript(typescript::Adapter),
    Rust(rust::Adapter),
}

impl Adapter {
    pub(crate) async fn prepare(
        config: &WorkspaceConfig,
        snapshot: &Snapshot,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Self, RequestError> {
        if cancel.is_cancelled() {
            return Err(RequestError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(RequestError::Timeout);
        }
        match (config.language.as_str(), config.readiness) {
            ("python", Readiness::PyreflyLazyBlocking) => Ok(Self::Python),
            ("typescript", Readiness::TypeScriptProject) => {
                typescript::Adapter::prepare(config, snapshot, deadline, cancel)
                    .await
                    .map(Self::TypeScript)
            }
            ("rust", Readiness::RustAnalyzerProject) => {
                rust::Adapter::prepare(config, snapshot, deadline, cancel)
                    .await
                    .map(Self::Rust)
            }
            _ => Err(RequestError::Failure("unsupported_profile")),
        }
    }
    pub(crate) fn initialization_options(&self) -> Value {
        match self {
            Self::Python => Value::Null,
            Self::TypeScript(adapter) => adapter.initialization_options(),
            Self::Rust(adapter) => adapter.initialization_options(),
        }
    }
    pub(crate) fn language_id(&self, relative: &Path) -> Option<&'static str> {
        let extension = relative.extension()?.to_str()?;
        match self {
            Self::Python => python::language_id(extension),
            Self::TypeScript(_) => match extension {
                "ts" | "mts" | "cts" => Some("typescript"),
                "tsx" => Some("typescriptreact"),
                "js" | "mjs" | "cjs" => Some("javascript"),
                "jsx" => Some("javascriptreact"),
                _ => None,
            },
            Self::Rust(_) => (extension == "rs").then_some("rust"),
        }
    }
    pub(crate) fn requires_server_status(&self) -> bool {
        matches!(self, Self::Rust(_))
    }
    pub(crate) fn validate_query(&self, relative: &Path) -> Result<(), &'static str> {
        match self {
            Self::Python => Ok(()),
            Self::TypeScript(adapter) => adapter.validate_query(relative),
            Self::Rust(adapter) => adapter.validate_query(relative),
        }
    }
    pub(crate) async fn validate_inputs(
        &self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<(), RequestError> {
        if cancel.is_cancelled() {
            return Err(RequestError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(RequestError::Timeout);
        }
        match self {
            Self::Python => Ok(()),
            Self::TypeScript(adapter) => adapter.validate_inputs(deadline, cancel).await,
            Self::Rust(adapter) => adapter.validate_inputs(deadline, cancel).await,
        }
    }
    pub(crate) async fn ready(
        &self,
        session: &mut Session,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<ReadinessPermit, RequestError> {
        match self {
            Self::Python => Ok(ReadinessPermit { epoch: None }),
            Self::TypeScript(adapter) => adapter.ready(session, deadline, cancel).await,
            Self::Rust(adapter) => adapter.ready(session, deadline, cancel).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn language_ids_are_document_specific() {
        assert_eq!(
            Adapter::Python.language_id(Path::new("stub.pyi")),
            Some("python")
        );
        assert_eq!(Adapter::Python.language_id(Path::new("file.ts")), None);
    }
}
