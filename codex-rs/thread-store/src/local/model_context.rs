//! Reconstructs model context and preserves source runtime metadata across fork cutoffs.

use std::io;
use std::path::Path;

use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::ReverseJsonlScanner;
use codex_rollout::RolloutItem;
use codex_rollout::ScanOutcome;

use super::LocalThreadStore;
use super::read_thread;
use super::rollout_lineage::RolloutLineage;
use super::thread_rollout_resolver;
use crate::LoadThreadHistoryParams;
use crate::StoredModelContext;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

#[cfg(test)]
#[path = "model_context_tests.rs"]
mod tests;

/// Loads rollout items needed to reconstruct the latest model-visible context.
///
/// Paginated JSONL rollouts use a reverse scan. It stops at the newest `CompactedItem` with both
/// replacement history and a window number, and returns that compaction plus its newer suffix. If
/// the newest compaction lacks either field, the scan continues to the beginning of the rollout.
///
/// Compressed segments are decoded before applying their original JSONL offsets. Legacy rollouts
/// keep the existing full-history path.
pub(super) async fn load_latest_model_context(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
) -> ThreadStoreResult<StoredModelContext> {
    let resolved = if params.include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, params.thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, params.thread_id).await?
    };
    let path =
        resolved
            .map(|resolved| resolved.path)
            .ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!("no rollout found for thread id {}", params.thread_id),
            })?;

    load_from_rollout_path(store, params.thread_id, &path).await
}

pub(super) async fn load_from_rollout_path(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
    path: &Path,
) -> ThreadStoreResult<StoredModelContext> {
    let before = super::history_revision::read(path).await;
    let session_meta = codex_rollout::read_session_meta_line(path)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read session metadata {}: {err}", path.display()),
        })?;
    if session_meta.meta.id != thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "rollout at {} belongs to thread {}, not {}",
                path.display(),
                session_meta.meta.id,
                thread_id
            ),
        });
    }

    let items = if matches!(session_meta.meta.history_mode, ThreadHistoryMode::Paginated) {
        let lineage = store
            .resolve_rollout_lineage(thread_id, Some(path.to_path_buf()))
            .await?;
        scan_model_context_from_lineage(lineage, session_meta).await?
    } else {
        read_thread::load_history_items(path).await?
    };

    let after = super::history_revision::read(path).await;
    Ok(StoredModelContext {
        revision: before.filter(|revision| Some(revision) == after.as_ref()),
        thread_id,
        items,
    })
}

/// Loads startup context from a fork's frozen inherited prefix.
pub(super) async fn load_for_fork(
    lineage: RolloutLineage,
    history_base: Option<HistoryPosition>,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let source_path = lineage
        .segments()
        .last()
        .map(|segment| segment.rollout_path.as_path())
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "fork lineage has no source segment".to_string(),
        })?;
    let mut session_meta = codex_rollout::read_session_meta_line(source_path)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to read session metadata {}: {err}",
                source_path.display()
            ),
        })?;
    if session_meta.meta.multi_agent_version.is_none() {
        // Recover only the runtime version before applying the fork cutoff. Stop at the
        // newest version-bearing context instead of retaining the source's full replay.
        let source_lineage = lineage.clone();
        let (stop, keep_running) = tokio::sync::oneshot::channel::<()>();
        session_meta.meta.multi_agent_version = tokio::task::spawn_blocking(move || {
            for segment in source_lineage.segments().iter().rev() {
                let file = codex_rollout::open_rollout_seekable_reader(
                    segment.rollout_path.as_path(),
                    || stop.is_closed(),
                )?;
                let mut scanner = match segment.end.map(|end| end.end_byte_offset) {
                    Some(end_byte_offset) => ReverseJsonlScanner::new_at(file, end_byte_offset)?,
                    None => ReverseJsonlScanner::new(file)?,
                };
                while let Some(outcome) = scanner.scan_next_rollout_line()? {
                    let ScanOutcome::Parsed(line) = outcome else {
                        continue;
                    };
                    if let Some(version) = codex_rollout::resume_multi_agent_version(&line.item) {
                        return Ok(Some(version));
                    }
                    // Ancestor metadata does not describe the immediate source's runtime.
                    if matches!(line.item, RolloutItem::SessionMeta(_)) {
                        break;
                    }
                }
            }
            Ok::<_, io::Error>(None)
        })
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to join fork runtime version scan: {err}"),
        })?
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read fork runtime version: {err}"),
        })?;
        drop(keep_running);
    }
    match history_base {
        Some(history_base) => {
            let lineage = lineage.truncate_at(history_base).await?;
            scan_model_context_from_lineage(lineage, session_meta).await
        }
        None => Ok(vec![RolloutItem::SessionMeta(Box::new(session_meta))]),
    }
}

async fn scan_model_context_from_lineage(
    lineage: RolloutLineage,
    session_meta: SessionMetaLine,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let (stop, keep_running) = tokio::sync::oneshot::channel::<()>();
    let scan = tokio::task::spawn_blocking(move || {
        scan_model_context_from_lineage_blocking(&lineage, session_meta, || stop.is_closed())
    })
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to join model context scan: {err}"),
    })?;
    drop(keep_running);
    match scan {
        Ok(items) => Ok(items),
        Err(err) => Err(ThreadStoreError::Internal {
            message: format!("failed to scan paginated model context lineage: {err}"),
        }),
    }
}

fn scan_model_context_from_lineage_blocking(
    lineage: &RolloutLineage,
    session_meta: SessionMetaLine,
    is_cancelled: impl Fn() -> bool,
) -> io::Result<Vec<RolloutItem>> {
    let mut scan = ModelContextScan::default();
    'segments: for segment in lineage.segments().iter().rev() {
        let file = codex_rollout::open_rollout_seekable_reader(
            segment.rollout_path.as_path(),
            &is_cancelled,
        )?;
        let mut scanner = match segment.end.map(|end| end.end_byte_offset) {
            Some(end_byte_offset) => ReverseJsonlScanner::new_at(file, end_byte_offset)?,
            None => ReverseJsonlScanner::new(file)?,
        };
        while let Some(outcome) = scanner.scan_next_rollout_line()? {
            let ScanOutcome::Parsed(line) = outcome else {
                continue;
            };
            // Each rollout segment contributes only its local delta. Its session metadata is
            // replaced with the requested thread's canonical SessionMeta after replay.
            if matches!(&line.item, RolloutItem::SessionMeta(_)) {
                break;
            }
            match scan.push(line.item) {
                ModelContextScanProgress::Continue => {}
                ModelContextScanProgress::Complete => break 'segments,
            }
        }
    }

    let mut items = scan.finish();
    items.insert(0, RolloutItem::SessionMeta(Box::new(session_meta)));
    Ok(items)
}

/// Finds the latest terminal event for an exact turn, even before a compaction boundary.
pub(super) async fn load_turn_terminal_event(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
    turn_id: String,
) -> ThreadStoreResult<Option<EventMsg>> {
    let resolved = if params.include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, params.thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, params.thread_id).await?
    }
    .ok_or_else(|| ThreadStoreError::InvalidRequest {
        message: format!("no rollout found for thread id {}", params.thread_id),
    })?;
    let lineage = store
        .resolve_rollout_lineage(params.thread_id, Some(resolved.path))
        .await?;
    let (stop, keep_running) = tokio::sync::oneshot::channel::<()>();
    let scan = tokio::task::spawn_blocking(move || {
        scan_turn_terminal_event(&lineage, &turn_id, || stop.is_closed())
    })
    .await
    .map_err(|error| ThreadStoreError::Internal {
        message: format!("failed to join terminal event scan: {error}"),
    })?;
    drop(keep_running);
    scan.map_err(|error| ThreadStoreError::Internal {
        message: format!("failed to scan turn terminal event: {error}"),
    })
}

fn scan_turn_terminal_event(
    lineage: &RolloutLineage,
    turn_id: &str,
    is_cancelled: impl Fn() -> bool,
) -> io::Result<Option<EventMsg>> {
    // Status restoration is best effort: missing/corrupt turn IDs must not replay
    // an entire archived lineage. The deadline also bounds compressed decoding.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let is_cancelled = || is_cancelled() || std::time::Instant::now() >= deadline;
    let mut remaining_records = 4096;
    for segment in lineage.segments().iter().rev() {
        let file = codex_rollout::open_rollout_seekable_reader(
            segment.rollout_path.as_path(),
            &is_cancelled,
        )?;
        let mut scanner = match segment.end.map(|end| end.end_byte_offset) {
            Some(end) => ReverseJsonlScanner::new_at(file, end)?,
            None => ReverseJsonlScanner::new(file)?,
        }
        .with_max_record_bytes(1024 * 1024);
        while let Some(outcome) = scanner.scan_next_rollout_line()? {
            if is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "terminal event scan cancelled",
                ));
            }
            if remaining_records == 0 {
                return Err(io::Error::other(
                    "terminal event scan record limit exceeded",
                ));
            }
            remaining_records -= 1;
            let ScanOutcome::Parsed(line) = outcome else {
                continue;
            };
            if line
                .ordinal
                .is_some_and(|ordinal| ordinal < segment.start_ordinal)
            {
                break;
            }
            let event = match line.item {
                RolloutItem::EventMsg(event) => event,
                RolloutItem::SessionMeta(_) => break,
                _ => continue,
            };
            let event_turn_id = match &event {
                EventMsg::TurnComplete(turn) => Some(turn.turn_id.as_str()),
                EventMsg::TurnAborted(turn) => turn.turn_id.as_deref(),
                // The caller asks about the newest projected turn. Reaching any
                // opening boundary without its terminal means the projection is
                // stale or incomplete; older turns cannot supply its result.
                EventMsg::TurnStarted(_) => return Ok(None),
                _ => None,
            };
            if event_turn_id == Some(turn_id) {
                return Ok(Some(event));
            }
        }
    }
    Ok(None)
}
