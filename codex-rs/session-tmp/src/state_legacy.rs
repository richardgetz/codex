//! Legacy marker import and transition helpers.

use super::ControlState;
use super::LEGACY_MARKER;
use super::LEGACY_MARKER_CONTENT;
use super::SessionTmpError;
use super::identity::is_real_directory;
use super::storage;
use crate::EntryMetadata;
use std::fs;
use std::io::ErrorKind;
use std::path::Component;
use std::path::Path;

impl ControlState {
    pub(crate) fn legacy_transition_active(&self) -> Result<bool, SessionTmpError> {
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

    pub(crate) fn import_legacy_control(&self) -> Result<(), SessionTmpError> {
        let legacy_sessions = self.payload_root.join("sessions");
        storage::ensure_directory_not_symlink(&legacy_sessions)?;
        if !legacy_sessions.is_dir() {
            return Ok(());
        }
        for item in fs::read_dir(&legacy_sessions)? {
            let legacy_session_dir = item?.path();
            let Some(session_id) = legacy_session_dir
                .file_name()
                .and_then(|name| name.to_str())
            else {
                continue;
            };
            if storage::validate_component(session_id).is_err()
                || !is_real_directory(&legacy_session_dir)
            {
                continue;
            }
            // New managers acquire external state before probing the legacy
            // lock domain. Keep import in the same order so an old marker
            // cannot deadlock an opening manager while its records are copied.
            let state_session_dir = self.state_session_dir(session_id);
            let Some(_state_lock) = storage::try_lock_session(&state_session_dir)? else {
                continue;
            };
            let _legacy_lock =
                match storage::try_lock_legacy_session(self.payload_root(), session_id)? {
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

    pub(crate) fn retire_legacy_marker_if_inactive(&self) -> Result<(), SessionTmpError> {
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
            if storage::validate_component(session_id).is_err() || !is_real_directory(&session_dir)
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
                &self.state_session_dir(session_id).join(storage::LEASES_DIR),
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
        // Keep every legacy lock pathname as a durable compatibility residue.
        // Removing a name while an older process may still hold or await its
        // descriptor would split the old lock domain. The marker itself can be
        // retired now that all validated session controls are durable; future
        // managers use external state and leave these tiny lock files alone.
        self.remove_legacy_marker()?;
        drop(legacy_locks);
        Ok(())
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
            if fs::read_to_string(&marker)? != LEGACY_MARKER_CONTENT {
                return Ok(());
            }
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
        if legacy_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("json")
        {
            continue;
        }
        let Ok(metadata) = storage::read_metadata(&legacy_path) else {
            continue;
        };
        if metadata.session_id != session_id || !valid_metadata_path(&metadata) {
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
                if existing != metadata && !metadata_matches_without_id(&existing, &metadata) {
                    // Keep a stable source-derived ID for a collision copy.
                    // A random ID here would append another record on every
                    // manager open because the legacy source remains intact.
                    let mut migrated_or_present = false;
                    for suffix in 0..32 {
                        let migrated_id = if suffix == 0 {
                            format!("{}-migrated", metadata.id)
                        } else {
                            format!("{}-migrated-{suffix}", metadata.id)
                        };
                        let migrated_path = state_dir.join(format!("{migrated_id}.json"));
                        match fs::symlink_metadata(&migrated_path) {
                            Ok(file_type) if storage::file_type_is_link(file_type.file_type()) => {
                                return Err(SessionTmpError::UnsafeManagedPath(migrated_path));
                            }
                            Ok(_) => {
                                let existing = storage::read_metadata(&migrated_path)?;
                                if metadata_matches_without_id(&existing, &metadata) {
                                    migrated_or_present = true;
                                    break;
                                }
                            }
                            Err(error) if error.kind() == ErrorKind::NotFound => {
                                let mut migrated = metadata.clone();
                                migrated.id = migrated_id;
                                storage::write_json_atomically(&migrated_path, &migrated)?;
                                migrated_or_present = true;
                                break;
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    if !migrated_or_present {
                        return Err(std::io::Error::new(
                            ErrorKind::AlreadyExists,
                            "unable to allocate a stable legacy metadata collision path",
                        )
                        .into());
                    }
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

fn metadata_matches_without_id(left: &EntryMetadata, right: &EntryMetadata) -> bool {
    left.session_id == right.session_id
        && left.thread_id == right.thread_id
        && left.path == right.path
        && left.purpose == right.purpose
        && left.retention == right.retention
        && left.created_at == right.created_at
        && left.expires_at == right.expires_at
}

fn valid_metadata_path(metadata: &EntryMetadata) -> bool {
    let mut components = metadata.path.components();
    components
        .next()
        .is_some_and(|component| component.as_os_str() == "agents")
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
        if legacy_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("json")
        {
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
