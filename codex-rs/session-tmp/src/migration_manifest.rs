//! Durable recovery migration manifest discovery.

use super::state;
use super::storage;
use super::read_manifest;
use super::MIGRATION_MANIFEST_PREFIX;
use super::MIGRATION_MANIFEST_SUFFIX;
use std::fs;
use std::path::Path;

/// Returns whether a target manifest still points at the historical recovery
/// path. This lets startup resume a state-only retirement after the recovery
/// payload directory has already disappeared.
pub(super) fn recovery_manifest_pending(default_root: &Path) -> bool {
    let normal_root = default_root.join("session-tmp");
    let Ok(canonical_normal) = state::canonicalize_for_identity(&normal_root) else {
        return false;
    };
    let target_root_id = state::root_id(&canonical_normal);
    let state_base = default_root
        .join(state::STATE_DIR)
        .join(state::STATE_SESSION_TMP_DIR);
    let target_state_root = state_base.join(&target_root_id);
    let recovery_root = default_root.join(state::LEGACY_RECOVERY_ROOT);
    let Ok(canonical_recovery) = state::canonicalize_for_identity(&recovery_root) else {
        return false;
    };
    let Ok(entries) = fs::read_dir(target_state_root) else {
        return false;
    };
    entries.take(32).filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if !name.starts_with(MIGRATION_MANIFEST_PREFIX)
            || !name.ends_with(MIGRATION_MANIFEST_SUFFIX)
        {
            return false;
        }
        let Some(manifest) = read_manifest(&path) else {
            return false;
        };
        if manifest.schema_version != 1
            || manifest.target_root_id != target_root_id
            || manifest.phase == "complete"
            || storage::validate_component(&manifest.source_root_id).is_err()
            || manifest.moved_paths.iter().any(|movement| {
                !super::records::valid_manifest_path(&movement.source)
                    || !super::records::valid_manifest_path(&movement.target)
            })
        {
            return false;
        }
        let source_state_root = state_base.join(&manifest.source_root_id);
        let source_payload_root = manifest.source_payload_root.or_else(|| {
            state::payload_root_from_state_root(&source_state_root, &manifest.source_root_id)
                .ok()
                .flatten()
        });
        source_payload_root
            .and_then(|path| state::canonicalize_for_identity(&path).ok())
            .is_some_and(|path| path == canonical_recovery)
    })
}
