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
            .session_root()
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
        .session_root()
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
    let old_session_root;
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
    }
    fs::write(
        old_session_root.join(SESSION_METADATA_FILE),
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
    let old_session_root;
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
    }
    fs::write(
        old_session_root.join(SESSION_METADATA_FILE),
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
    let old_session_root;
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
    }
    fs::write(
        old_session_root.join(SESSION_METADATA_FILE),
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
    let lock_path = old_session_root
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
    let old_session_root;
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
    }
    fs::write(
        old_session_root.join(SESSION_METADATA_FILE),
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
    let lock_path = old_session_root
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
    let session_lock = crate::storage::lock_session(user_manager.session_root()).unwrap();

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
        live_manager.session_root().join(SESSION_METADATA_FILE),
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
