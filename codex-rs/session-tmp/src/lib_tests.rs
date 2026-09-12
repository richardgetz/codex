use super::*;
use crate::storage::SessionRecord;
use pretty_assertions::assert_eq;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

fn config(root: &TempDir) -> SessionTmpConfig {
    SessionTmpConfig {
        enabled: true,
        root: Some(root.path().join("managed")),
        stale_after: Duration::from_secs(60),
    }
}

fn default_config() -> SessionTmpConfig {
    SessionTmpConfig {
        enabled: true,
        root: None,
        stale_after: Duration::from_secs(60),
    }
}

#[test]
fn disabled_storage_is_inert() {
    let root = tempfile::tempdir().unwrap();
    let result = SessionTmpManager::open(
        &SessionTmpConfig::default(),
        root.path(),
        "session",
        "thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap();
    assert!(result.is_none());
    assert!(!root.path().join("session-tmp").exists());
}

#[test]
fn fresh_roots_keep_control_state_outside_disposable_payloads() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert!(!manager.root().join(crate::state::LEGACY_MARKER).exists());
    assert!(
        manager
            .state
            .state_root()
            .join(crate::state::STATE_MARKER)
            .exists()
    );
    assert!(manager.state_session_dir.join(LEASES_DIR).is_dir());
}

#[test]
fn deleting_payload_recreates_the_same_namespace_from_external_state() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let payload_root = manager.root().to_path_buf();
    let state_session = manager.state_session_dir.clone();
    fs::remove_dir_all(&payload_root).unwrap();

    let entry = manager
        .create(
            None,
            "recreate after manual deletion",
            Retention::Manual,
            TempKind::File,
        )
        .unwrap();

    assert_eq!(manager.root(), payload_root.as_path());
    assert!(entry.absolute_path.exists());
    assert!(state_session.join(LEASES_DIR).is_dir());
}

#[test]
fn external_heartbeat_survives_payload_deletion() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let payload_root = manager.root().to_path_buf();
    let state_session = manager.state_session_dir.clone();
    let lease_path = state_session.join(LEASES_DIR).join("thread-1.json");
    fs::write(
        &lease_path,
        serde_json::to_vec(&crate::storage::LeaseRecord {
            schema_version: 1,
            session_id: "session-1".to_string(),
            thread_id: "thread-1".to_string(),
            process_id: std::process::id(),
            owner_token: crate::storage::read_lease_record(&lease_path)
                .unwrap()
                .owner_token,
            updated_at: 0,
        })
        .unwrap(),
    )
    .unwrap();
    fs::remove_dir_all(payload_root).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    assert!(
        crate::storage::read_lease_record(&lease_path)
            .unwrap()
            .updated_at
            > 0
    );
    manager
        .create(
            None,
            "resume after heartbeat",
            Retention::Manual,
            TempKind::File,
        )
        .unwrap();
}

#[test]
fn deleting_external_session_state_does_not_resurrect_heartbeat_files() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let state_session = manager.state_session_dir.clone();
    fs::remove_dir_all(&state_session).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    assert!(!state_session.exists());
    drop(manager);
}

#[test]
fn missing_payload_keeps_other_live_agent_metadata_reclaimable() {
    let root = tempfile::tempdir().unwrap();
    let config = config(&root);
    let root_manager = SessionTmpManager::open(
        &config,
        root.path(),
        "session-1",
        "root-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let agent_manager = SessionTmpManager::open(
        &config,
        root.path(),
        "session-1",
        "agent-thread",
        SessionTmpOwner::Agent,
    )
    .unwrap()
    .unwrap();
    let entry = agent_manager
        .create(
            None,
            "live agent output",
            Retention::Session,
            TempKind::File,
        )
        .unwrap();
    fs::remove_dir_all(root_manager.root()).unwrap();

    let listing = root_manager.list().unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert!(!listing.entries[0].exists);
    root_manager.clean().unwrap();
    assert!(
        root_manager
            .state_session_dir
            .join(ENTRY_METADATA_DIR)
            .join(format!("{}.json", entry.metadata.id))
            .exists()
    );
}

#[test]
fn deleting_legacy_payload_does_not_resurrect_heartbeat_control_files() {
    let root = tempfile::tempdir().unwrap();
    let configured_root = root.path().join("managed");
    let legacy_session = configured_root.join(SESSIONS_DIR).join("session-1");
    fs::create_dir_all(legacy_session.join(AGENTS_DIR).join("old-thread")).unwrap();
    fs::create_dir_all(legacy_session.join(LEASES_DIR)).unwrap();
    fs::create_dir_all(legacy_session.parent().unwrap().join(".locks")).unwrap();
    fs::write(
        legacy_session
            .parent()
            .unwrap()
            .join(".locks")
            .join("session-1.lock"),
        b"legacy lock",
    )
    .unwrap();
    fs::write(
        configured_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    let updated_at = crate::storage::now_seconds();
    fs::write(
        legacy_session.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "session-1".to_string(),
            created_at: updated_at,
            updated_at,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    fs::write(
        legacy_session.join(LEASES_DIR).join("old-thread.json"),
        serde_json::to_vec(&crate::storage::LeaseRecord {
            schema_version: 1,
            session_id: "session-1".to_string(),
            thread_id: "old-thread".to_string(),
            process_id: std::process::id(),
            owner_token: None,
            updated_at,
        })
        .unwrap(),
    )
    .unwrap();
    let mut config = config(&root);
    config.root = Some(configured_root.clone());
    let manager = SessionTmpManager::open(
        &config,
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let state_session = manager.state_session_dir.clone();

    fs::remove_dir_all(&configured_root).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    assert!(!configured_root.exists());
    assert!(
        state_session
            .join(LEASES_DIR)
            .join("thread-1.json")
            .exists()
    );
}

#[test]
fn markerless_nonempty_custom_roots_are_not_adopted() {
    let root = tempfile::tempdir().unwrap();
    let configured_root = root.path().join("managed");
    fs::create_dir_all(&configured_root).unwrap();
    fs::write(configured_root.join("preserve.txt"), b"keep").unwrap();

    let result = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    );

    assert!(matches!(
        result,
        Err(SessionTmpError::RootNotManaged(path)) if path == configured_root
    ));
    assert!(configured_root.join("preserve.txt").exists());
}

#[test]
fn recovery_root_is_merged_into_a_hidden_default_namespace() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    fs::create_dir_all(&normal_root).unwrap();
    fs::write(normal_root.join("preserved.txt"), b"keep").unwrap();
    let recovery_session = recovery_root.join(SESSIONS_DIR).join("legacy-session");
    let recovery_agent = recovery_session.join(AGENTS_DIR).join("legacy-thread");
    fs::create_dir_all(&recovery_agent).unwrap();
    fs::write(
        recovery_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    let updated_at = crate::storage::now_seconds();
    fs::write(
        recovery_session.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "legacy-session".to_string(),
            created_at: updated_at,
            updated_at,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    fs::write(recovery_agent.join("artifact.txt"), b"migrated").unwrap();

    let _manager = SessionTmpManager::open(
        &default_config(),
        home.path(),
        "new-session",
        "new-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert!(!recovery_root.exists());
    assert_eq!(
        fs::read(normal_root.join("preserved.txt")).unwrap(),
        b"keep"
    );
    assert_eq!(
        fs::read(
            normal_root
                .join(crate::state::V2_PAYLOAD_NAMESPACE)
                .join(SESSIONS_DIR)
                .join("legacy-session")
                .join(AGENTS_DIR)
                .join("legacy-thread")
                .join("artifact.txt")
        )
        .unwrap(),
        b"migrated"
    );
}

#[test]
fn recovery_merge_leaves_unknown_files_at_original_paths() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    let recovery_session = recovery_root.join(SESSIONS_DIR).join("known-session");
    let recovery_agent = recovery_session.join(AGENTS_DIR).join("known-thread");
    fs::create_dir_all(&recovery_agent).unwrap();
    fs::write(
        recovery_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    fs::write(recovery_root.join("operator-notes.txt"), b"leave here").unwrap();
    let updated_at = crate::storage::now_seconds();
    fs::write(
        recovery_session.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "known-session".to_string(),
            created_at: updated_at,
            updated_at,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    fs::write(recovery_agent.join("artifact.txt"), b"migrated").unwrap();

    let manager = SessionTmpManager::open(
        &default_config(),
        home.path(),
        "current-session",
        "current-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert!(recovery_root.exists());
    assert_eq!(
        fs::read(recovery_root.join("operator-notes.txt")).unwrap(),
        b"leave here"
    );
    assert!(!recovery_session.exists());
    assert_eq!(
        fs::read(
            normal_root
                .join(crate::state::V2_PAYLOAD_NAMESPACE)
                .join(SESSIONS_DIR)
                .join("known-session")
                .join(AGENTS_DIR)
                .join("known-thread")
                .join("artifact.txt")
        )
        .unwrap(),
        b"migrated"
    );
    drop(manager);
}

#[test]
fn malformed_default_marker_uses_hidden_namespace_and_ignores_foreign_controls() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    fs::create_dir_all(
        normal_root
            .join(SESSIONS_DIR)
            .join("foreign")
            .join(LEASES_DIR),
    )
    .unwrap();
    fs::write(
        normal_root.join(crate::state::LEGACY_MARKER),
        b"operator marker\n",
    )
    .unwrap();
    fs::write(
        normal_root
            .join(SESSIONS_DIR)
            .join("foreign")
            .join(LEASES_DIR)
            .join("foreign-thread.json"),
        serde_json::to_vec(&crate::storage::LeaseRecord {
            schema_version: 1,
            session_id: "foreign".to_string(),
            thread_id: "foreign-thread".to_string(),
            process_id: std::process::id(),
            owner_token: None,
            updated_at: crate::storage::now_seconds(),
        })
        .unwrap(),
    )
    .unwrap();
    let recovery_session = recovery_root.join(SESSIONS_DIR).join("recovery-session");
    let recovery_agent = recovery_session.join(AGENTS_DIR).join("recovery-thread");
    fs::create_dir_all(&recovery_agent).unwrap();
    fs::write(
        recovery_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    let updated_at = crate::storage::now_seconds();
    fs::write(
        recovery_session.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "recovery-session".to_string(),
            created_at: updated_at,
            updated_at,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    fs::write(recovery_agent.join("artifact.txt"), b"migrated").unwrap();

    let manager = SessionTmpManager::open(
        &default_config(),
        home.path(),
        "current-session",
        "current-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert_eq!(
        fs::read(
            normal_root
                .join(crate::state::V2_PAYLOAD_NAMESPACE)
                .join(SESSIONS_DIR)
                .join("recovery-session")
                .join(AGENTS_DIR)
                .join("recovery-thread")
                .join("artifact.txt")
        )
        .unwrap(),
        b"migrated"
    );
    assert_eq!(
        fs::read_to_string(normal_root.join(crate::state::LEGACY_MARKER)).unwrap(),
        "operator marker\n"
    );
    assert!(
        normal_root
            .join(SESSIONS_DIR)
            .join("foreign")
            .join(LEASES_DIR)
            .join("foreign-thread.json")
            .exists()
    );
    drop(manager);
}

#[test]
fn corrupt_recovery_manifest_is_preserved_for_retry() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    fs::create_dir_all(&recovery_root).unwrap();
    fs::write(
        recovery_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    let target = crate::state::ControlState::open(home.path(), &normal_root).unwrap();
    let source_id =
        crate::state::root_id(&crate::state::canonicalize_for_identity(&recovery_root).unwrap());
    let manifest_path = target
        .state_root()
        .join(format!(".migration-{source_id}.json"));
    fs::write(&manifest_path, b"{ not json").unwrap();

    crate::migration::consolidate_recovery(&target, home.path()).unwrap();

    assert_eq!(fs::read(&manifest_path).unwrap(), b"{ not json");
    assert!(recovery_root.exists());
}

#[test]
fn recovery_manifest_completes_after_both_source_roots_were_retired() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    let target = crate::state::ControlState::open(home.path(), &normal_root).unwrap();
    let source_root_id =
        crate::state::root_id(&crate::state::canonicalize_for_identity(&recovery_root).unwrap());
    let manifest_path = target
        .state_root()
        .join(format!(".migration-{source_root_id}.json"));
    fs::write(
        &manifest_path,
        serde_json::json!({
            "schema_version": 1,
            "source_root_id": source_root_id,
            "target_root_id": target.root_id(),
            "phase": "in_progress",
            "updated_at": crate::storage::now_seconds(),
            "source_payload_root": recovery_root,
            "moved_paths": [],
        })
        .to_string(),
    )
    .unwrap();

    crate::migration::consolidate_recovery(&target, home.path()).unwrap();

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["phase"], "complete");
}

#[test]
fn unsafe_recovery_manifest_is_not_completed_without_source_roots() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    let target = crate::state::ControlState::open(home.path(), &normal_root).unwrap();
    let source_root_id =
        crate::state::root_id(&crate::state::canonicalize_for_identity(&recovery_root).unwrap());
    let manifest_path = target
        .state_root()
        .join(format!(".migration-{source_root_id}.json"));
    fs::write(
        &manifest_path,
        serde_json::json!({
            "schema_version": 1,
            "source_root_id": source_root_id,
            "target_root_id": target.root_id(),
            "phase": "in_progress",
            "updated_at": crate::storage::now_seconds(),
            "source_payload_root": recovery_root,
            "moved_paths": [{"source": "../escape", "target": "agents/thread/file"}],
        })
        .to_string(),
    )
    .unwrap();

    crate::migration::consolidate_recovery(&target, home.path()).unwrap();

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["phase"], "in_progress");
}

#[test]
fn lease_drop_does_not_remove_a_replacement_external_lease() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let lease_path = manager
        .state_session_dir
        .join(LEASES_DIR)
        .join("thread-1.json");
    fs::write(
        &lease_path,
        serde_json::to_vec(&crate::storage::LeaseRecord {
            schema_version: 1,
            session_id: "session-1".to_string(),
            thread_id: "thread-1".to_string(),
            process_id: std::process::id().saturating_add(1),
            owner_token: None,
            updated_at: crate::storage::now_seconds(),
        })
        .unwrap(),
    )
    .unwrap();
    drop(manager);

    assert_eq!(
        crate::storage::read_lease_record(&lease_path)
            .unwrap()
            .process_id,
        std::process::id().saturating_add(1)
    );
}

#[test]
fn stale_heartbeat_does_not_overwrite_a_replacement_external_lease() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let lease_path = manager
        .state_session_dir
        .join(LEASES_DIR)
        .join("thread-1.json");
    fs::write(
        &lease_path,
        serde_json::to_vec(&crate::storage::LeaseRecord {
            schema_version: 1,
            session_id: "session-1".to_string(),
            thread_id: "thread-1".to_string(),
            process_id: std::process::id(),
            owner_token: Some("replacement-owner".to_string()),
            updated_at: crate::storage::now_seconds(),
        })
        .unwrap(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let record = crate::storage::read_lease_record(&lease_path).unwrap();
    assert_eq!(record.owner_token.as_deref(), Some("replacement-owner"));
    drop(manager);
    assert_eq!(
        crate::storage::read_lease_record(&lease_path)
            .unwrap()
            .owner_token
            .as_deref(),
        Some("replacement-owner")
    );
}

#[test]
fn recovery_merge_preserves_collisions_and_mixes_session_ids() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    let normal_session = normal_root
        .join(SESSIONS_DIR)
        .join("same-session")
        .join(AGENTS_DIR)
        .join("normal-thread");
    let recovery_session = recovery_root
        .join(SESSIONS_DIR)
        .join("same-session")
        .join(AGENTS_DIR)
        .join("normal-thread");
    let recovery_only = recovery_root
        .join(SESSIONS_DIR)
        .join("recovery-only")
        .join(AGENTS_DIR)
        .join("recovery-thread");
    fs::create_dir_all(&normal_session).unwrap();
    fs::create_dir_all(&recovery_session).unwrap();
    fs::create_dir_all(&recovery_only).unwrap();
    fs::write(
        normal_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    fs::write(
        recovery_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    let updated_at = crate::storage::now_seconds();
    for (root, session_id) in [
        (&normal_root, "same-session"),
        (&recovery_root, "same-session"),
        (&recovery_root, "recovery-only"),
    ] {
        let session_dir = root.join(SESSIONS_DIR).join(session_id);
        fs::write(
            session_dir.join(SESSION_METADATA_FILE),
            serde_json::to_vec(&SessionRecord {
                schema_version: 1,
                session_id: session_id.to_string(),
                created_at: updated_at,
                updated_at,
                status: "active".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
    }
    fs::write(normal_session.join("artifact.txt"), b"normal").unwrap();
    fs::write(recovery_session.join("artifact.txt"), b"recovery").unwrap();
    fs::write(normal_session.join("same.txt"), b"identical").unwrap();
    fs::write(recovery_session.join("same.txt"), b"identical").unwrap();
    fs::write(recovery_only.join("only.txt"), b"only").unwrap();

    let manager = SessionTmpManager::open(
        &default_config(),
        home.path(),
        "current-session",
        "current-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert!(!recovery_root.exists());
    assert_eq!(
        fs::read(normal_session.join("artifact.txt")).unwrap(),
        b"normal"
    );
    let migrated_collision = fs::read_dir(
        normal_root
            .join(SESSIONS_DIR)
            .join("same-session")
            .join(AGENTS_DIR)
            .join("normal-thread"),
    )
    .unwrap()
    .filter_map(Result::ok)
    .map(|entry| entry.path())
    .find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("artifact.txt-migrated-"))
    })
    .expect("recovery collision should receive a unique destination");
    assert_eq!(fs::read(migrated_collision).unwrap(), b"recovery");
    assert_eq!(
        fs::read(normal_session.join("same.txt")).unwrap(),
        b"identical"
    );
    assert!(
        !fs::read_dir(&normal_session)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("same.txt-migrated-"))
            })
    );
    assert_eq!(
        fs::read(
            normal_root
                .join(SESSIONS_DIR)
                .join("recovery-only")
                .join(AGENTS_DIR)
                .join("recovery-thread")
                .join("only.txt")
        )
        .unwrap(),
        b"only"
    );
    drop(manager);
}

#[test]
fn recovery_merge_scopes_mappings_to_each_session() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    let normal_conflict = normal_root
        .join(SESSIONS_DIR)
        .join("session-one")
        .join(AGENTS_DIR)
        .join("shared-thread");
    let recovery_one = recovery_root
        .join(SESSIONS_DIR)
        .join("session-one")
        .join(AGENTS_DIR)
        .join("shared-thread");
    let recovery_two = recovery_root
        .join(SESSIONS_DIR)
        .join("session-two")
        .join(AGENTS_DIR)
        .join("shared-thread");
    fs::create_dir_all(&normal_conflict).unwrap();
    fs::create_dir_all(&recovery_one).unwrap();
    fs::create_dir_all(&recovery_two).unwrap();
    for root in [&normal_root, &recovery_root] {
        fs::write(
            root.join(crate::state::LEGACY_MARKER),
            crate::state::LEGACY_MARKER_CONTENT,
        )
        .unwrap();
    }
    let updated_at = crate::storage::now_seconds();
    for (root, session_id) in [
        (&normal_root, "session-one"),
        (&recovery_root, "session-one"),
        (&recovery_root, "session-two"),
    ] {
        let session_dir = root.join(SESSIONS_DIR).join(session_id);
        fs::write(
            session_dir.join(SESSION_METADATA_FILE),
            serde_json::to_vec(&SessionRecord {
                schema_version: 1,
                session_id: session_id.to_string(),
                created_at: updated_at,
                updated_at,
                status: "active".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
    }
    fs::write(normal_conflict.join("artifact.txt"), b"target-one").unwrap();
    fs::write(recovery_one.join("artifact.txt"), b"source-one").unwrap();
    fs::write(recovery_two.join("artifact.txt"), b"source-two").unwrap();

    let manager = SessionTmpManager::open(
        &default_config(),
        home.path(),
        "current-session",
        "current-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert!(!recovery_root.exists());
    let session_two_agent = normal_root
        .join(SESSIONS_DIR)
        .join("session-two")
        .join(AGENTS_DIR)
        .join("shared-thread");
    assert_eq!(
        fs::read(session_two_agent.join("artifact.txt")).unwrap(),
        b"source-two"
    );
    assert!(
        !fs::read_dir(session_two_agent)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("artifact.txt-migrated-"))
            })
    );
    drop(manager);
}

#[test]
fn recovery_merge_preserves_foreign_migrated_prefix_directory() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    let normal_agent = normal_root
        .join(SESSIONS_DIR)
        .join("same-session")
        .join(AGENTS_DIR)
        .join("same-thread");
    let recovery_session = recovery_root.join(SESSIONS_DIR).join("same-session");
    let recovery_agent = recovery_root
        .join(SESSIONS_DIR)
        .join("same-session")
        .join(AGENTS_DIR)
        .join("same-thread");
    let foreign_destination = normal_agent.join("nested-migrated-foreign");
    let updated_at = crate::storage::now_seconds();
    fs::create_dir_all(&normal_agent).unwrap();
    fs::create_dir_all(foreign_destination.join("deep")).unwrap();
    fs::create_dir_all(recovery_agent.join("nested").join("deep")).unwrap();
    fs::write(normal_agent.join("nested"), b"normal file").unwrap();
    fs::write(
        foreign_destination.join("deep").join("artifact.txt"),
        b"foreign",
    )
    .unwrap();
    fs::write(
        recovery_agent
            .join("nested")
            .join("deep")
            .join("artifact.txt"),
        b"replayed",
    )
    .unwrap();
    fs::create_dir_all(recovery_session.join(ENTRY_METADATA_DIR)).unwrap();
    fs::write(
        recovery_session
            .join(ENTRY_METADATA_DIR)
            .join("nested-entry.json"),
        serde_json::to_vec(&EntryMetadata {
            id: "nested-entry".to_string(),
            session_id: "same-session".to_string(),
            thread_id: "same-thread".to_string(),
            path: PathBuf::from("agents/same-thread/nested/deep/artifact.txt"),
            purpose: "nested migration payload".to_string(),
            retention: Retention::Manual,
            created_at: updated_at,
            expires_at: None,
        })
        .unwrap(),
    )
    .unwrap();
    fs::write(
        normal_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    fs::write(
        recovery_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    for root in [&normal_root, &recovery_root] {
        let session_dir = root.join(SESSIONS_DIR).join("same-session");
        fs::write(
            session_dir.join(SESSION_METADATA_FILE),
            serde_json::to_vec(&SessionRecord {
                schema_version: 1,
                session_id: "same-session".to_string(),
                created_at: updated_at,
                updated_at,
                status: "active".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
    }

    let manager = SessionTmpManager::open(
        &default_config(),
        home.path(),
        "current-session",
        "current-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert!(!recovery_root.exists());
    assert!(normal_agent.join("nested").is_file());
    assert_eq!(
        fs::read(foreign_destination.join("deep").join("artifact.txt")).unwrap(),
        b"foreign"
    );
    assert_eq!(
        fs::read_dir(&normal_agent)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("nested-migrated-"))
            })
            .count(),
        2
    );
    let migrated_destination = fs::read_dir(&normal_agent)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("nested-migrated-") && path != &foreign_destination
                })
        })
        .expect("managed collision should have its own deterministic destination");
    assert_eq!(
        fs::read(migrated_destination.join("deep").join("artifact.txt")).unwrap(),
        b"replayed"
    );
    let metadata_dir = manager
        .state
        .state_session_dir("same-session")
        .join(ENTRY_METADATA_DIR);
    let migrated_metadata_path = fs::read_dir(&metadata_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("nested-entry"))
        })
        .expect("migrated metadata should remain discoverable");
    let migrated_metadata = crate::storage::read_metadata(&migrated_metadata_path).unwrap();
    assert_eq!(
        fs::read(
            manager
                .state
                .payload_session_dir("same-session")
                .join(migrated_metadata.path)
        )
        .unwrap(),
        b"replayed"
    );
    drop(manager);
}

#[test]
fn recovery_merge_defers_a_live_legacy_lease_and_retries_after_release() {
    let home = tempfile::tempdir().unwrap();
    let normal_root = home.path().join("session-tmp");
    let recovery_root = home.path().join("session-tmp-recovery");
    let recovery_session = recovery_root.join(SESSIONS_DIR).join("live-session");
    let recovery_agent = recovery_session.join(AGENTS_DIR).join("live-thread");
    let recovery_leases = recovery_session.join(LEASES_DIR);
    fs::create_dir_all(&recovery_agent).unwrap();
    fs::create_dir_all(&recovery_leases).unwrap();
    fs::write(
        recovery_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    let updated_at = crate::storage::now_seconds();
    fs::write(
        recovery_session.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "live-session".to_string(),
            created_at: updated_at,
            updated_at,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    fs::write(recovery_agent.join("artifact.txt"), b"live").unwrap();
    fs::write(
        recovery_leases.join("live-thread.json"),
        serde_json::to_vec(&crate::storage::LeaseRecord {
            schema_version: 1,
            session_id: "live-session".to_string(),
            thread_id: "live-thread".to_string(),
            process_id: std::process::id(),
            owner_token: None,
            updated_at,
        })
        .unwrap(),
    )
    .unwrap();

    let manager = SessionTmpManager::open(
        &default_config(),
        home.path(),
        "current-session",
        "current-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    assert!(recovery_root.exists());
    drop(manager);

    let source_state_root = home
        .path()
        .join(crate::state::STATE_DIR)
        .join(crate::state::STATE_SESSION_TMP_DIR)
        .join(crate::state::root_id(
            &crate::state::canonicalize_for_identity(&recovery_root).unwrap(),
        ));
    fs::remove_file(recovery_leases.join("live-thread.json")).unwrap();
    fs::remove_file(
        source_state_root
            .join(crate::state::STATE_SESSIONS_DIR)
            .join("live-session")
            .join(LEASES_DIR)
            .join("live-thread.json"),
    )
    .unwrap();

    let manager = SessionTmpManager::open(
        &default_config(),
        home.path(),
        "next-session",
        "next-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    assert!(!recovery_root.exists());
    assert!(
        normal_root
            .join(SESSIONS_DIR)
            .join("live-session")
            .join(AGENTS_DIR)
            .join("live-thread")
            .join("artifact.txt")
            .exists()
    );
    drop(manager);
}

#[test]
fn legacy_inline_control_files_are_imported_without_moving_payloads() {
    let root = tempfile::tempdir().unwrap();
    let configured_root = root.path().join("managed");
    let session_dir = configured_root.join(SESSIONS_DIR).join("legacy-session");
    fs::create_dir_all(session_dir.join(AGENTS_DIR).join("legacy-thread")).unwrap();
    fs::write(
        configured_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    fs::write(
        session_dir.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "legacy-session".to_string(),
            created_at: 1,
            updated_at: 1,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    fs::write(
        session_dir
            .join(AGENTS_DIR)
            .join("legacy-thread")
            .join("legacy.txt"),
        b"legacy payload",
    )
    .unwrap();
    fs::create_dir_all(session_dir.join(ENTRY_METADATA_DIR)).unwrap();
    fs::write(
        session_dir
            .join(ENTRY_METADATA_DIR)
            .join("legacy-entry.json"),
        serde_json::to_vec(&EntryMetadata {
            id: "legacy-entry".to_string(),
            session_id: "legacy-session".to_string(),
            thread_id: "legacy-thread".to_string(),
            path: PathBuf::from("agents/legacy-thread/legacy.txt"),
            purpose: "legacy payload".to_string(),
            retention: Retention::Manual,
            created_at: 1,
            expires_at: None,
        })
        .unwrap(),
    )
    .unwrap();
    fs::create_dir_all(session_dir.join(LEASES_DIR)).unwrap();
    fs::write(
        session_dir.join(LEASES_DIR).join("legacy-thread.json"),
        serde_json::to_vec(&crate::storage::LeaseRecord {
            schema_version: 1,
            session_id: "legacy-session".to_string(),
            thread_id: "legacy-thread".to_string(),
            process_id: std::process::id(),
            owner_token: None,
            updated_at: 1,
        })
        .unwrap(),
    )
    .unwrap();
    let legacy_lock_path = configured_root
        .join(SESSIONS_DIR)
        .join(".locks")
        .join("legacy-session.lock");
    fs::create_dir_all(legacy_lock_path.parent().unwrap()).unwrap();
    fs::write(&legacy_lock_path, b"").unwrap();
    let mut config = config(&root);
    config.root = Some(configured_root.clone());

    let manager =
        SessionTmpManager::open_for_user(&config, root.path(), "legacy-session", "legacy-thread")
            .unwrap()
            .unwrap();

    assert_eq!(manager.root(), configured_root.as_path());
    assert!(!configured_root.join(crate::state::LEGACY_MARKER).exists());
    assert!(!session_dir.join(SESSION_METADATA_FILE).exists());
    assert!(session_dir.join(AGENTS_DIR).join("legacy-thread").exists());
    assert!(!session_dir.join(ENTRY_METADATA_DIR).exists());
    assert!(!session_dir.join(LEASES_DIR).exists());
    assert!(legacy_lock_path.exists());
    assert!(
        manager
            .state_session_dir
            .join(SESSION_METADATA_FILE)
            .exists()
    );
}

#[test]
fn legacy_metadata_collision_reuses_a_stable_id_on_retry() {
    let root = tempfile::tempdir().unwrap();
    let configured_root = root.path().join("managed");
    let session_dir = configured_root.join(SESSIONS_DIR).join("legacy-session");
    let payload = session_dir
        .join(AGENTS_DIR)
        .join("legacy-thread")
        .join("legacy.txt");
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(&payload, b"legacy payload").unwrap();
    fs::write(
        configured_root.join(crate::state::LEGACY_MARKER),
        crate::state::LEGACY_MARKER_CONTENT,
    )
    .unwrap();
    let updated_at = crate::storage::now_seconds();
    fs::write(
        session_dir.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "legacy-session".to_string(),
            created_at: updated_at,
            updated_at,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    let metadata = EntryMetadata {
        id: "entry".to_string(),
        session_id: "legacy-session".to_string(),
        thread_id: "legacy-thread".to_string(),
        path: PathBuf::from("agents/legacy-thread/legacy.txt"),
        purpose: "legacy payload".to_string(),
        retention: Retention::Manual,
        created_at: updated_at,
        expires_at: None,
    };
    let legacy_metadata_dir = session_dir.join(ENTRY_METADATA_DIR);
    fs::create_dir_all(&legacy_metadata_dir).unwrap();
    fs::write(
        legacy_metadata_dir.join("entry.json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();

    let state = crate::state::ControlState::open(root.path(), &configured_root).unwrap();
    let state_metadata_dir = state
        .state_session_dir("legacy-session")
        .join(ENTRY_METADATA_DIR);
    let mut conflicting = metadata;
    conflicting.purpose = "operator replacement".to_string();
    fs::write(
        state_metadata_dir.join("entry.json"),
        serde_json::to_vec(&conflicting).unwrap(),
    )
    .unwrap();

    state.import_legacy_control().unwrap();
    state.import_legacy_control().unwrap();

    let mut files = fs::read_dir(state_metadata_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(files, vec!["entry-migrated.json", "entry.json"]);
}

#[test]
fn reaping_missing_payload_removes_external_records_without_a_path_count() {
    let root = tempfile::tempdir().unwrap();
    let config = config(&root);
    let old_manager =
        SessionTmpManager::open_for_user(&config, root.path(), "old-session", "old-thread")
            .unwrap()
            .unwrap();
    let old_payload = old_manager.session_root().to_path_buf();
    let old_state = old_manager.state_session_dir.clone();
    fs::remove_dir_all(old_payload).unwrap();

    let current_manager = SessionTmpManager::open(
        &config,
        root.path(),
        "current-session",
        "current-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let report = current_manager.reap_with_mode(ReapMode::Force).unwrap();

    assert_eq!(report.removed_sessions, 1);
    assert_eq!(report.removed_paths, 0);
    assert!(!old_state.exists());
}

#[test]
fn create_records_session_and_thread_lineage() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let entry = manager
        .create(
            Some("artifact.txt"),
            "compiler output",
            Retention::Manual,
            TempKind::File,
        )
        .unwrap();

    assert_eq!(entry.metadata.session_id, "session-1");
    assert_eq!(entry.metadata.thread_id, "thread-1");
    assert_eq!(entry.metadata.purpose, "compiler output");
    assert_eq!(entry.metadata.retention, Retention::Manual);
    assert!(entry.absolute_path.starts_with(manager.agent_root()));
    assert!(
        manager
            .state_session_dir
            .join(ENTRY_METADATA_DIR)
            .join(format!("{}.json", entry.metadata.id))
            .exists()
    );
}

#[test]
fn root_listing_identifies_entries_from_each_agent_thread() {
    let root = tempfile::tempdir().unwrap();
    let root_manager =
        SessionTmpManager::open_for_user(&config(&root), root.path(), "session-1", "root-thread")
            .unwrap()
            .unwrap();
    let agent_manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "agent-thread",
        SessionTmpOwner::Agent,
    )
    .unwrap()
    .unwrap();

    root_manager
        .create(None, "root output", Retention::Manual, TempKind::File)
        .unwrap();
    agent_manager
        .create(None, "agent output", Retention::Manual, TempKind::File)
        .unwrap();

    let root_listing = root_manager.list().unwrap();
    assert_eq!(root_listing.entries.len(), 2);
    assert!(
        root_listing
            .entries
            .iter()
            .any(|entry| entry.metadata.thread_id == "root-thread")
    );
    assert!(
        root_listing
            .entries
            .iter()
            .any(|entry| entry.metadata.thread_id == "agent-thread")
    );

    let agent_listing = agent_manager.list().unwrap();
    assert_eq!(agent_listing.entries.len(), 1);
    assert_eq!(agent_listing.entries[0].metadata.thread_id, "agent-thread");
}

#[test]
fn user_manager_resolves_an_agent_thread_to_its_owning_session() {
    let root = tempfile::tempdir().unwrap();
    let root_manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "root-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let agent_manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "agent-thread",
        SessionTmpOwner::Agent,
    )
    .unwrap()
    .unwrap();
    agent_manager
        .create(None, "agent output", Retention::Session, TempKind::File)
        .unwrap();

    let user_manager = SessionTmpManager::open_for_user(
        &config(&root),
        root.path(),
        "agent-thread",
        "agent-thread",
    )
    .unwrap()
    .unwrap();

    assert_eq!(user_manager.session_id(), "session-1");
    assert_eq!(user_manager.list().unwrap().entries.len(), 1);
    drop(root_manager);
}

#[test]
fn register_rejects_symlink_escape() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "secret").unwrap();
    let link = manager.agent_root().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&outside, &link).unwrap();

    let error = manager
        .register(&link, "escaped", Retention::Session)
        .unwrap_err();
    assert!(matches!(error, SessionTmpError::PathOutsideAgent(_)));
}

#[test]
fn register_rejects_the_agent_root_without_creating_metadata() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert!(matches!(
        manager.register(Path::new("."), "agent root", Retention::Session),
        Err(SessionTmpError::PathOutsideAgent(_))
    ));
    assert!(manager.list().unwrap().entries.is_empty());
}

#[test]
fn duplicate_live_thread_ownership_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    let result = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    );
    assert!(matches!(
        result,
        Err(SessionTmpError::SessionAlreadyOwned(thread_id)) if thread_id == "thread-1"
    ));
    drop(manager);
}

#[test]
fn session_and_entry_names_must_be_single_path_components() {
    let root = tempfile::tempdir().unwrap();
    assert!(matches!(
        SessionTmpManager::open(
            &config(&root),
            root.path(),
            "session/nested",
            "thread-1",
            SessionTmpOwner::RootSession,
        ),
        Err(SessionTmpError::InvalidComponent(value)) if value == "session/nested"
    ));

    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    assert!(matches!(
        manager.create(
            Some("nested/name"),
            "invalid name",
            Retention::Session,
            TempKind::File,
        ),
        Err(SessionTmpError::InvalidComponent(value)) if value == "nested/name"
    ));
}

#[test]
fn cleanup_rejects_tampered_metadata_before_touching_outside_paths() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "do not remove").unwrap();
    let metadata_path = manager
        .state_session_dir
        .join(ENTRY_METADATA_DIR)
        .join("tampered.json");
    fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
    fs::write(
        metadata_path,
        serde_json::to_vec(&EntryMetadata {
            id: "tampered".to_string(),
            session_id: "session-1".to_string(),
            thread_id: "thread-1".to_string(),
            path: PathBuf::from("../../outside.txt"),
            purpose: "tampered metadata".to_string(),
            retention: Retention::Session,
            created_at: 0,
            expires_at: None,
        })
        .unwrap(),
    )
    .unwrap();

    assert!(matches!(
        manager.clean(),
        Err(SessionTmpError::PathOutsideAgent(_))
    ));
    assert!(outside.exists());
}

#[cfg(unix)]
#[test]
fn cleanup_fails_closed_when_an_agent_root_is_replaced_by_a_symlink() {
    let root = tempfile::tempdir().unwrap();
    let manager =
        SessionTmpManager::open_for_user(&config(&root), root.path(), "session-1", "thread-1")
            .unwrap()
            .unwrap();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "do not remove").unwrap();

    let detached_agent_root = root.path().join("detached-agent-root");
    fs::rename(manager.agent_root(), &detached_agent_root).unwrap();
    std::os::unix::fs::symlink(&outside, manager.agent_root()).unwrap();

    assert!(matches!(
        manager.clean(),
        Err(SessionTmpError::UnsafeManagedPath(_))
    ));
    assert!(outside.join("keep.txt").exists());
}

#[cfg(unix)]
#[test]
fn cleanup_removes_symlink_entries_without_following_them() {
    let root = tempfile::tempdir().unwrap();
    let manager =
        SessionTmpManager::open_for_user(&config(&root), root.path(), "session-1", "thread-1")
            .unwrap()
            .unwrap();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, manager.agent_root().join("escape")).unwrap();

    manager.clean().unwrap();

    assert!(outside.join("keep.txt").exists());
    assert!(!manager.agent_root().join("escape").exists());
}

#[test]
fn agent_manager_cannot_clean_and_drop_does_not_remove_entries() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "agent-1",
        SessionTmpOwner::Agent,
    )
    .unwrap()
    .unwrap();
    let entry = manager
        .create(None, "agent output", Retention::Session, TempKind::File)
        .unwrap();

    assert!(matches!(
        manager.clean(),
        Err(SessionTmpError::CleanupNotOwned)
    ));
    drop(manager);
    assert!(entry.absolute_path.exists());
}

#[test]
fn user_manager_drop_does_not_trigger_automatic_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let manager =
        SessionTmpManager::open_for_user(&config(&root), root.path(), "session-1", "thread-1")
            .unwrap()
            .unwrap();
    let entry = manager
        .create(
            None,
            "inspect before cleanup",
            Retention::Session,
            TempKind::File,
        )
        .unwrap();

    drop(manager);
    assert!(entry.absolute_path.exists());
}

#[test]
fn persisted_metadata_can_be_reopened_after_the_process_handle_drops() {
    let root = tempfile::tempdir().unwrap();
    let entry_id;
    {
        let manager =
            SessionTmpManager::open_for_user(&config(&root), root.path(), "session-1", "thread-1")
                .unwrap()
                .unwrap();
        entry_id = manager
            .create(
                None,
                "survive a process restart",
                Retention::Manual,
                TempKind::File,
            )
            .unwrap()
            .metadata
            .id;
    }

    let reopened =
        SessionTmpManager::open_for_user(&config(&root), root.path(), "session-1", "thread-1")
            .unwrap()
            .unwrap();
    assert_eq!(reopened.list().unwrap().entries[0].metadata.id, entry_id);
}

#[test]
fn clean_removes_expired_ttl_entries() {
    let root = tempfile::tempdir().unwrap();
    let manager =
        SessionTmpManager::open_for_user(&config(&root), root.path(), "session-1", "thread-1")
            .unwrap()
            .unwrap();
    let entry = manager
        .create(
            None,
            "expire immediately",
            Retention::Ttl(0),
            TempKind::File,
        )
        .unwrap();

    let report = manager.clean().unwrap();
    assert_eq!(report.removed_paths, 1);
    assert!(!entry.absolute_path.exists());
    assert!(manager.list().unwrap().entries.is_empty());
}

#[test]
fn clean_preserves_live_agent_roots_and_allows_follow_up_creation() {
    let root = tempfile::tempdir().unwrap();
    let root_manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "root-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let agent_manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "agent-thread",
        SessionTmpOwner::Agent,
    )
    .unwrap()
    .unwrap();
    let entry = agent_manager
        .create(None, "before cleanup", Retention::Session, TempKind::File)
        .unwrap();

    root_manager.clean().unwrap();

    assert!(agent_manager.agent_root().is_dir());
    assert!(entry.absolute_path.exists());
    agent_manager
        .create(None, "after cleanup", Retention::Session, TempKind::File)
        .unwrap();
}

#[test]
fn clear_preserves_live_agent_trees() {
    let root = tempfile::tempdir().unwrap();
    let root_manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "root-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let agent_manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "agent-thread",
        SessionTmpOwner::Agent,
    )
    .unwrap()
    .unwrap();
    let agent_entry = agent_manager
        .create(None, "live agent output", Retention::Manual, TempKind::File)
        .unwrap();
    root_manager
        .create(None, "root output", Retention::Manual, TempKind::File)
        .unwrap();

    root_manager.clear().unwrap();

    assert!(agent_entry.absolute_path.exists());
    assert_eq!(agent_manager.list().unwrap().entries.len(), 1);
    assert_eq!(root_manager.list().unwrap().entries.len(), 1);
    assert_eq!(
        root_manager.list().unwrap().entries[0].metadata.thread_id,
        "agent-thread"
    );
}

#[test]
fn drop_cleans_session_retention_but_preserves_manual_retention() {
    let root = tempfile::tempdir().unwrap();
    let session_root;
    {
        let manager = SessionTmpManager::open(
            &config(&root),
            root.path(),
            "session-1",
            "thread-1",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        session_root = manager.session_root().to_path_buf();
        manager
            .create(None, "remove me", Retention::Session, TempKind::File)
            .unwrap();
        manager
            .create(None, "keep me", Retention::Manual, TempKind::File)
            .unwrap();
    }
    assert!(session_root.exists());
    let remaining = fs::read_dir(session_root.join(AGENTS_DIR).join("thread-1"))
        .unwrap()
        .count();
    assert_eq!(remaining, 1);
}

#[test]
fn cleanup_preserves_contents_of_manual_directories() {
    let root = tempfile::tempdir().unwrap();
    let manual_dir;
    {
        let manager = SessionTmpManager::open(
            &config(&root),
            root.path(),
            "session-1",
            "thread-1",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        let entry = manager
            .create(
                None,
                "keep directory",
                Retention::Manual,
                TempKind::Directory,
            )
            .unwrap();
        manual_dir = entry.absolute_path;
        fs::write(manual_dir.join("nested.txt"), "keep me").unwrap();
        manager
            .create(None, "remove file", Retention::Session, TempKind::File)
            .unwrap();
    }

    assert!(manual_dir.join("nested.txt").exists());
}

#[test]
fn cleanup_preserves_nested_manual_entries_without_preserving_untracked_siblings() {
    let root = tempfile::tempdir().unwrap();
    let manager =
        SessionTmpManager::open_for_user(&config(&root), root.path(), "session-1", "thread-1")
            .unwrap()
            .unwrap();
    let session_dir = manager
        .create(
            None,
            "session directory",
            Retention::Session,
            TempKind::Directory,
        )
        .unwrap()
        .absolute_path;
    let nested_manual = session_dir.join("manual.txt");
    let untracked_sibling = session_dir.join("remove.txt");
    fs::write(&nested_manual, "keep me").unwrap();
    fs::write(&untracked_sibling, "remove me").unwrap();
    manager
        .register(&nested_manual, "nested manual artifact", Retention::Manual)
        .unwrap();

    manager.clean().unwrap();

    assert!(nested_manual.exists());
    assert!(!untracked_sibling.exists());
    assert_eq!(manager.list().unwrap().entries.len(), 2);
}

#[test]
fn clear_only_removes_the_current_session() {
    let root = tempfile::tempdir().unwrap();
    let first = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let second = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-2",
        "thread-2",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    first
        .create(None, "first", Retention::Manual, TempKind::File)
        .unwrap();
    second
        .create(None, "second", Retention::Manual, TempKind::File)
        .unwrap();
    fs::write(
        first.agent_root().join("untracked.txt"),
        "direct shell output",
    )
    .unwrap();

    first.clear().unwrap();

    assert!(first.session_root().exists());
    assert!(second.session_root().exists());
    assert_eq!(first.list().unwrap().entries.len(), 0);
    assert!(first.list().unwrap().untracked_paths.is_empty());
    assert_eq!(second.list().unwrap().entries.len(), 1);
}

#[test]
fn root_open_reaps_stale_sessions_but_zero_disables_reaping() {
    let root = tempfile::tempdir().unwrap();
    let stale_config = config(&root);
    let (old_session_root, old_state_session_root);
    {
        let manager = SessionTmpManager::open(
            &stale_config,
            root.path(),
            "old-session",
            "old-thread",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        old_session_root = manager.session_root().to_path_buf();
        old_state_session_root = manager.state_session_dir.clone();
    }
    fs::write(
        old_state_session_root.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "old-session".to_string(),
            created_at: 0,
            updated_at: 0,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();

    let reaping_manager = SessionTmpManager::open(
        &stale_config,
        root.path(),
        "new-session",
        "new-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    assert!(!old_session_root.exists());
    drop(reaping_manager);

    let zero_root = tempfile::tempdir().unwrap();
    let mut zero_config = config(&zero_root);
    zero_config.stale_after = Duration::ZERO;
    let (old_session_root, old_state_session_root);
    {
        let manager = SessionTmpManager::open(
            &zero_config,
            zero_root.path(),
            "old-session",
            "old-thread",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        old_session_root = manager.session_root().to_path_buf();
        old_state_session_root = manager.state_session_dir.clone();
    }
    fs::write(
        old_state_session_root.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "old-session".to_string(),
            created_at: 0,
            updated_at: 0,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    let _manager = SessionTmpManager::open(
        &zero_config,
        zero_root.path(),
        "new-session",
        "new-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    assert!(old_session_root.exists());
}

#[cfg(unix)]
#[test]
fn root_open_skips_stale_session_with_an_inaccessible_lock() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let stale_config = config(&root);
    let (old_session_root, old_state_session_root);
    {
        let manager = SessionTmpManager::open(
            &stale_config,
            root.path(),
            "old-session",
            "old-thread",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        old_session_root = manager.session_root().to_path_buf();
        old_state_session_root = manager.state_session_dir.clone();
    }
    fs::write(
        old_state_session_root.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "old-session".to_string(),
            created_at: 0,
            updated_at: 0,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    let lock_path = old_state_session_root
        .parent()
        .unwrap()
        .join(".locks")
        .join("old-session.lock");
    let mut permissions = fs::metadata(&lock_path).unwrap().permissions();
    permissions.set_mode(0o400);
    fs::set_permissions(&lock_path, permissions).unwrap();

    let manager = SessionTmpManager::open(
        &stale_config,
        root.path(),
        "new-session",
        "new-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert_eq!(manager.session_id(), "new-session");
    assert!(old_session_root.exists());
}

#[cfg(unix)]
#[test]
fn root_open_rejects_stale_session_with_an_unsafe_lock() {
    let root = tempfile::tempdir().unwrap();
    let stale_config = config(&root);
    let (old_session_root, old_state_session_root);
    {
        let manager = SessionTmpManager::open(
            &stale_config,
            root.path(),
            "old-session",
            "old-thread",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        old_session_root = manager.session_root().to_path_buf();
        old_state_session_root = manager.state_session_dir.clone();
    }
    fs::write(
        old_state_session_root.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "old-session".to_string(),
            created_at: 0,
            updated_at: 0,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    let lock_path = old_state_session_root
        .parent()
        .unwrap()
        .join(".locks")
        .join("old-session.lock");
    fs::remove_file(&lock_path).unwrap();
    std::os::unix::fs::symlink(root.path().join("outside"), &lock_path).unwrap();

    let result = SessionTmpManager::open(
        &stale_config,
        root.path(),
        "new-session",
        "new-thread",
        SessionTmpOwner::RootSession,
    );

    assert!(matches!(
        result,
        Err(SessionTmpError::UnsafeManagedPath(path)) if path == lock_path
    ));
}

#[test]
fn session_operation_lock_serializes_new_leases_with_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let user_manager =
        SessionTmpManager::open_for_user(&config(&root), root.path(), "session-1", "thread-1")
            .unwrap()
            .unwrap();
    let session_lock = crate::storage::lock_session(&user_manager.state_session_dir).unwrap();

    let result = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "agent-thread",
        SessionTmpOwner::Agent,
    );
    assert!(matches!(
        result,
        Err(SessionTmpError::SessionAlreadyOwned(thread_id)) if thread_id == "agent-thread"
    ));

    drop(session_lock);
    assert!(
        SessionTmpManager::open(
            &config(&root),
            root.path(),
            "session-1",
            "agent-thread",
            SessionTmpOwner::Agent,
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn live_lease_protects_a_session_with_an_old_record() {
    let root = tempfile::tempdir().unwrap();
    let stale_config = config(&root);
    let live_manager = SessionTmpManager::open(
        &stale_config,
        root.path(),
        "old-session",
        "old-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    fs::write(
        live_manager.state_session_dir.join(SESSION_METADATA_FILE),
        serde_json::to_vec(&SessionRecord {
            schema_version: 1,
            session_id: "old-session".to_string(),
            created_at: 0,
            updated_at: 0,
            status: "active".to_string(),
        })
        .unwrap(),
    )
    .unwrap();

    let reaping_manager = SessionTmpManager::open(
        &stale_config,
        root.path(),
        "new-session",
        "new-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    assert!(live_manager.session_root().exists());
    reaping_manager.reap(Duration::from_secs(1)).unwrap();
    assert!(live_manager.session_root().exists());
    drop(reaping_manager);
    drop(live_manager);
}

#[test]
fn force_reap_removes_inactive_sessions_but_preserves_live_leases() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(&root);
    config.stale_after = Duration::ZERO;

    let live_manager = SessionTmpManager::open(
        &config,
        root.path(),
        "live-session",
        "live-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let inactive_session_root = {
        let manager = SessionTmpManager::open(
            &config,
            root.path(),
            "inactive-session",
            "inactive-thread",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        manager.session_root().to_path_buf()
    };
    let current_manager = SessionTmpManager::open(
        &config,
        root.path(),
        "current-session",
        "current-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();

    let report = current_manager.reap_with_mode(ReapMode::Force).unwrap();

    assert_eq!(report.removed_paths, 1);
    assert_eq!(report.removed_sessions, 1);
    assert!(!inactive_session_root.exists());
    assert!(live_manager.session_root().exists());
    assert!(current_manager.session_root().exists());
}

#[test]
fn force_reap_preserves_unsafe_session_state_and_continues() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(&root);
    config.stale_after = Duration::ZERO;

    let current_manager = SessionTmpManager::open(
        &config,
        root.path(),
        "current-session",
        "current-thread",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let (unsafe_session_root, unsafe_state_session_root) = {
        let manager = SessionTmpManager::open(
            &config,
            root.path(),
            "unsafe-session",
            "unsafe-thread",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        (
            manager.session_root().to_path_buf(),
            manager.state_session_dir.clone(),
        )
    };
    fs::remove_dir_all(unsafe_state_session_root.join(LEASES_DIR)).unwrap();
    fs::write(
        unsafe_state_session_root.join(LEASES_DIR),
        b"preserve unsafe state",
    )
    .unwrap();
    let (unsafe_lease_entry_root, unsafe_lease_state_session_root) = {
        let manager = SessionTmpManager::open(
            &config,
            root.path(),
            "unsafe-lease-entry",
            "unsafe-thread",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        (
            manager.session_root().to_path_buf(),
            manager.state_session_dir.clone(),
        )
    };
    fs::create_dir(
        unsafe_lease_state_session_root
            .join(LEASES_DIR)
            .join("unexpected-directory"),
    )
    .unwrap();
    let inactive_session_root = {
        let manager = SessionTmpManager::open(
            &config,
            root.path(),
            "inactive-session",
            "inactive-thread",
            SessionTmpOwner::RootSession,
        )
        .unwrap()
        .unwrap();
        manager.session_root().to_path_buf()
    };

    let report = current_manager.reap_with_mode(ReapMode::Force).unwrap();

    assert_eq!(report.removed_paths, 1);
    assert_eq!(report.removed_sessions, 1);
    assert!(!inactive_session_root.exists());
    assert!(unsafe_session_root.exists());
    assert!(unsafe_state_session_root.join(LEASES_DIR).is_file());
    assert!(unsafe_lease_entry_root.exists());
    assert!(
        unsafe_lease_state_session_root
            .join(LEASES_DIR)
            .join("unexpected-directory")
            .is_dir()
    );
}

#[test]
fn purpose_is_bounded_before_any_path_is_created() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionTmpManager::open(
        &config(&root),
        root.path(),
        "session-1",
        "thread-1",
        SessionTmpOwner::RootSession,
    )
    .unwrap()
    .unwrap();
    let purpose = "x".repeat(MAX_PURPOSE_BYTES + 1);

    assert!(matches!(
        manager.create(None, &purpose, Retention::Session, TempKind::File),
        Err(SessionTmpError::PurposeTooLong)
    ));
    assert!(manager.list().unwrap().entries.is_empty());
}
