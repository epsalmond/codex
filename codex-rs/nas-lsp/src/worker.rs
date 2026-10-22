//! Read-only saved-file worker. See `.codex/design/nas-lsp-broker.md`.

use crate::config::WorkspaceConfig;
use crate::freshness::{self, Snapshot};
use crate::lsp::{RequestError, Session};
use codex_lsp_protocol::Location;
use codex_lsp_protocol::NavigationResponse;
use codex_lsp_protocol::Operation;
use codex_lsp_protocol::Position;
use codex_lsp_protocol::Status;
use codex_lsp_protocol::{MAX_PATH_BYTES, MAX_RESULTS};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub struct Worker {
    config: Arc<WorkspaceConfig>,
    generation: Snapshot,
    session: Option<Session>,
}

pub struct WorkerQuery {
    pub operation: Operation,
    pub path: PathBuf,
    pub position: Position,
    pub max_results: usize,
}

impl Worker {
    pub async fn start(
        config: Arc<WorkspaceConfig>,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Self, NavigationResponse> {
        let generation =
            freshness::snapshot(config.root.clone(), config.exclude_dirs.clone(), deadline)
                .await
                .map_err(inventory_error)?;
        let mut worker = Self {
            config,
            generation,
            session: None,
        };
        worker.initialize(deadline, cancel).await?;
        Ok(worker)
    }

    async fn initialize(
        &mut self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<(), NavigationResponse> {
        if Instant::now() >= deadline {
            return Err(status(Status::Timeout, "deadline"));
        }
        let mut session = Session::spawn(&self.config).map_err(unavailable)?;
        let root_uri = url::Url::from_directory_path(&self.config.root)
            .map_err(|_| unavailable("invalid_root"))?
            .to_string();
        // Pyrefly only uses workspaceFolders when the corresponding client
        // capability is true. Otherwise declarations and imports can receive
        // different inferred module identities, hiding reverse references.
        let result = session.request("initialize", json!({
            "processId": std::process::id(), "rootUri":root_uri,
            "capabilities":{"general":{"positionEncodings":["utf-16"]},"workspace":{"workspaceFolders":true},"textDocument":{"definition":{"linkSupport":true},"references":{}},"window":{"workDoneProgress":false}},
            "workspaceFolders":[{"uri":root_uri,"name":self.config.id}]
        }), deadline, cancel.clone()).await;
        let result = match result {
            Ok(value) => value,
            Err(error) => {
                session.shutdown().await;
                return Err(request_error(error));
            }
        };
        let capabilities = &result["capabilities"];
        if capabilities
            .get("positionEncoding")
            .and_then(Value::as_str)
            .is_some_and(|encoding| encoding != "utf-16")
        {
            session.shutdown().await;
            return Err(unavailable("unsupported_position_encoding"));
        }
        for provider in ["definitionProvider", "referencesProvider"] {
            if !matches!(
                capabilities.get(provider),
                Some(Value::Bool(true) | Value::Object(_))
            ) {
                session.shutdown().await;
                return Err(unavailable("unsupported_navigation"));
            }
        }
        let sync = &capabilities["textDocumentSync"];
        let open_close = sync["openClose"]
            .as_bool()
            .unwrap_or_else(|| sync.as_u64().is_some_and(|kind| kind > 0));
        let supported_changes = sync["change"]
            .as_u64()
            .or_else(|| sync.as_u64())
            .is_some_and(|kind| matches!(kind, 1 | 2));
        if !open_close || !supported_changes {
            session.shutdown().await;
            return Err(unavailable("unsupported_document_sync"));
        }
        if let Err(error) = session.notify("initialized", json!({}), deadline).await {
            session.shutdown().await;
            return Err(unavailable(error));
        }
        // Lazy-blocking waits for admitted documents, but does not discover all
        // unopened reverse dependents. Admit the entire bounded Python source
        // inventory before any navigation request, including stubs even when
        // the operator only permits querying .py files. A saved change creates
        // a new session, so every admitted document is immutable for its lifetime.
        let admitted = self.admit_documents(&session, deadline, &cancel).await;
        if let Err(response) = admitted {
            session.shutdown().await;
            return Err(response);
        }
        self.session = Some(session);
        Ok(())
    }

    async fn admit_documents(
        &self,
        session: &Session,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<(), NavigationResponse> {
        for (relative, bytes) in &self.generation.files {
            if cancel.is_cancelled() {
                return Err(status(Status::Cancelled, "cancelled"));
            }
            if Instant::now() >= deadline {
                return Err(status(Status::Timeout, "deadline"));
            }
            if !matches!(
                relative.extension().and_then(|ext| ext.to_str()),
                Some("py" | "pyi")
            ) {
                continue;
            }
            let text = std::str::from_utf8(bytes)
                .map_err(|_| unavailable("unsupported_source_encoding"))?;
            let uri = url::Url::from_file_path(self.config.root.join(relative))
                .map_err(|_| unavailable("invalid_path"))?;
            session
                .notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {
                        "uri": uri.as_str(), "languageId": self.config.language,
                        "version": 1, "text": text
                    }}),
                    deadline,
                )
                .await
                .map_err(|error| request_error(RequestError::Failure(error)))?;
        }
        Ok(())
    }

    pub async fn query(
        &mut self,
        query: WorkerQuery,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> NavigationResponse {
        if cancel.is_cancelled() {
            return status(Status::Cancelled, "cancelled");
        }
        let generation = match freshness::snapshot(
            self.config.root.clone(),
            self.config.exclude_dirs.clone(),
            deadline,
        )
        .await
        {
            Ok(generation) => generation,
            Err(error) => {
                // A failed refresh must never leave a stale generation usable.
                if let Some(mut session) = self.session.take() {
                    session.shutdown().await;
                }
                return inventory_error(error);
            }
        };
        if self.generation != generation || !self.session.as_ref().is_some_and(Session::healthy) {
            if let Some(mut session) = self.session.take() {
                session.shutdown().await;
            }
            self.generation = generation;
            if let Err(response) = self.initialize(deadline, cancel.clone()).await {
                return response;
            }
        }
        let relative = match query.path.strip_prefix(&self.config.root) {
            Ok(path)
                if path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))) =>
            {
                path.to_path_buf()
            }
            _ => return status(Status::InvalidRequest, "invalid_path"),
        };
        if !relative
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| {
                self.config
                    .source_extensions
                    .iter()
                    .any(|allowed| allowed.trim_start_matches('.') == ext)
            })
        {
            return status(Status::InvalidRequest, "unsupported_source");
        }
        let text = match self
            .generation
            .files
            .get(&relative)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
        {
            Some(text) => text,
            None => return status(Status::InvalidRequest, "source_unavailable"),
        };
        let position = match to_utf16(text, query.position) {
            Some(position) => position,
            None => return status(Status::InvalidRequest, "invalid_position"),
        };
        let uri = match url::Url::from_file_path(&query.path) {
            Ok(uri) => uri.to_string(),
            Err(_) => return status(Status::InvalidRequest, "invalid_path"),
        };
        let session = match self.session.as_mut() {
            Some(session) => session,
            None => return unavailable("server_unavailable"),
        };
        let (method, mut params) = match query.operation {
            Operation::Definition => (
                "textDocument/definition",
                json!({"textDocument":{"uri":uri},"position":position}),
            ),
            Operation::References => (
                "textDocument/references",
                json!({"textDocument":{"uri":uri},"position":position,"context":{"includeDeclaration":true}}),
            ),
        };
        // Lazy-blocking is the configured Pyrefly readiness barrier; initialize
        // by itself cannot turn an empty, incompletely indexed result into Ok.
        let result = session
            .request(method, params.take(), deadline, cancel)
            .await;
        let result = match result {
            Ok(result) if !session.indexing() => result,
            Ok(_) => return status(Status::Indexing, "indexing"),
            Err(error) => return request_error(error),
        };
        let root = self.config.root.clone();
        let generation = self.generation.clone();
        let normalized = tokio::task::spawn_blocking(move || {
            normalize(&root, &generation, result, query.max_results, deadline)
        });
        let response = match tokio::time::timeout_at(deadline, normalized).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return unavailable("result_processing_failed"),
            Err(_) => return status(Status::Timeout, "deadline"),
        };
        match freshness::snapshot(
            self.config.root.clone(),
            self.config.exclude_dirs.clone(),
            deadline,
        )
        .await
        {
            Ok(generation) if generation == self.generation => response,
            Ok(_) => status(Status::Stale, "files_changed_during_query"),
            Err(error) => inventory_error(error),
        }
    }

    pub async fn shutdown(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.shutdown().await;
        }
    }

    pub fn process_id(&self) -> Option<u32> {
        self.session.as_ref().and_then(Session::process_id)
    }
}

fn status(kind: Status, reason: &'static str) -> NavigationResponse {
    NavigationResponse::status(kind, reason)
}
fn unavailable(reason: &'static str) -> NavigationResponse {
    status(Status::Unavailable, reason)
}
fn inventory_error(reason: &'static str) -> NavigationResponse {
    if reason == "inventory_timeout" {
        status(Status::Timeout, "deadline")
    } else {
        unavailable(reason)
    }
}
fn request_error(error: RequestError) -> NavigationResponse {
    match error {
        RequestError::Timeout => status(Status::Timeout, "deadline"),
        RequestError::Cancelled => status(Status::Cancelled, "cancelled"),
        RequestError::Server(-32802 | -32002) => status(Status::Indexing, "server_not_ready"),
        RequestError::Server(-32801) => status(Status::Stale, "server_content_modified"),
        RequestError::Server(_) => unavailable("server_error"),
        RequestError::Failure("lsp_timeout") => status(Status::Timeout, "deadline"),
        RequestError::Failure(reason) => unavailable(reason),
    }
}

fn line(text: &str, number: u32) -> Option<&str> {
    let line = text.split('\n').nth(number as usize)?;
    Some(line.strip_suffix('\r').unwrap_or(line))
}

fn to_utf16(text: &str, position: Position) -> Option<Value> {
    let line_number = position.line.checked_sub(1)?;
    let scalar = position.column.checked_sub(1)? as usize;
    let line = line(text, line_number)?;
    if scalar > line.chars().count() {
        return None;
    }
    let character: usize = line.chars().take(scalar).map(char::len_utf16).sum();
    Some(json!({"line":line_number,"character":character}))
}

fn from_utf16(text: &str, position: &Value) -> Option<Position> {
    let line_number = u32::try_from(position["line"].as_u64()?).ok()?;
    let units = usize::try_from(position["character"].as_u64()?).ok()?;
    let line = line(text, line_number)?;
    let mut current = 0;
    for (scalar, character) in line.chars().enumerate() {
        if current == units {
            return Some(Position {
                line: line_number.checked_add(1)?,
                column: u32::try_from(scalar + 1).ok()?,
            });
        }
        current += character.len_utf16();
        if current > units {
            return None;
        }
    }
    (current == units).then(|| Position {
        line: line_number + 1,
        column: line.chars().count() as u32 + 1,
    })
}

fn normalize(
    root: &std::path::Path,
    generation: &Snapshot,
    result: Value,
    limit: usize,
    deadline: Instant,
) -> NavigationResponse {
    let values = match result {
        Value::Null => vec![],
        Value::Array(values) => values,
        Value::Object(_) => vec![result],
        _ => return unavailable("lsp_protocol"),
    };
    let mut locations = BTreeSet::new();
    let mut truncated = false;
    for value in values {
        if Instant::now() >= deadline {
            return status(Status::Timeout, "deadline");
        }
        let (uri, range) = if value.get("targetUri").is_some() {
            (&value["targetUri"], &value["targetSelectionRange"])
        } else {
            (&value["uri"], &value["range"])
        };
        let path = match uri
            .as_str()
            .and_then(|uri| url::Url::parse(uri).ok())
            .and_then(|uri| uri.to_file_path().ok())
        {
            Some(path) => path,
            None => return unavailable("invalid_result_uri"),
        };
        if !path.starts_with(root) {
            return unavailable("external_result");
        }
        let canonical = match std::fs::canonicalize(&path) {
            Ok(path) => path,
            Err(_) => return status(Status::Stale, "result_file_changed"),
        };
        let relative = match canonical.strip_prefix(root) {
            Ok(path) => path,
            Err(_) => return unavailable("external_result"),
        };
        let text = match generation
            .files
            .get(relative)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
        {
            Some(text) => text,
            None => return status(Status::Stale, "result_file_changed"),
        };
        let (start, end) = match (
            from_utf16(text, &range["start"]),
            from_utf16(text, &range["end"]),
        ) {
            (Some(start), Some(end)) if start <= end => (start, end),
            _ => return unavailable("invalid_result_position"),
        };
        let path = match relative.to_str() {
            Some(path) => path,
            None => return unavailable("invalid_result_path"),
        };
        if path.len() > MAX_PATH_BYTES {
            truncated = true;
            continue;
        }
        locations.insert(Location {
            path: path.to_owned(),
            start,
            end,
        });
    }
    let limit = limit.min(MAX_RESULTS);
    truncated |= locations.len() > limit;
    let mut response = NavigationResponse::ok(locations.into_iter().take(limit).collect());
    response.truncated = truncated;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_scalar_utf16_roundtrip_and_mid_surrogate_rejection() {
        let text = "é😀x\r\nlast\n";
        assert_eq!(
            to_utf16(text, Position { line: 1, column: 3 }),
            Some(json!({"line":0,"character":3}))
        );
        assert_eq!(
            from_utf16(text, &json!({"line":0,"character":3})),
            Some(Position { line: 1, column: 3 })
        );
        assert_eq!(from_utf16(text, &json!({"line":0,"character":2})), None);
        assert_eq!(to_utf16(text, Position { line: 1, column: 5 }), None);
        assert_eq!(
            to_utf16(text, Position { line: 3, column: 1 }),
            Some(json!({"line":2,"character":0}))
        );
    }

    #[tokio::test]
    async fn normalized_links_dedup_sort_caps_and_external_failure() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("main.py"), "é😀xx\n").unwrap();
        let generation = freshness::snapshot(
            root.to_path_buf(),
            vec![],
            Instant::now() + std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        let uri = url::Url::from_file_path(root.join("main.py"))
            .unwrap()
            .to_string();
        let first = json!({"uri":uri,"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":4}}});
        let link = json!({"targetUri":uri,"targetRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"targetSelectionRange":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}}});
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let response = normalize(root, &generation, json!([link, first, first]), 1, deadline);
        assert_eq!(response.status, Status::Ok);
        assert!(response.truncated);
        assert_eq!(response.locations.len(), 1);
        assert_eq!(response.locations[0].start, Position { line: 1, column: 3 });
        let bad = json!({"uri":uri,"range":{"start":{"line":0,"character":2},"end":{"line":0,"character":4}}});
        assert_eq!(
            normalize(root, &generation, bad, 100, deadline)
                .reason
                .as_deref(),
            Some("invalid_result_position")
        );
        let outside = json!({"uri":"file:///etc/passwd","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}});
        let response = normalize(root, &generation, json!([first, outside]), 100, deadline);
        assert_eq!(response.status, Status::Unavailable);
        assert!(response.locations.is_empty());
    }
}
