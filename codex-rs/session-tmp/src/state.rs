//! Durable control state for session temporary storage.
//!
//! Payload directories are user-disposable.  This module keeps ownership,
//! metadata, leases, and coordination files in a per-payload-root state tree
//! under the Codex home so deleting a payload root cannot erase liveness data.

use super::storage;
use super::SessionTmpError;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

pub(super) const STATE_DIR: &str = "state";
pub(super) const STATE_SESSION_TMP_DIR: &str = "session-tmp";
pub(super) const STATE_MARKER: &str = ".codex-managed-session-tmp-state";
pub(super) const STATE_MARKER_CONTENT: &str =
    "codex managed session temporary control state\nschema_version=1\n";
pub(super) const STATE_ROOT_RECORD: &str = "root.json";
pub(super) const STATE_SESSIONS_DIR: &str = "sessions";
pub(super) const STATE_LOCKS_DIR: &str = ".locks";
pub(super) const LEGACY_MARKER: &str = ".codex-managed-session-tmp";
pub(super) const LEGACY_MARKER_CONTENT: &str =
    "codex managed session temporary storage\nschema_version=1\n";
pub(super) const LEGACY_RECOVERY_ROOT: &str = "session-tmp-recovery";
pub(super) const V2_PAYLOAD_NAMESPACE: &str = ".codex-session-tmp-v2";

#[path = "state_identity.rs"]
mod identity;
#[path = "state_legacy.rs"]
mod legacy;

pub(super) use identity::{
    canonicalize_for_identity, ensure_existing_ancestors_for_runtime, external_identity_present,
    inspect_payload_root, is_real_directory, payload_is_nonempty, payload_root_from_state_root,
    root_id,
};

#[derive(Clone, Debug)]
pub(super) struct ControlState {
    pub(super) payload_root: PathBuf,
    pub(super) canonical_payload_root: PathBuf,
    pub(super) payload_namespace: PathBuf,
    pub(super) state_root: PathBuf,
    pub(super) state_base: PathBuf,
    pub(super) root_id: String,
    pub(super) legacy_managed: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct RootRecord {
    schema_version: u8,
    root_id: String,
    payload_root: PathBuf,
    canonical_payload_root: PathBuf,
    payload_namespace: PathBuf,
}

impl ControlState {
    pub(super) fn open(
        default_root: &Path,
        payload_root: &Path,
    ) -> Result<Self, SessionTmpError> {
        Self::open_with_options(
            default_root,
            payload_root,
            /*allow_nonempty_without_legacy_marker*/ false,
            /*import_legacy*/ true,
            /*recreate_existing_payload*/ true,
            /*wait_for_migration*/ true,
        )
    }

    /// Enrolls a markerless nonempty default payload root after a separately
    /// validated legacy root has supplied the exact managed session records.
    /// The caller must keep all unknown payloads outside the state registry;
    /// this option is never exposed through model-facing APIs or custom roots.
    pub(super) fn open_for_validated_migration(
        default_root: &Path,
        payload_root: &Path,
    ) -> Result<Self, SessionTmpError> {
        Self::open_with_options(
            default_root,
            payload_root,
            /*allow_nonempty_without_legacy_marker*/ true,
            /*import_legacy*/ true,
            /*recreate_existing_payload*/ true,
            /*wait_for_migration*/ true,
        )
    }

    /// Opens a legacy source for migration without copying its control files
    /// until the caller holds the source migration lock.
    pub(super) fn open_for_migration(
        default_root: &Path,
        payload_root: &Path,
    ) -> Result<Self, SessionTmpError> {
        Self::open_with_options(
            default_root,
            payload_root,
            /*allow_nonempty_without_legacy_marker*/ false,
            /*import_legacy*/ false,
            /*recreate_existing_payload*/ false,
            /*wait_for_migration*/ false,
        )
    }

    fn open_with_options(
        default_root: &Path,
        payload_root: &Path,
        allow_nonempty_without_legacy_marker: bool,
        import_legacy: bool,
        recreate_existing_payload: bool,
        wait_for_migration: bool,
    ) -> Result<Self, SessionTmpError> {
        if !default_root.is_absolute() {
            return Err(SessionTmpError::RootNotAbsolute(default_root.to_path_buf()));
        }
        if !payload_root.is_absolute() {
            return Err(SessionTmpError::RootNotAbsolute(payload_root.to_path_buf()));
        }
        identity::ensure_existing_ancestors_for_runtime(default_root)?;
        identity::ensure_existing_ancestors_for_runtime(payload_root)?;
        storage::ensure_directory_not_symlink(default_root)?;
        fs::create_dir_all(default_root)?;
        storage::ensure_directory_not_symlink(default_root)?;

        let canonical_payload_root = identity::canonicalize_for_identity(payload_root)?;
        let state_base = default_root.join(STATE_DIR).join(STATE_SESSION_TMP_DIR);
        identity::ensure_existing_ancestors_for_runtime(&state_base)?;
        let canonical_state_base = identity::canonicalize_for_identity(&state_base)?;
        if identity::paths_overlap(&canonical_payload_root, &canonical_state_base) {
            return Err(SessionTmpError::UnsafeManagedPath(state_base));
        }
        storage::ensure_directory_not_symlink(&state_base)?;
        fs::create_dir_all(&state_base)?;
        storage::ensure_directory_not_symlink(&state_base)?;
        storage::set_private_directory(&state_base)?;

        let root_id = identity::root_id(&canonical_payload_root);
        let state_root = state_base.join(&root_id);
        storage::ensure_directory_not_symlink(&state_root)?;
        let _migration_lock = if wait_for_migration {
            storage::ensure_directory_not_symlink(&state_base.join(".migration-locks"))?;
            storage::wait_for_migration_lock(
                &state_base
                    .join(".migration-locks")
                    .join(format!("{root_id}.lock")),
            )?
        } else {
            None
        };
        // Acquire the persistent external barrier before the historical
        // state-root lock. Migration uses this order as well, preventing a
        // recovery manager that still has an inline lock from deadlocking
        // while waiting for the new coordination domain.
        let _legacy_migration_lock = if wait_for_migration {
            storage::wait_for_migration_lock(&state_root.join(".migration.lock"))?
        } else {
            None
        };
        let legacy_managed = match identity::inspect_payload_root(payload_root) {
            Err(SessionTmpError::RootNotManaged(_)) if allow_nonempty_without_legacy_marker => {
                // A validated recovery migration may preserve a malformed
                // regular marker as unknown payload while allocating the
                // hidden managed namespace below this root. Symlinks and
                // non-directory roots remain unsafe errors.
                false
            }
            result => result?,
        };
        let existing_state = identity::inspect_state_root(&state_root, &root_id, &canonical_payload_root)?;
        let payload_namespace = match existing_state {
            Some(namespace) => {
                // A valid external identity enrolls the payload path even when its
                // disposable contents, including the old marker, have vanished.
                if recreate_existing_payload {
                    identity::ensure_payload_root(payload_root)?;
                    identity::ensure_payload_namespace(&namespace, &canonical_payload_root)?;
                }
                namespace
            }
            None => {
                if !legacy_managed
                    && identity::payload_is_nonempty(payload_root)?
                    && !allow_nonempty_without_legacy_marker
                {
                    return Err(SessionTmpError::RootNotManaged(payload_root.to_path_buf()));
                }
                identity::initialize_state_root(
                    &state_root,
                    &root_id,
                    payload_root,
                    &canonical_payload_root,
                    allow_nonempty_without_legacy_marker && !legacy_managed,
                )?;
                identity::ensure_payload_root(payload_root)?;
                let namespace = if allow_nonempty_without_legacy_marker && !legacy_managed {
                    payload_root.join(V2_PAYLOAD_NAMESPACE)
                } else {
                    payload_root.to_path_buf()
                };
                identity::ensure_payload_namespace(&namespace, &canonical_payload_root)?;
                namespace
            }
        }

        let state = Self {
            payload_root: payload_root.to_path_buf(),
            canonical_payload_root,
            payload_namespace,
            state_root,
            state_base,
            root_id,
            legacy_managed,
        };
        state.ensure_identity()?;
        if import_legacy && state.legacy_managed {
            state.import_legacy_control()?;
        }
        state.ensure_state_layout()?;
        Ok(state)
    }

    pub(super) fn payload_root(&self) -> &Path {
        &self.payload_root
    }

    pub(super) fn canonical_payload_root(&self) -> &Path {
        &self.canonical_payload_root
    }

    pub(super) fn payload_namespace(&self) -> &Path {
        &self.payload_namespace
    }

    pub(super) fn payload_sessions_dir(&self) -> PathBuf {
        self.payload_namespace.join("sessions")
    }

    pub(super) fn payload_session_dir(&self, session_id: &str) -> PathBuf {
        self.payload_sessions_dir().join(session_id)
    }

    pub(super) fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub(super) fn state_base(&self) -> &Path {
        &self.state_base
    }

    pub(super) fn default_root(&self) -> PathBuf {
        self.state_base
            .parent()
            .and_then(|state_dir| state_dir.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.state_base.clone())
    }

    pub(super) fn root_id(&self) -> &str {
        &self.root_id
    }

    pub(super) fn state_sessions_dir(&self) -> PathBuf {
        self.state_root.join(STATE_SESSIONS_DIR)
    }

    pub(super) fn state_session_dir(&self, session_id: &str) -> PathBuf {
        self.state_sessions_dir().join(session_id)
    }

    pub(super) fn state_locks_dir(&self) -> PathBuf {
        self.state_root.join(STATE_LOCKS_DIR)
    }

    pub(super) fn legacy_session_dir(&self, session_id: &str) -> PathBuf {
        self.payload_root.join("sessions").join(session_id)
    }

    pub(super) fn ensure_identity(&self) -> Result<(), SessionTmpError> {
        storage::ensure_directory_not_symlink(&self.state_base)?;
        storage::ensure_directory_not_symlink(&self.state_root)?;
        let marker = self.state_root.join(STATE_MARKER);
        if fs::symlink_metadata(&marker)
            .map(|metadata| storage::file_type_is_link(metadata.file_type()))
            .unwrap_or(false)
        {
            return Err(SessionTmpError::UnsafeManagedPath(marker));
        }
        let content = fs::read_to_string(&marker).map_err(SessionTmpError::Io)?;
        if content != STATE_MARKER_CONTENT {
            return Err(SessionTmpError::RootNotManaged(self.state_root.clone()));
        }
        let record_path = self.state_root.join(STATE_ROOT_RECORD);
        let record = identity::read_root_record(&record_path)?;
        let expected_namespace = identity::canonicalize_for_identity(&self.payload_namespace)?;
        let namespace_is_same = identity::canonicalize_for_identity(&record.payload_namespace)
            .is_ok_and(|namespace| namespace == expected_namespace);
        if record.schema_version != 1
            || record.root_id != self.root_id
            || record.canonical_payload_root != self.canonical_payload_root
            || !namespace_is_same
        {
            return Err(SessionTmpError::RootNotManaged(self.state_root.clone()));
        }
        Ok(())
    }

    pub(super) fn ensure_state_layout(&self) -> Result<(), SessionTmpError> {
        self.ensure_identity()?;
        let sessions_dir = self.state_sessions_dir();
        let locks_dir = self.state_locks_dir();
        storage::ensure_directory_not_symlink(&sessions_dir)?;
        storage::ensure_directory_not_symlink(&locks_dir)?;
        fs::create_dir_all(&sessions_dir)?;
        fs::create_dir_all(&locks_dir)?;
        storage::set_private_directory(&sessions_dir)?;
        storage::set_private_directory(&locks_dir)?;
        Ok(())
    }
}
