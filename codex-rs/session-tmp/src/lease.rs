use super::SessionTmpError;
use super::storage::LEASE_STALE_AFTER;
use super::storage::LEASES_DIR;
use super::storage::LeaseRecord;
use super::storage::ensure_directory_not_symlink;
use super::storage::file_type_is_link;
use super::storage::lease_is_fresh;
use super::storage::now_seconds;
use super::storage::set_private_file;
use super::storage::write_json_atomically;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

pub(super) struct SessionLease {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SessionLease {
    pub(super) fn acquire(
        session_dir: &Path,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Self, SessionTmpError> {
        let Some(_session_lock) = super::storage::try_lock_session(session_dir)? else {
            return Err(SessionTmpError::SessionAlreadyOwned(thread_id.to_string()));
        };
        let leases_dir = session_dir.join(LEASES_DIR);
        ensure_directory_not_symlink(&leases_dir)?;
        fs::create_dir_all(&leases_dir)?;
        super::storage::set_private_directory(&leases_dir)?;
        let path = leases_dir.join(format!("{thread_id}.json"));
        let record = LeaseRecord {
            schema_version: 1,
            session_id: session_id.to_string(),
            thread_id: thread_id.to_string(),
            process_id: std::process::id(),
            updated_at: now_seconds(),
        };

        for _ in 0..2 {
            if fs::symlink_metadata(&path)
                .map(|metadata| file_type_is_link(metadata.file_type()))
                .unwrap_or(false)
            {
                return Err(SessionTmpError::UnsafeManagedPath(path));
            }
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    serde_json::to_writer(&mut file, &record)?;
                    file.sync_all()?;
                    set_private_file(&path)?;
                    let stop = Arc::new(AtomicBool::new(false));
                    let stop_for_thread = Arc::clone(&stop);
                    let path_for_thread = path.clone();
                    let session_id_for_thread = session_id.to_string();
                    let thread_id_for_thread = thread_id.to_string();
                    let thread = match std::thread::Builder::new()
                        .name("codex-session-tmp-lease".to_string())
                        .spawn(move || {
                            while !stop_for_thread.load(Ordering::Relaxed) {
                                let mut remaining = LEASE_HEARTBEAT_INTERVAL;
                                while !remaining.is_zero()
                                    && !stop_for_thread.load(Ordering::Relaxed)
                                {
                                    let wait = remaining.min(Duration::from_millis(100));
                                    std::thread::sleep(wait);
                                    remaining = remaining.saturating_sub(wait);
                                }
                                if stop_for_thread.load(Ordering::Relaxed) {
                                    break;
                                }
                                let record = LeaseRecord {
                                    schema_version: 1,
                                    session_id: session_id_for_thread.clone(),
                                    thread_id: thread_id_for_thread.clone(),
                                    process_id: std::process::id(),
                                    updated_at: now_seconds(),
                                };
                                if let Err(error) = write_json_atomically(&path_for_thread, &record)
                                {
                                    tracing::debug!(
                                        error = %error,
                                        path = %path_for_thread.display(),
                                        "session temporary lease heartbeat failed"
                                    );
                                }
                            }
                        }) {
                        Ok(thread) => thread,
                        Err(error) => {
                            let _ = fs::remove_file(&path);
                            return Err(error.into());
                        }
                    };
                    return Ok(Self {
                        path,
                        stop,
                        thread: Some(thread),
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if lease_is_fresh(&path, LEASE_STALE_AFTER) {
                        return Err(SessionTmpError::SessionAlreadyOwned(thread_id.to_string()));
                    }
                    match fs::remove_file(&path) {
                        Ok(()) => continue,
                        Err(error) if error.kind() == ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }

        Err(SessionTmpError::SessionAlreadyOwned(thread_id.to_string()))
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}
