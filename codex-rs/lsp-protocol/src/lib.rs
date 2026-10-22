//! Bounded native navigation wire contract. See `.codex/design/nas-lsp-broker.md`.

use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::path::PathBuf;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 8192;
pub const MAX_RESPONSE_BYTES: usize = 16384;
pub const MAX_RESULTS: usize = 100;
pub const MAX_PATH_BYTES: usize = 1024;
pub const MAX_REASON_BYTES: usize = 64;
pub const MAX_TIMEOUT_MS: u64 = 15_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Definition,
    References,
}

/// One-based Unicode scalar coordinates; ends are exclusive.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub path: String,
    pub start: Position,
    pub end: Position,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Unavailable,
    Indexing,
    Stale,
    Timeout,
    Cancelled,
    Busy,
    InvalidRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NavigationRequest {
    pub version: u32,
    pub cwd: PathBuf,
    pub path: String,
    pub operation: Operation,
    pub position: Position,
    pub max_results: usize,
    pub max_response_bytes: usize,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NavigationResponse {
    pub status: Status,
    pub locations: Vec<Location>,
    pub truncated: bool,
    pub reason: Option<String>,
}

impl NavigationResponse {
    pub fn status(status: Status, reason: impl Into<String>) -> Self {
        Self {
            status,
            locations: Vec::new(),
            truncated: false,
            reason: Some(reason.into()),
        }
    }

    pub fn ok(locations: Vec<Location>) -> Self {
        Self {
            status: Status::Ok,
            locations,
            truncated: false,
            reason: None,
        }
    }
}

/// The caller must use a smaller scalar/empty output when a status cannot fit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetTooSmall;

/// Fit complete serialized JSON, including escaping, metadata and all locations.
pub fn fit_response(
    mut response: NavigationResponse,
    budget: usize,
) -> Result<NavigationResponse, BudgetTooSmall> {
    let budget = budget.min(MAX_RESPONSE_BYTES);
    if let Some(reason) = &mut response.reason {
        if reason.len() > MAX_REASON_BYTES || !reason.is_ascii() {
            *reason = "invalid_reason".to_owned();
        }
    }
    let count = response.locations.len();
    response.locations.retain(|location| {
        location.path.len() <= MAX_PATH_BYTES
            && location.start.line > 0
            && location.start.column > 0
            && location.end.line > 0
            && location.end.column > 0
    });
    response.truncated |= response.locations.len() != count;
    if response.locations.len() > MAX_RESULTS {
        response.locations.truncate(MAX_RESULTS);
        response.truncated = true;
    }
    loop {
        if serde_json::to_vec(&response)
            .map_err(|_| BudgetTooSmall)?
            .len()
            <= budget
        {
            return Ok(response);
        }
        if response.locations.pop().is_none() {
            return Err(BudgetTooSmall);
        }
        response.truncated = true;
    }
}

/// Read a length-prefixed UTF-8 JSON frame, rejecting size before allocation.
/// Callers own their admission and whole-frame I/O deadline.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R, limit: usize) -> io::Result<Vec<u8>> {
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid navigation frame size",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    limit: usize,
) -> io::Result<()> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "navigation frame too large"))?;
    if bytes.is_empty() || bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid navigation frame size",
        ));
    }
    writer.write_u32(length).await?;
    writer.write_all(bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn fits_escaped_paths_and_marks_omission() {
        let location = Location {
            path: "a\\\"".repeat(100),
            start: Position { line: 1, column: 1 },
            end: Position { line: 1, column: 2 },
        };
        let result =
            fit_response(NavigationResponse::ok(vec![location; 100]), 1024).expect("status fits");
        assert!(result.truncated);
        assert!(serde_json::to_vec(&result).expect("serialize").len() <= 1024);
    }

    #[test]
    fn tiny_budget_cannot_return_an_oversized_status() {
        assert_eq!(
            fit_response(NavigationResponse::ok(Vec::new()), 0),
            Err(BudgetTooSmall)
        );
        let result = fit_response(
            NavigationResponse::status(Status::Unavailable, "x".repeat(20000)),
            256,
        )
        .expect("bounded status fits");
        assert!(serde_json::to_vec(&result).expect("serialize").len() <= 256);
    }

    #[tokio::test]
    async fn frame_round_trip_and_preallocation_limit() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, b"{}", 2).await.expect("frame");
        assert_eq!(
            read_frame(&mut bytes.as_slice(), 2).await.expect("read"),
            b"{}"
        );
        assert!(read_frame(&mut bytes.as_slice(), 1).await.is_err());
    }
}
