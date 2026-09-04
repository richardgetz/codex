use std::io::SeekFrom;
use std::path::Path;

use chrono::DateTime;
use codex_app_server_protocol::ThreadHistoryChangeSet;
use codex_app_server_protocol::project_rollout_line;
use codex_protocol::ThreadId;
use codex_rollout::RolloutItem;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::BufReader;
use tracing::warn;

use super::LocalThreadStore;
use super::thread_history::ProjectedRolloutLine;
use super::thread_history::RolloutProjectionStep;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

const PROJECTION_BATCH_BYTES: u64 = 256 * 1024;
const MAX_ROLLOUT_LINE_BYTES: usize = 16 * 1024 * 1024;
const ROLLOUT_RECORD_DISCARD_CHUNK_BYTES: u64 = 8 * 1024;

pub(super) async fn materialize_to_sqlite(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<()> {
    if store.state_db.is_none() {
        return Ok(());
    }
    let projection_state = super::thread_history::projection_state(store, thread_id).await?;
    let start_offset = projection_state
        .as_ref()
        .map_or(0, |state| state.next_byte_offset);
    if projection_state.is_none()
        && codex_rollout::existing_rollout_path(rollout_path)
            .await
            .is_none()
    {
        return Ok(());
    }
    let session_meta = codex_rollout::read_session_meta_line(rollout_path)
        .await
        .map_err(thread_store_io_error)?
        .meta;
    let initial_ordinal = session_meta
        .history_base
        .map_or(0, |base| base.end_ordinal_exclusive);
    let subagent_history_start_ordinal = session_meta.subagent_history_start_ordinal;
    let expected_ordinal = projection_state
        .as_ref()
        .map_or(initial_ordinal, |state| state.next_ordinal);
    let path = rollout_path.to_path_buf();
    let file =
        tokio::task::spawn_blocking(move || codex_rollout::open_rollout_seekable_reader(&path))
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to join rollout projection read: {err}"),
            })?;
    let file = match file {
        Ok(file) => tokio::fs::File::from_std(file),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && start_offset == 0 => {
            return Ok(());
        }
        Err(err) => return Err(thread_store_io_error(err)),
    };
    let file_end_offset = file.metadata().await.map_err(thread_store_io_error)?.len();
    if file_end_offset < start_offset {
        return Err(ThreadStoreError::Internal {
            message: "durable rollout shrank before projection".to_string(),
        });
    }
    let mut file = BufReader::with_capacity(PROJECTION_BATCH_BYTES as usize, file);
    file.seek(SeekFrom::Start(start_offset))
        .await
        .map_err(thread_store_io_error)?;
    let mut line_bytes =
        Vec::with_capacity(MAX_ROLLOUT_LINE_BYTES.min(PROJECTION_BATCH_BYTES as usize));
    let projection_context = ProjectionReadContext {
        file_end_offset,
        rollout_path,
        thread_id,
        subagent_history_start_ordinal,
    };
    let mut start_offset = start_offset;
    let mut expected_ordinal = expected_ordinal;
    loop {
        let Some((projections, next_offset, next_ordinal)) = read_projection_steps(
            &mut file,
            &mut line_bytes,
            &projection_context,
            start_offset,
            expected_ordinal,
        )
        .await?
        else {
            return Ok(());
        };
        // Empty valid records can still consume bytes through blank complete lines.
        if projections.is_empty() && start_offset == next_offset {
            return Ok(());
        }
        super::thread_history::apply_projection(
            store,
            thread_id,
            start_offset,
            next_offset,
            initial_ordinal,
            projections,
        )
        .await?;
        start_offset = next_offset;
        expected_ordinal = next_ordinal;
    }
}

#[derive(Clone, Copy)]
struct ProjectionReadContext<'a> {
    file_end_offset: u64,
    rollout_path: &'a Path,
    thread_id: ThreadId,
    subagent_history_start_ordinal: Option<u64>,
}

async fn read_projection_steps(
    reader: &mut BufReader<tokio::fs::File>,
    line_bytes: &mut Vec<u8>,
    context: &ProjectionReadContext<'_>,
    start_offset: u64,
    expected_ordinal: u64,
) -> ThreadStoreResult<Option<(Vec<RolloutProjectionStep>, u64, u64)>> {
    let ProjectionReadContext {
        file_end_offset,
        rollout_path,
        thread_id,
        subagent_history_start_ordinal,
    } = *context;
    let mut projections = Vec::new();
    let mut next_ordinal = expected_ordinal;
    let mut next_offset = start_offset;
    let mut pending_rejected_line_count = 0_u64;
    let mut line_start_offset = start_offset;
    // Keep rejected lines pending until a later valid ordinal proves whether they consumed history.
    // This lets a same-ordinal retry replace a failed write without advancing only one checkpoint.
    loop {
        if next_offset > start_offset
            && line_start_offset.saturating_sub(start_offset) >= PROJECTION_BATCH_BYTES
        {
            rewind_projection_reader(reader, next_offset).await?;
            return Ok(Some((projections, next_offset, next_ordinal)));
        }
        let available_bytes = file_end_offset
            .checked_sub(line_start_offset)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "durable rollout byte offset overflow".to_string(),
            })?;
        let Some(record) = read_rollout_record(reader, line_bytes, available_bytes)
            .await
            .map_err(thread_store_io_error)?
        else {
            let current_end_offset = reader
                .get_ref()
                .metadata()
                .await
                .map_err(thread_store_io_error)?
                .len();
            if current_end_offset < file_end_offset {
                return Err(ThreadStoreError::Internal {
                    message: "durable rollout shrank during projection".to_string(),
                });
            }
            if next_offset != line_start_offset {
                rewind_projection_reader(reader, next_offset).await?;
            }
            return if next_offset == start_offset {
                Ok(None)
            } else {
                Ok(Some((projections, next_offset, next_ordinal)))
            };
        };
        let line_end_offset = line_start_offset
            .checked_add(record.byte_count)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "durable rollout byte offset overflow".to_string(),
            })?;
        if !record.complete {
            let current_end_offset = reader
                .get_ref()
                .metadata()
                .await
                .map_err(thread_store_io_error)?
                .len();
            if current_end_offset < file_end_offset {
                return Err(ThreadStoreError::Internal {
                    message: "durable rollout shrank during projection".to_string(),
                });
            }
            if next_offset != line_start_offset {
                rewind_projection_reader(reader, next_offset).await?;
            }
            return if next_offset == start_offset {
                Ok(None)
            } else {
                Ok(Some((projections, next_offset, next_ordinal)))
            };
        }
        if record.oversized {
            warn!(
                thread_id = %thread_id,
                rollout_path = %rollout_path.display(),
                line_start_byte_offset = line_start_offset,
                line_end_byte_offset = line_end_offset,
                expected_ordinal = next_ordinal,
                max_line_bytes = MAX_ROLLOUT_LINE_BYTES,
                "deferring oversized rollout line until a later ordinal resolves it"
            );
            pending_rejected_line_count = pending_rejected_line_count.saturating_add(1);
            line_start_offset = line_end_offset;
            continue;
        }
        if line_bytes.iter().all(u8::is_ascii_whitespace) {
            if pending_rejected_line_count == 0 {
                next_offset = line_end_offset;
            }
            line_start_offset = line_end_offset;
            continue;
        }
        let value = match serde_json::from_slice::<serde_json::Value>(line_bytes) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    thread_id = %thread_id,
                    rollout_path = %rollout_path.display(),
                    line_start_byte_offset = line_start_offset,
                    line_end_byte_offset = line_end_offset,
                    expected_ordinal = next_ordinal,
                    error = %err,
                    "deferring rejected rollout line until a later ordinal resolves it"
                );
                pending_rejected_line_count = pending_rejected_line_count.saturating_add(1);
                line_start_offset = line_end_offset;
                continue;
            }
        };
        let value_ordinal = value.get("ordinal").and_then(serde_json::Value::as_u64);
        let line = match codex_rollout::decode_rollout_line(value) {
            Ok(line) => Some(line),
            Err(err) => {
                warn!(
                    thread_id = %thread_id,
                    rollout_path = %rollout_path.display(),
                    line_start_byte_offset = line_start_offset,
                    line_end_byte_offset = line_end_offset,
                    expected_ordinal = next_ordinal,
                    line_ordinal = ?value_ordinal,
                    error = %err,
                    "deferring unknown rollout line until a later ordinal resolves it"
                );
                None
            }
        };
        let ordinal = match line
            .as_ref()
            .and_then(|line| line.ordinal)
            .or(value_ordinal)
        {
            Some(ordinal) => ordinal,
            None if line.is_none() => {
                pending_rejected_line_count = pending_rejected_line_count.saturating_add(1);
                line_start_offset = line_end_offset;
                continue;
            }
            None => {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "paginated rollout line for {thread_id} is missing an ordinal"
                    ),
                });
            }
        };
        if ordinal < next_ordinal {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "thread history projection for {thread_id} expected ordinal {next_ordinal}, got {ordinal}"
                ),
            });
        }
        let Some(line) = line else {
            pending_rejected_line_count = pending_rejected_line_count.saturating_add(1);
            line_start_offset = line_end_offset;
            continue;
        };
        let skipped_ordinal_count = ordinal - next_ordinal;
        if skipped_ordinal_count > pending_rejected_line_count {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "thread history projection for {thread_id} expected ordinal {next_ordinal}, got {ordinal}; {pending_rejected_line_count} rejected rollout lines cannot cover that gap"
                ),
            });
        }
        let is_inherited_subagent_history =
            subagent_history_start_ordinal.is_some_and(|start| ordinal < start);
        let changes = if is_inherited_subagent_history {
            ThreadHistoryChangeSet::default()
        } else {
            project_rollout_line(&line)
        };
        let fallback_created_at_ms = if changes
            .changed_items
            .iter()
            .any(|item| item.started_at_ms.is_none())
            || (!is_inherited_subagent_history
                && matches!(&line.item, RolloutItem::RealtimeItem(_)))
        {
            match DateTime::parse_from_rfc3339(line.timestamp.as_str()) {
                Ok(timestamp) => Some(timestamp.timestamp_millis()),
                Err(err) => {
                    warn!(
                        thread_id = %thread_id,
                        rollout_path = %rollout_path.display(),
                        line_start_byte_offset = line_start_offset,
                        line_end_byte_offset = line_end_offset,
                        expected_ordinal = next_ordinal,
                        line_ordinal = ordinal,
                        error = %err,
                        "deferring rollout line with invalid timestamp until a later ordinal resolves it"
                    );
                    pending_rejected_line_count = pending_rejected_line_count.saturating_add(1);
                    line_start_offset = line_end_offset;
                    continue;
                }
            }
        } else {
            None
        };
        if skipped_ordinal_count > 0 {
            warn!(
                thread_id = %thread_id,
                rollout_path = %rollout_path.display(),
                line_start_byte_offset = line_start_offset,
                line_end_byte_offset = line_end_offset,
                expected_ordinal = next_ordinal,
                line_ordinal = ordinal,
                skipped_ordinal_start = next_ordinal,
                skipped_ordinal_end_exclusive = ordinal,
                "skipping rollout ordinal range after rejected lines"
            );
            projections.push(RolloutProjectionStep::SkippedOrdinalRange {
                start_ordinal: next_ordinal,
                end_ordinal_exclusive: ordinal,
            });
        }
        pending_rejected_line_count = 0;
        let next_line_ordinal =
            ordinal
                .checked_add(1)
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "rollout ordinal exceeds SQLite integer range".to_string(),
                })?;
        projections.push(RolloutProjectionStep::Line(Box::new(
            ProjectedRolloutLine {
                ordinal,
                start_byte_offset: line_start_offset,
                end_byte_offset: line_end_offset,
                fallback_created_at_ms,
                changes,
                realtime_item: match line.item {
                    RolloutItem::RealtimeItem(item) if !is_inherited_subagent_history => Some(item),
                    _ => None,
                },
            },
        )));
        next_ordinal = next_line_ordinal;
        next_offset = line_end_offset;
        line_start_offset = line_end_offset;
    }
}

async fn rewind_projection_reader(
    reader: &mut BufReader<tokio::fs::File>,
    offset: u64,
) -> ThreadStoreResult<()> {
    reader
        .seek(SeekFrom::Start(offset))
        .await
        .map_err(thread_store_io_error)
        .map(|_| ())
}

struct ProjectionRecord {
    byte_count: u64,
    complete: bool,
    oversized: bool,
}

async fn read_rollout_record(
    reader: &mut BufReader<tokio::fs::File>,
    line_bytes: &mut Vec<u8>,
    available_bytes: u64,
) -> std::io::Result<Option<ProjectionRecord>> {
    const MAX_READ_BYTES: u64 = (MAX_ROLLOUT_LINE_BYTES + 1) as u64;

    line_bytes.clear();
    let mut discarded_bytes = Vec::with_capacity(ROLLOUT_RECORD_DISCARD_CHUNK_BYTES as usize);
    let mut available_bytes = available_bytes;
    let mut byte_count = 0_u64;
    let mut oversized = false;
    loop {
        if available_bytes == 0 {
            break;
        }
        let (read, ended_with_newline) = if oversized {
            discarded_bytes.clear();
            let read_limit = ROLLOUT_RECORD_DISCARD_CHUNK_BYTES.min(available_bytes);
            let read = reader
                .take(read_limit)
                .read_until(b'\n', &mut discarded_bytes)
                .await?;
            (read, discarded_bytes.last() == Some(&b'\n'))
        } else {
            let remaining_line_capacity = MAX_READ_BYTES.saturating_sub(line_bytes.len() as u64);
            if remaining_line_capacity == 0 {
                oversized = true;
                line_bytes.clear();
                continue;
            }
            let read_limit = remaining_line_capacity.min(available_bytes);
            let read = reader
                .take(read_limit)
                .read_until(b'\n', line_bytes)
                .await?;
            (read, line_bytes.last() == Some(&b'\n'))
        };
        if read == 0 {
            break;
        }
        let read = u64::try_from(read).map_err(std::io::Error::other)?;
        byte_count = byte_count
            .checked_add(read)
            .ok_or_else(|| std::io::Error::other("rollout record byte count overflow"))?;
        available_bytes -= read;
        if ended_with_newline {
            let oversized = oversized || line_bytes.len() > MAX_ROLLOUT_LINE_BYTES;
            if oversized {
                line_bytes.clear();
            }
            return Ok(Some(ProjectionRecord {
                byte_count,
                complete: true,
                oversized,
            }));
        }
        if !oversized && line_bytes.len() > MAX_ROLLOUT_LINE_BYTES {
            oversized = true;
            line_bytes.clear();
        }
    }
    if byte_count == 0 {
        Ok(None)
    } else {
        Ok(Some(ProjectionRecord {
            byte_count,
            complete: false,
            oversized,
        }))
    }
}

fn thread_store_io_error(err: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: err.to_string(),
    }
}

#[cfg(test)]
#[path = "thread_history_materialization_tests.rs"]
mod tests;
