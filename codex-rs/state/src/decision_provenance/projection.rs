use super::PROVENANCE_PROJECTION_VERSION;
use super::model::ChangeSet;
use super::model::Crossroad;
use super::model::Decision;
use super::model::PreferenceBoundary;
use super::model::ProvenanceNotification;
use super::model::ProvenanceRelationship;
use super::model::Scope;
use super::model::Warrant;
use super::query::ProvenanceIndexes;
use super::query::ProvenanceProjection;
use chrono::DateTime;
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub(crate) fn build_projection(
    decisions: Vec<Decision>,
    crossroads: Vec<Crossroad>,
    preference_boundaries: Vec<PreferenceBoundary>,
    warrants: Vec<Warrant>,
    change_sets: Vec<ChangeSet>,
    relationships: Vec<ProvenanceRelationship>,
    notifications: Vec<ProvenanceNotification>,
) -> ProvenanceProjection {
    let mut indexes = ProvenanceIndexes::default();
    for decision in &decisions {
        index(&mut indexes.decision_id, &decision.id, &decision.id);
        index_optional(
            &mut indexes.crossroad_id,
            decision.parent_crossroad_id.as_deref(),
            &decision.id,
        );
        index_optional(
            &mut indexes.session_id,
            decision.source_session_id.as_deref(),
            &decision.id,
        );
        index_optional(
            &mut indexes.repository,
            decision.repository.as_deref(),
            &decision.id,
        );
        index_optional(
            &mut indexes.project,
            decision.project_ref.as_deref(),
            &decision.id,
        );
        index(
            &mut indexes.timestamp,
            &date_key(decision.timestamps.recorded_at),
            &decision.id,
        );
        index(&mut indexes.status, decision.status.as_str(), &decision.id);
        index(&mut indexes.actor, decision.actor.as_str(), &decision.id);
    }
    for crossroad in &crossroads {
        index(&mut indexes.crossroad_id, &crossroad.id, &crossroad.id);
        index_optional(
            &mut indexes.session_id,
            crossroad.session_id.as_deref(),
            &crossroad.id,
        );
        index_optional(
            &mut indexes.project,
            crossroad.project_ref.as_deref(),
            &crossroad.id,
        );
        index(
            &mut indexes.timestamp,
            &date_key(crossroad.timestamps.recorded_at),
            &crossroad.id,
        );
        index(
            &mut indexes.status,
            crossroad.status.as_str(),
            &crossroad.id,
        );
        index(&mut indexes.actor, crossroad.actor.as_str(), &crossroad.id);
    }
    for boundary in &preference_boundaries {
        index(
            &mut indexes.preference_boundary_id,
            &boundary.id,
            &boundary.id,
        );
        index(
            &mut indexes.scope,
            &scope_key(boundary.scope.kind, &boundary.scope.id),
            &boundary.id,
        );
        index(
            &mut indexes.timestamp,
            &date_key(boundary.timestamps.recorded_at),
            &boundary.id,
        );
        index(
            &mut indexes.status,
            boundary.lifecycle_status.as_str(),
            &boundary.id,
        );
        index(
            &mut indexes.actor,
            boundary.authority.as_str(),
            &boundary.id,
        );
    }
    for change_set in &change_sets {
        index_optional(
            &mut indexes.session_id,
            change_set.session_id.as_deref(),
            &change_set.id,
        );
        index_optional(
            &mut indexes.commit_sha,
            change_set.commit_sha.as_deref(),
            &change_set.id,
        );
        index_optional(
            &mut indexes.pull_request,
            change_set.pull_request.as_deref(),
            &change_set.id,
        );
        index(
            &mut indexes.timestamp,
            &date_key(change_set.timestamps.recorded_at),
            &change_set.id,
        );
    }
    for notification in &notifications {
        index(
            &mut indexes.status,
            notification.category.as_str(),
            &notification.id,
        );
        index(
            &mut indexes.timestamp,
            &date_key(notification.created_at),
            &notification.id,
        );
        index_optional(
            &mut indexes.preference_boundary_id,
            notification.preference_boundary_id.as_deref(),
            &notification.id,
        );
        index_optional(
            &mut indexes.decision_id,
            notification.decision_id.as_deref(),
            &notification.id,
        );
    }

    ProvenanceProjection {
        schema_version: PROVENANCE_PROJECTION_VERSION,
        generated_at: Utc::now(),
        read_only: true,
        source_event_id: None,
        source_event_recorded_at: None,
        truncated: false,
        decisions,
        crossroads,
        preference_boundaries,
        warrants,
        change_sets,
        relationships,
        notifications,
        indexes,
    }
}

fn index(map: &mut BTreeMap<String, Vec<String>>, key: &str, value: &str) {
    let values = map.entry(key.to_string()).or_default();
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

fn index_optional(map: &mut BTreeMap<String, Vec<String>>, key: Option<&str>, value: &str) {
    if let Some(key) = key.filter(|key| !key.is_empty()) {
        index(map, key, value);
    }
}

fn date_key(timestamp: DateTime<Utc>) -> String {
    timestamp.format("%Y-%m-%d").to_string()
}

fn scope_key(scope: Scope, id: &str) -> String {
    format!("{}:{id}", scope.as_str())
}

pub(crate) async fn write_projection_atomically(
    path: &Path,
    projection: &ProvenanceProjection,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("provenance projection has no parent directory"))?;
    tokio::fs::create_dir_all(parent).await?;
    let bytes = serde_json::to_vec_pretty(projection)?;
    let temporary_path = parent.join(format!(".projection-v1.{}.tmp", Uuid::now_v7()));
    let write_result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .await?;
        file.write_all(&bytes).await?;
        file.write_all(b"\n").await?;
        file.sync_all().await?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&temporary_path, std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        replace_projection_file(&temporary_path, path).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if write_result.is_err() {
        let _ = tokio::fs::remove_file(&temporary_path).await;
    }
    write_result
}

#[cfg(not(windows))]
async fn replace_projection_file(temporary_path: &Path, path: &Path) -> anyhow::Result<()> {
    tokio::fs::rename(temporary_path, path).await?;
    Ok(())
}

#[cfg(windows)]
async fn replace_projection_file(temporary_path: &Path, path: &Path) -> anyhow::Result<()> {
    // `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` replaces the destination as one filesystem
    // operation. Removing the old projection first would leave Inbound with a missing snapshot
    // and would allow a crash between the remove and rename to lose the last valid projection.
    replace_projection_file_sync(temporary_path, path)?;
    Ok(())
}

#[cfg(windows)]
fn replace_projection_file_sync(temporary_path: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let temporary_path = temporary_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both vectors are NUL-terminated UTF-16 paths that remain alive for the call, and
    // the flags request an atomic replacement with write-through semantics.
    let result = unsafe {
        MoveFileExW(
            temporary_path.as_ptr(),
            path.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
