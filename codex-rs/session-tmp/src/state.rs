//! Durable control state for session temporary storage.
//!
//! Payload directories are user-disposable.  This module keeps ownership,
//! metadata, leases, and coordination files in a per-payload-root state tree
//! under the Codex home so deleting a payload root cannot erase liveness data.

use super::storage;
use super::EntryMetadata;
use super::SessionTmpError;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::path::Component;
use uuid::Uuid;

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
        Self::open_with_options(default_root, payload_root, false, true)
    }

    /// Enrolls a markerless nonempty default payload root after a separately
    /// validated legacy root has supplied the exact managed session records.
    /// The caller must keep all unknown payloads outside the state registry;
    /// this option is never exposed through model-facing APIs or custom roots.
    pub(super) fn open_for_validated_migration(
        default_root: &Path,
        payload_root: &Path,
    ) -> Result<Self, SessionTmpError> {
        Self::open_with_options(default_root, payload_root, true, true)
    }

    /// Opens a legacy source for migration without copying its control files
    /// until the caller holds the source migration lock.
    pub(super) fn open_for_migration(
        default_root: &Path,
        payload_root: &Path,
    ) -> Result<Self, SessionTmpError> {
        Self::open_with_options(default_root, payload_root, false, false)
    }

    fn open_with_options(
        default_root: &Path,
        payload_root: &Path,
        allow_nonempty_without_legacy_marker: bool,
        import_legacy: bool,
    ) -> Result<Self, SessionTmpError> {
        if !default_root.is_absolute() {
            return Err(SessionTmpError::RootNotAbsolute(default_root.to_path_buf()));
        }
        if !payload_root.is_absolute() {
            return Err(SessionTmpError::RootNotAbsolute(payload_root.to_path_buf()));
        }
        ensure_existing_ancestors_for_runtime(default_root)?;
        ensure_existing_ancestors_for_runtime(payload_root)?;
        storage::ensure_directory_not_symlink(default_root)?;
        fs::create_dir_all(default_root)?;
        storage::ensure_directory_not_symlink(default_root)?;

        let canonical_payload_root = canonicalize_for_identity(payload_root)?;
        let state_base = default_root.join(STATE_DIR).join(STATE_SESSION_TMP_DIR);
        ensure_existing_ancestors_for_runtime(&state_base)?;
        let canonical_state_base = canonicalize_for_identity(&state_base)?;
        if paths_overlap(&canonical_payload_root, &canonical_state_base) {
            return Err(SessionTmpError::UnsafeManagedPath(state_base));
        }
        storage::ensure_directory_not_symlink(&state_base)?;
        fs::create_dir_all(&state_base)?;
        storage::ensure_directory_not_symlink(&state_base)?;
        storage::set_private_directory(&state_base)?;

        let root_id = root_id(&canonical_payload_root);
        let state_root = state_base.join(&root_id);
        let legacy_managed = inspect_payload_root(payload_root)?;
        let existing_state = inspect_state_root(&state_root, &root_id, &canonical_payload_root)?;
        let payload_namespace = match existing_state {
            Some(namespace) => {
                // A valid external identity enrolls the payload path even when its
                // disposable contents, including the old marker, have vanished.
                ensure_payload_root(payload_root)?;
                ensure_payload_namespace(&namespace, &canonical_payload_root)?;
                namespace
            }
            None => {
                if !legacy_managed
                    && payload_is_nonempty(payload_root)?
                    && !allow_nonempty_without_legacy_marker
                {
                    return Err(SessionTmpError::RootNotManaged(payload_root.to_path_buf()));
                }
                initialize_state_root(
                    &state_root,
                    &root_id,
                    payload_root,
                    &canonical_payload_root,
                    allow_nonempty_without_legacy_marker && !legacy_managed,
                )?;
                ensure_payload_root(payload_root)?;
                let namespace = if allow_nonempty_without_legacy_marker && !legacy_managed {
                    payload_root.join(V2_PAYLOAD_NAMESPACE)
                } else {
                    payload_root.to_path_buf()
                };
                ensure_payload_namespace(&namespace, &canonical_payload_root)?;
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

    /// Returns whether the legacy payload marker still establishes a
    /// compatibility window for old binaries. A marker can be removed after
    /// all imported sessions are inactive; once absent, new writers must keep
    /// every control update in external state.
    pub(super) fn legacy_transition_active(&self) -> Result<bool, SessionTmpError> {
        let marker = self.payload_root.join(LEGACY_MARKER);
        match fs::symlink_metadata(&marker) {
            Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
                Err(SessionTmpError::UnsafeManagedPath(marker))
            }
            Ok(_) => Ok(fs::read_to_string(marker)? == LEGACY_MARKER_CONTENT),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
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
        let record = read_root_record(&record_path)?;
        let expected_namespace = canonicalize_for_identity(&self.payload_namespace)?;
        let namespace_is_same = canonicalize_for_identity(&record.payload_namespace)
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

    pub(super) fn import_legacy_control(&self) -> Result<(), SessionTmpError> {
        let legacy_sessions = self.payload_root.join("sessions");
        storage::ensure_directory_not_symlink(&legacy_sessions)?;
        if !legacy_sessions.is_dir() {
            return Ok(());
        }
        for item in fs::read_dir(&legacy_sessions)? {
            let legacy_session_dir = item?.path();
            let Some(session_id) = legacy_session_dir.file_name().and_then(|name| name.to_str())
            else {
                continue;
            };
            if storage::validate_component(session_id).is_err()
                || !is_real_directory(&legacy_session_dir)
            {
                continue;
            }
            let _legacy_lock = match storage::try_lock_legacy_session(self.payload_root(), session_id)? {
                storage::LegacyLock::Held(lock) => Some(lock),
                storage::LegacyLock::Absent => None,
                storage::LegacyLock::Unavailable => continue,
            };
            let legacy_record_path = legacy_session_dir.join("session.json");
            let Ok(record) = storage::read_session_record(&legacy_record_path) else {
                continue;
            };
            if record.schema_version != 1 || record.session_id != session_id {
                continue;
            }
            let state_session_dir = self.state_session_dir(session_id);
            storage::ensure_directory_not_symlink(&state_session_dir)?;
            fs::create_dir_all(&state_session_dir)?;
            storage::set_private_directory(&state_session_dir)?;
            let state_record_path = state_session_dir.join("session.json");
            match fs::symlink_metadata(&state_record_path) {
                Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
                    return Err(SessionTmpError::UnsafeManagedPath(state_record_path));
                }
                Ok(_) => {
                    let existing = storage::read_session_record(&state_record_path)?;
                    if record.updated_at > existing.updated_at {
                        storage::write_json_atomically(&state_record_path, &record)?;
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    storage::write_json_atomically(&state_record_path, &record)?;
                }
                Err(error) => return Err(error.into()),
            }
            import_metadata(
                &legacy_session_dir.join("metadata"),
                &state_session_dir.join("metadata"),
                session_id,
            )?;
            import_leases(
                &legacy_session_dir.join("leases"),
                &state_session_dir.join("leases"),
                session_id,
            )?;
        }
        Ok(())
    }

    /// Retires the old payload marker once every recognizable legacy session
    /// is inactive.  Unknown payloads and active old-version sessions keep the
    /// marker as a compatibility boundary; no arbitrary directory is adopted
    /// or removed by this transition.
    pub(super) fn retire_legacy_marker_if_inactive(&self) -> Result<(), SessionTmpError> {
        if !self.legacy_managed {
            return Ok(());
        }
        let sessions_dir = self.payload_root.join("sessions");
        match fs::symlink_metadata(&sessions_dir) {
            Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
                return Err(SessionTmpError::UnsafeManagedPath(sessions_dir));
            }
            Ok(metadata) if !metadata.file_type().is_dir() => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return self.remove_legacy_marker();
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        storage::ensure_directory_not_symlink(&sessions_dir)?;
        storage::ensure_directory_not_symlink(&sessions_dir.join(".locks"))?;
        let mut legacy_locks = Vec::new();
        let mut legacy_session_ids = Vec::new();
        for item in fs::read_dir(&sessions_dir)? {
            let session_dir = item?.path();
            let Some(session_id) = session_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if session_id == ".locks" {
                continue;
            }
            if storage::validate_component(session_id).is_err()
                || !is_real_directory(&session_dir)
            {
                // Leave the marker in place when ownership of a payload
                // subtree cannot be proven from a legacy session record.
                return Ok(());
            }
            let Ok(record) = storage::read_session_record(&session_dir.join("session.json")) else {
                return Ok(());
            };
            if record.schema_version != 1 || record.session_id != session_id {
                return Ok(());
            }
            match storage::try_lock_legacy_session(self.payload_root(), session_id)? {
                storage::LegacyLock::Held(lock) => legacy_locks.push(lock),
                storage::LegacyLock::Absent => {}
                storage::LegacyLock::Unavailable => return Ok(()),
            }
            if storage::has_fresh_lease(
                &session_dir.join(storage::LEASES_DIR),
                storage::LEASE_STALE_AFTER,
            )? || storage::has_fresh_lease(
                &self
                    .state_session_dir(session_id)
                    .join(storage::LEASES_DIR),
                storage::LEASE_STALE_AFTER,
            )? {
                return Ok(());
            }
            legacy_session_ids.push(session_id.to_string());
        }
        // The external copies are already durable at this point. Remove only
        // validated legacy control records while their legacy locks remain
        // held; payload agents and unknown files stay in place. If a control
        // file is malformed it is intentionally left for later inspection.
        for session_id in &legacy_session_ids {
            retire_legacy_session_controls(&self.legacy_session_dir(session_id), session_id)?;
        }
        self.remove_legacy_marker()?;
        drop(legacy_locks);
        remove_legacy_lock_files(&sessions_dir, &legacy_session_ids)
    }

    fn remove_legacy_marker(&self) -> Result<(), SessionTmpError> {
        let marker = self.payload_root.join(LEGACY_MARKER);
        if fs::symlink_metadata(&marker)
            .map(|metadata| storage::file_type_is_link(metadata.file_type()))
            .unwrap_or(false)
        {
            return Err(SessionTmpError::UnsafeManagedPath(marker));
        }
        if marker.exists() {
            fs::remove_file(marker)?;
        }
        Ok(())
    }
}

fn retire_legacy_session_controls(
    session_dir: &Path,
    session_id: &str,
) -> Result<(), SessionTmpError> {
    let record_path = session_dir.join("session.json");
    if let Ok(record) = storage::read_session_record(&record_path)
        && record.schema_version == 1
        && record.session_id == session_id
    {
        storage::remove_path(&record_path)?;
    }

    let metadata_dir = session_dir.join("metadata");
    if is_real_directory(&metadata_dir) {
        for item in fs::read_dir(&metadata_dir)? {
            let path = item?.path();
            let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            let Ok(metadata) = storage::read_metadata(&path) else {
                continue;
            };
            if metadata.session_id == session_id
                && metadata.id == stem
                && storage::validate_component(&metadata.id).is_ok()
                && storage::validate_component(&metadata.thread_id).is_ok()
                && valid_metadata_path(&metadata)
            {
                storage::remove_path(&path)?;
            }
        }
        if fs::read_dir(&metadata_dir)?.next().transpose()?.is_none() {
            fs::remove_dir(&metadata_dir)?;
        }
    }

    let leases_dir = session_dir.join(storage::LEASES_DIR);
    if is_real_directory(&leases_dir) {
        for item in fs::read_dir(&leases_dir)? {
            let path = item?.path();
            let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            let Ok(record) = storage::read_lease_record(&path) else {
                continue;
            };
            if record.schema_version == 1
                && record.session_id == session_id
                && record.thread_id == stem
                && storage::validate_component(stem).is_ok()
            {
                storage::remove_path(&path)?;
            }
        }
        if fs::read_dir(&leases_dir)?.next().transpose()?.is_none() {
            fs::remove_dir(&leases_dir)?;
        }
    }

    if fs::read_dir(session_dir)?.next().transpose()?.is_none() {
        fs::remove_dir(session_dir)?;
    }
    Ok(())
}

fn remove_legacy_lock_files(
    sessions_dir: &Path,
    session_ids: &[String],
) -> Result<(), SessionTmpError> {
    let locks_dir = sessions_dir.join(".locks");
    if !is_real_directory(&locks_dir) {
        return Ok(());
    }
    for session_id in session_ids {
        let path = locks_dir.join(format!("{session_id}.lock"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
                return Err(SessionTmpError::UnsafeManagedPath(path));
            }
            Ok(_) => match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if fs::read_dir(locks_dir)?.next().transpose()?.is_none() {
        fs::remove_dir(locks_dir)?;
    }
    Ok(())
}

pub(super) fn inspect_payload_root(root: &Path) -> Result<bool, SessionTmpError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            return Err(SessionTmpError::UnsafeManagedPath(root.to_path_buf()));
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(SessionTmpError::UnsafeManagedPath(root.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let marker = root.join(LEGACY_MARKER);
    if fs::symlink_metadata(&marker)
        .map(|metadata| storage::file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(marker));
    }
    if marker.exists() {
        let content = fs::read_to_string(&marker)?;
        if content != LEGACY_MARKER_CONTENT {
            return Err(SessionTmpError::RootNotManaged(root.to_path_buf()));
        }
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn payload_is_nonempty(root: &Path) -> Result<bool, SessionTmpError> {
    if !root.exists() {
        return Ok(false);
    }
    Ok(fs::read_dir(root)?.next().transpose()?.is_some())
}

pub(super) fn ensure_payload_root(root: &Path) -> Result<(), SessionTmpError> {
    storage::ensure_directory_not_symlink(root)?;
    fs::create_dir_all(root)?;
    storage::ensure_directory_not_symlink(root)?;
    storage::set_private_directory(root)?;
    Ok(())
}

fn ensure_payload_namespace(
    namespace: &Path,
    canonical_payload_root: &Path,
) -> Result<(), SessionTmpError> {
    let canonical_namespace = canonicalize_for_identity(namespace)?;
    if !canonical_namespace.starts_with(canonical_payload_root) {
        return Err(SessionTmpError::UnsafeManagedPath(namespace.to_path_buf()));
    }
    storage::ensure_directory_not_symlink(namespace)?;
    fs::create_dir_all(namespace)?;
    storage::ensure_directory_not_symlink(namespace)?;
    storage::set_private_directory(namespace)?;
    let sessions = namespace.join("sessions");
    storage::ensure_directory_not_symlink(&sessions)?;
    fs::create_dir_all(&sessions)?;
    storage::set_private_directory(&sessions)?;
    Ok(())
}

fn inspect_state_root(
    state_root: &Path,
    root_id: &str,
    canonical_payload_root: &Path,
) -> Result<Option<PathBuf>, SessionTmpError> {
    match fs::symlink_metadata(state_root) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            return Err(SessionTmpError::UnsafeManagedPath(state_root.to_path_buf()));
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(SessionTmpError::UnsafeManagedPath(state_root.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let marker = state_root.join(STATE_MARKER);
    if fs::symlink_metadata(&marker)
        .map(|metadata| storage::file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(marker));
    }
    if fs::read_to_string(&marker).ok().as_deref() != Some(STATE_MARKER_CONTENT) {
        return Err(SessionTmpError::RootNotManaged(state_root.to_path_buf()));
    }
    let record = read_root_record(&state_root.join(STATE_ROOT_RECORD))?;
    let namespace_is_safe = record.payload_namespace.is_absolute()
        && canonicalize_for_identity(&record.payload_namespace)
            .is_ok_and(|namespace| namespace.starts_with(canonical_payload_root));
    if record.schema_version != 1
        || record.root_id != root_id
        || record.canonical_payload_root != canonical_payload_root
        || !namespace_is_safe
    {
        return Err(SessionTmpError::RootNotManaged(state_root.to_path_buf()));
    }
    Ok(Some(record.payload_namespace))
}

fn initialize_state_root(
    state_root: &Path,
    root_id: &str,
    payload_root: &Path,
    canonical_payload_root: &Path,
    use_v2_namespace: bool,
) -> Result<(), SessionTmpError> {
    fs::create_dir_all(state_root)?;
    storage::ensure_directory_not_symlink(state_root)?;
    storage::set_private_directory(state_root)?;
    let marker = state_root.join(STATE_MARKER);
    match OpenOptions::new().write(true).create_new(true).open(&marker) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(STATE_MARKER_CONTENT.as_bytes())?;
            file.sync_all()?;
            storage::set_private_file(&marker)?;
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if fs::read_to_string(&marker)? != STATE_MARKER_CONTENT {
                return Err(SessionTmpError::RootNotManaged(state_root.to_path_buf()));
            }
        }
        Err(error) => return Err(error.into()),
    }
    let record = RootRecord {
        schema_version: 1,
        root_id: root_id.to_string(),
        payload_root: payload_root.to_path_buf(),
        canonical_payload_root: canonical_payload_root.to_path_buf(),
        payload_namespace: if use_v2_namespace {
            payload_root.join(V2_PAYLOAD_NAMESPACE)
        } else {
            payload_root.to_path_buf()
        },
    };
    let record_path = state_root.join(STATE_ROOT_RECORD);
    if record_path.exists() {
        let existing = read_root_record(&record_path)?;
        if existing.root_id != record.root_id
            || existing.canonical_payload_root != record.canonical_payload_root
            || existing.payload_namespace != record.payload_namespace
        {
            return Err(SessionTmpError::RootNotManaged(state_root.to_path_buf()));
        }
    } else {
        storage::write_json_atomically(&record_path, &record)?;
    }
    Ok(())
}

fn read_root_record(path: &Path) -> Result<RootRecord, SessionTmpError> {
    if fs::symlink_metadata(path)
        .map(|metadata| storage::file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
    {
        return Err(SessionTmpError::UnsafeManagedPath(path.to_path_buf()));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(|source| SessionTmpError::InvalidMetadata {
        path: path.to_path_buf(),
        source,
    })
}

fn import_metadata(
    legacy_dir: &Path,
    state_dir: &Path,
    session_id: &str,
) -> Result<(), SessionTmpError> {
    storage::ensure_directory_not_symlink(legacy_dir)?;
    if !legacy_dir.is_dir() {
        return Ok(());
    }
    storage::ensure_directory_not_symlink(state_dir)?;
    fs::create_dir_all(state_dir)?;
    storage::set_private_directory(state_dir)?;
    for item in fs::read_dir(legacy_dir)? {
        let legacy_path = item?.path();
        if legacy_path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(metadata) = storage::read_metadata(&legacy_path) else {
            continue;
        };
        if metadata.session_id != session_id
            || !valid_metadata_path(&metadata)
        {
            continue;
        }
        if storage::validate_component(&metadata.id).is_err()
            || storage::validate_component(&metadata.thread_id).is_err()
        {
            continue;
        }
        let state_path = state_dir.join(format!("{}.json", metadata.id));
        match fs::symlink_metadata(&state_path) {
            Ok(file_type) if storage::file_type_is_link(file_type.file_type()) => {
                return Err(SessionTmpError::UnsafeManagedPath(state_path));
            }
            Ok(_) => {
                let existing = storage::read_metadata(&state_path)?;
                if existing != metadata {
                    let mut migrated = metadata;
                    migrated.id = Uuid::new_v4().simple().to_string();
                    storage::write_json_atomically(
                        &state_dir.join(format!("{}.json", migrated.id)),
                        &migrated,
                    )?;
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                storage::write_json_atomically(&state_path, &metadata)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn valid_metadata_path(metadata: &EntryMetadata) -> bool {
    let mut components = metadata.path.components();
    components.next().is_some_and(|component| component.as_os_str() == "agents")
        && components
            .next()
            .is_some_and(|component| component.as_os_str() == metadata.thread_id.as_str())
        && components
            .next()
            .is_some_and(|component| matches!(component, Component::Normal(_)))
        && components.all(|component| matches!(component, Component::Normal(_)))
}

fn import_leases(
    legacy_dir: &Path,
    state_dir: &Path,
    session_id: &str,
) -> Result<(), SessionTmpError> {
    storage::ensure_directory_not_symlink(legacy_dir)?;
    if !legacy_dir.is_dir() {
        return Ok(());
    }
    storage::ensure_directory_not_symlink(state_dir)?;
    fs::create_dir_all(state_dir)?;
    storage::set_private_directory(state_dir)?;
    for item in fs::read_dir(legacy_dir)? {
        let legacy_path = item?.path();
        if legacy_path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(record) = storage::read_lease_record(&legacy_path) else {
            continue;
        };
        if record.schema_version != 1
            || record.session_id != session_id
            || storage::validate_component(&record.thread_id).is_err()
        {
            continue;
        }
        let Some(name) = legacy_path.file_name() else {
            continue;
        };
        let state_path = state_dir.join(name);
        match fs::symlink_metadata(&state_path) {
            Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
                return Err(SessionTmpError::UnsafeManagedPath(state_path));
            }
            Ok(_) => {
                let existing = storage::read_lease_record(&state_path)?;
                if record.updated_at > existing.updated_at {
                    storage::write_json_atomically(&state_path, &record)?;
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                storage::write_json_atomically(&state_path, &record)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir() && !storage::file_type_is_link(metadata.file_type()))
        .unwrap_or(false)
}

pub(super) fn canonicalize_for_identity(path: &Path) -> Result<PathBuf, SessionTmpError> {
    if path.exists() {
        return Ok(fs::canonicalize(path)?);
    }
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        let Some(name) = cursor.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        cursor = cursor.parent().ok_or_else(|| {
            SessionTmpError::RootNotAbsolute(path.to_path_buf())
        })?;
    }
    let mut canonical = fs::canonicalize(cursor)?;
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

pub(super) fn root_id(canonical_payload_root: &Path) -> String {
    // FNV-1a is tiny, deterministic across processes/platforms, and keeps
    // the state directory name to one validated component without adding a
    // dependency solely for hashing a path identity.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in canonical_payload_root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub(super) fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Returns whether an external identity already enrolls `payload_root`.
/// Unlike [`ControlState::open`], this probe never creates directories or
/// recreates a deleted payload root, so it is safe for migration discovery.
pub(super) fn external_identity_present(
    default_root: &Path,
    payload_root: &Path,
) -> Result<bool, SessionTmpError> {
    if !default_root.is_absolute() || !payload_root.is_absolute() {
        return Ok(false);
    }
    let canonical_payload_root = canonicalize_for_identity(payload_root)?;
    let state_base = default_root.join(STATE_DIR).join(STATE_SESSION_TMP_DIR);
    let state_root = state_base.join(root_id(&canonical_payload_root));
    match fs::symlink_metadata(&state_root) {
        Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
            return Err(SessionTmpError::UnsafeManagedPath(state_root));
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(SessionTmpError::UnsafeManagedPath(state_root));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    Ok(inspect_state_root(&state_root, &root_id(&canonical_payload_root), &canonical_payload_root)
        .is_ok_and(|namespace| namespace.is_some()))
}

pub(super) fn ensure_existing_ancestors_for_runtime(
    path: &Path,
) -> Result<(), SessionTmpError> {
    let mut cursor = Some(path);
    while let Some(candidate) = cursor {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if storage::file_type_is_link(metadata.file_type()) => {
                return Err(SessionTmpError::UnsafeManagedPath(candidate.to_path_buf()));
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(SessionTmpError::UnsafeManagedPath(candidate.to_path_buf()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        cursor = candidate.parent();
    }
    Ok(())
}
