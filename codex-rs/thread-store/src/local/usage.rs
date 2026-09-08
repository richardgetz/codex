//! Complete token-usage reads for billing projections.
//!
//! Model-context reads may stop at a latest compaction checkpoint. Usage reads have a different
//! contract: they scan every physical rollout segment contributing to the selected thread so
//! callers can reconstruct spend after a cold resume.

use codex_protocol::protocol::ThreadHistoryMode;

use super::LocalThreadStore;
use super::thread_rollout_resolver;
use crate::LoadThreadHistoryParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::store::enrich_token_usage_records;
use crate::store::token_usage_records_from_rollout_items;

pub(super) async fn load_token_usage_records(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
) -> ThreadStoreResult<Vec<codex_protocol::protocol::TokenUsageRecord>> {
    let resolved = if params.include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, params.thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, params.thread_id).await?
    }
    .ok_or_else(|| ThreadStoreError::ThreadNotFound {
        thread_id: params.thread_id,
    })?;
    let meta = codex_rollout::read_session_meta_line(resolved.path.as_path())
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to read usage metadata {}: {err}",
                resolved.path.display()
            ),
        })?;

    if meta.meta.history_mode != ThreadHistoryMode::Paginated {
        let (lines, _, parse_errors) =
            codex_rollout::RolloutRecorder::load_rollout_lines(resolved.path.as_path())
                .await
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!(
                        "failed to load usage history {}: {err}",
                        resolved.path.display()
                    ),
                })?;
        if parse_errors > 0 {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "usage history {} contains {parse_errors} malformed rollout lines",
                    resolved.path.display()
                ),
            });
        }
        let items: Vec<_> = lines.into_iter().map(|line| line.item).collect();
        let records = items
            .iter()
            .flat_map(|item| token_usage_records_from_rollout_items(std::slice::from_ref(item)))
            .collect();
        return Ok(enrich_token_usage_records(records, &items, None));
    }

    let lineage = store.resolve_rollout_lineage(params.thread_id).await?;
    let mut records = Vec::new();
    let mut items = Vec::new();
    for segment in lineage.segments() {
        let (lines, _, parse_errors) =
            codex_rollout::RolloutRecorder::load_rollout_lines(segment.rollout_path.as_path())
                .await
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!(
                        "failed to load usage history {}: {err}",
                        segment.rollout_path.display()
                    ),
                })?;
        if parse_errors > 0 {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "usage history {} contains {parse_errors} malformed rollout lines",
                    segment.rollout_path.display()
                ),
            });
        }
        for line in lines {
            // The lineage range starts after each segment's SessionMeta ordinal. Keep that
            // metadata in the enrichment stream even when it is outside the selected range so
            // legacy records can use the provider captured by their own rollout segment.
            let item = line.item;
            if matches!(&item, codex_rollout::RolloutItem::SessionMeta(_)) {
                items.push(item.clone());
            }
            let ordinal = line.ordinal.ok_or_else(|| ThreadStoreError::Internal {
                message: format!(
                    "paginated usage line in {} is missing an ordinal",
                    segment.rollout_path.display()
                ),
            })?;
            if ordinal < segment.start_ordinal
                || segment
                    .end
                    .is_some_and(|end| ordinal >= end.end_ordinal_exclusive)
            {
                continue;
            }
            records.extend(token_usage_records_from_rollout_items(
                std::slice::from_ref(&item),
            ));
            items.push(item);
        }
    }
    Ok(enrich_token_usage_records(records, &items, None))
}
