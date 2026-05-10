use crate::types::AccountsConfig;
use crate::types::AuthCredentialsStoreMode;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub const ACCOUNTS_DIR: &str = "accounts";
pub const ACCOUNT_REGISTRY_FILE: &str = "registry.json";
pub const DEFAULT_ACCOUNT_ALIAS: &str = "default";
const REGISTRY_VERSION: u32 = 1;
const ACCOUNT_REGISTRY_BACKUP_FILE: &str = "registry.json.bak";
const ACCOUNT_REGISTRY_LOCK_FILE: &str = "registry.lock";
static REGISTRY_PROCESS_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRegistry {
    pub version: u32,
    pub updated_at_unix_secs: u64,
    pub accounts: Vec<AccountRegistryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRegistryEntry {
    pub alias: String,
    pub label: String,
    pub source: AccountRegistrySource,
    pub storage_home: PathBuf,
    pub auth_file: PathBuf,
    pub auth_file_present: bool,
    pub credentials_store: AuthCredentialsStoreMode,
    pub last_seen_at_unix_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at_unix_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountRegistrySource {
    Root,
    Config,
    Directory,
    Usage,
}

pub fn accounts_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(ACCOUNTS_DIR)
}

pub fn registry_path(codex_home: &Path) -> PathBuf {
    accounts_dir(codex_home).join(ACCOUNT_REGISTRY_FILE)
}

pub fn account_storage_home(codex_home: &Path, alias: &str) -> PathBuf {
    if is_default_alias(alias) {
        codex_home.to_path_buf()
    } else {
        accounts_dir(codex_home).join(alias)
    }
}

pub fn alias_from_auth_storage_home(codex_home: &Path, auth_storage_home: &Path) -> Option<String> {
    if paths_equal(codex_home, auth_storage_home) {
        return Some(DEFAULT_ACCOUNT_ALIAS.to_string());
    }

    let account_root = accounts_dir(codex_home);
    let relative = auth_storage_home.strip_prefix(account_root).ok()?;
    if relative.components().count() != 1 {
        return None;
    }
    relative
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(normalize_alias)
}

pub fn self_heal_account_registry(
    codex_home: &Path,
    accounts_config: &AccountsConfig,
    default_store: AuthCredentialsStoreMode,
) -> io::Result<AccountRegistry> {
    let directory_aliases = existing_alias_directories(codex_home)?;
    with_locked_registry(codex_home, |registry, now| {
        let mut changed = upsert_entry(
            registry,
            codex_home,
            DEFAULT_ACCOUNT_ALIAS,
            AccountRegistrySource::Root,
            default_store,
            now,
            /*mark_used*/ false,
        );

        if let Some(active) = accounts_config.active.as_deref() {
            changed |= upsert_entry(
                registry,
                codex_home,
                active,
                AccountRegistrySource::Config,
                store_mode_for_alias(active, default_store),
                now,
                /*mark_used*/ false,
            );
        }

        for alias in &accounts_config.rotation {
            changed |= upsert_entry(
                registry,
                codex_home,
                alias,
                AccountRegistrySource::Config,
                store_mode_for_alias(alias, default_store),
                now,
                /*mark_used*/ false,
            );
        }

        for alias in &directory_aliases {
            changed |= upsert_entry(
                registry,
                codex_home,
                alias,
                AccountRegistrySource::Directory,
                store_mode_for_alias(alias, default_store),
                now,
                /*mark_used*/ false,
            );
        }

        changed
    })
}

pub fn record_account_alias_use(
    codex_home: &Path,
    alias: &str,
    store_mode: AuthCredentialsStoreMode,
) -> io::Result<AccountRegistry> {
    with_locked_registry(codex_home, |registry, now| {
        upsert_entry(
            registry,
            codex_home,
            alias,
            if is_default_alias(alias) {
                AccountRegistrySource::Root
            } else {
                AccountRegistrySource::Usage
            },
            store_mode,
            now,
            /*mark_used*/ true,
        )
    })
}

fn with_locked_registry(
    codex_home: &Path,
    mutate: impl FnOnce(&mut AccountRegistry, u64) -> bool,
) -> io::Result<AccountRegistry> {
    let _lock = RegistryLock::acquire(codex_home)?;
    let mut loaded = load_registry_unlocked(codex_home)?;
    let now = unix_now();
    let changed = mutate(&mut loaded.registry, now);
    normalize_registry(&mut loaded.registry);
    if changed || loaded.needs_write {
        write_registry_unlocked(codex_home, &mut loaded.registry, now)?;
    }
    Ok(loaded.registry)
}

struct LoadedRegistry {
    registry: AccountRegistry,
    needs_write: bool,
}

fn load_registry_unlocked(codex_home: &Path) -> io::Result<LoadedRegistry> {
    let path = registry_path(codex_home);
    if !path.exists() {
        return Ok(LoadedRegistry {
            registry: empty_registry(),
            needs_write: false,
        });
    }

    match read_registry_file(&path) {
        Ok(registry) => Ok(LoadedRegistry {
            registry,
            needs_write: false,
        }),
        Err(primary_err) => {
            let backup_path = registry_backup_path(codex_home);
            let backup = read_registry_file(&backup_path).ok();
            quarantine_corrupt_registry(&path)?;
            match backup {
                Some(registry) => Ok(LoadedRegistry {
                    registry,
                    needs_write: true,
                }),
                None => {
                    if backup_path.exists() {
                        let _ = quarantine_corrupt_registry(&backup_path);
                    }
                    let _ = primary_err;
                    Ok(LoadedRegistry {
                        registry: empty_registry(),
                        needs_write: true,
                    })
                }
            }
        }
    }
}

fn empty_registry() -> AccountRegistry {
    AccountRegistry {
        version: REGISTRY_VERSION,
        updated_at_unix_secs: 0,
        accounts: Vec::new(),
    }
}

fn write_registry_unlocked(
    codex_home: &Path,
    registry: &mut AccountRegistry,
    now: u64,
) -> io::Result<()> {
    registry.version = REGISTRY_VERSION;
    registry.updated_at_unix_secs = now;
    normalize_registry(registry);

    let path = registry_path(codex_home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let _ = fs::copy(&path, registry_backup_path(codex_home));
    }
    let temp_path = unique_registry_temp_path(codex_home, now);
    let contents = serde_json::to_string_pretty(registry)
        .map_err(|err| io::Error::other(format!("serialize account registry: {err}")))?;
    fs::write(&temp_path, contents)?;
    fs::rename(temp_path, path)?;
    Ok(())
}

fn normalize_registry(registry: &mut AccountRegistry) {
    registry.version = REGISTRY_VERSION;
    registry.accounts.sort_by(|left, right| {
        account_sort_key(&left.alias)
            .cmp(&account_sort_key(&right.alias))
            .then_with(|| left.alias.cmp(&right.alias))
    });
    registry
        .accounts
        .dedup_by(|left, right| left.alias == right.alias);
}

fn upsert_entry(
    registry: &mut AccountRegistry,
    codex_home: &Path,
    alias: &str,
    source: AccountRegistrySource,
    credentials_store: AuthCredentialsStoreMode,
    now: u64,
    mark_used: bool,
) -> bool {
    let Some(alias) = normalize_alias(alias) else {
        return false;
    };
    let storage_home = account_storage_home(codex_home, &alias);
    let auth_file = storage_home.join("auth.json");
    let auth_file_present = auth_file.exists();

    match registry
        .accounts
        .iter_mut()
        .find(|entry| entry.alias == alias)
    {
        Some(entry) => {
            let label = label_for_alias(&alias);
            let source = choose_source(entry.source, source);
            let mut changed = false;
            if entry.label != label {
                entry.label = label;
                changed = true;
            }
            if entry.source != source {
                entry.source = source;
                changed = true;
            }
            if entry.storage_home != storage_home {
                entry.storage_home = storage_home;
                changed = true;
            }
            if entry.auth_file != auth_file {
                entry.auth_file = auth_file;
                changed = true;
            }
            if entry.auth_file_present != auth_file_present {
                entry.auth_file_present = auth_file_present;
                changed = true;
            }
            if entry.credentials_store != credentials_store {
                entry.credentials_store = credentials_store;
                changed = true;
            }
            if changed || mark_used {
                entry.last_seen_at_unix_secs = now;
            }
            if mark_used && entry.last_used_at_unix_secs != Some(now) {
                entry.last_used_at_unix_secs = Some(now);
                changed = true;
            }
            changed
        }
        None => {
            registry.accounts.push(AccountRegistryEntry {
                label: label_for_alias(&alias),
                alias,
                source,
                storage_home,
                auth_file,
                auth_file_present,
                credentials_store,
                last_seen_at_unix_secs: now,
                last_used_at_unix_secs: mark_used.then_some(now),
            });
            true
        }
    }
}

fn existing_alias_directories(codex_home: &Path) -> io::Result<Vec<String>> {
    let root = accounts_dir(codex_home);
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut aliases = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(alias) = entry.file_name().to_str().and_then(normalize_alias) {
            aliases.push(alias);
        }
    }
    Ok(aliases)
}

fn normalize_alias(alias: &str) -> Option<String> {
    let alias = alias.trim();
    if alias.is_empty() {
        None
    } else if alias.eq_ignore_ascii_case(DEFAULT_ACCOUNT_ALIAS) {
        Some(DEFAULT_ACCOUNT_ALIAS.to_string())
    } else {
        Some(alias.to_string())
    }
}

fn is_default_alias(alias: &str) -> bool {
    alias.eq_ignore_ascii_case(DEFAULT_ACCOUNT_ALIAS)
}

fn label_for_alias(alias: &str) -> String {
    if is_default_alias(alias) {
        "Default".to_string()
    } else {
        alias.to_string()
    }
}

fn store_mode_for_alias(
    alias: &str,
    default_store: AuthCredentialsStoreMode,
) -> AuthCredentialsStoreMode {
    if is_default_alias(alias) {
        default_store
    } else {
        match default_store {
            AuthCredentialsStoreMode::File => AuthCredentialsStoreMode::Auto,
            mode => mode,
        }
    }
}

fn choose_source(
    existing: AccountRegistrySource,
    incoming: AccountRegistrySource,
) -> AccountRegistrySource {
    if source_rank(incoming).ge(&source_rank(existing)) {
        incoming
    } else {
        existing
    }
}

fn source_rank(source: AccountRegistrySource) -> u8 {
    match source {
        AccountRegistrySource::Directory => 1,
        AccountRegistrySource::Config => 2,
        AccountRegistrySource::Usage => 3,
        AccountRegistrySource::Root => 4,
    }
}

fn account_sort_key(alias: &str) -> (u8, String) {
    if is_default_alias(alias) {
        (0, String::new())
    } else {
        (1, alias.to_ascii_lowercase())
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

struct RegistryLock {
    _process_guard: MutexGuard<'static, ()>,
    _file: File,
}

impl RegistryLock {
    fn acquire(codex_home: &Path) -> io::Result<Self> {
        let process_guard = REGISTRY_PROCESS_MUTEX
            .lock()
            .map_err(|_| io::Error::other("account registry process lock poisoned"))?;
        let root = accounts_dir(codex_home);
        fs::create_dir_all(&root)?;
        let lock_path = root.join(ACCOUNT_REGISTRY_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        file.lock()?;
        Ok(Self {
            _process_guard: process_guard,
            _file: file,
        })
    }
}

fn read_registry_file(path: &Path) -> io::Result<AccountRegistry> {
    let contents = fs::read_to_string(path)?;
    serde_json::from_str::<AccountRegistry>(&contents)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn registry_backup_path(codex_home: &Path) -> PathBuf {
    accounts_dir(codex_home).join(ACCOUNT_REGISTRY_BACKUP_FILE)
}

fn unique_registry_temp_path(codex_home: &Path, now: u64) -> PathBuf {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    accounts_dir(codex_home).join(format!(
        "{ACCOUNT_REGISTRY_FILE}.{}.{}.{}.tmp",
        std::process::id(),
        now,
        counter
    ))
}

fn quarantine_corrupt_registry(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let quarantined = path.with_file_name(format!(
        "{}.corrupt.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("registry.json"),
        std::process::id(),
        unix_now()
    ));
    fs::rename(path, quarantined)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;

    #[test]
    fn self_heal_registry_discovers_configured_and_directory_accounts() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(codex_home.path().join("accounts").join("mobian"))
            .expect("create account dir");
        fs::write(codex_home.path().join("auth.json"), "{}").expect("seed root auth");

        let registry = self_heal_account_registry(
            codex_home.path(),
            &AccountsConfig {
                active: Some("work".to_string()),
                rotation: vec!["default".to_string(), "team".to_string()],
            },
            AuthCredentialsStoreMode::File,
        )
        .expect("self heal registry");

        let aliases: Vec<_> = registry
            .accounts
            .iter()
            .map(|entry| entry.alias.as_str())
            .collect();
        assert_eq!(aliases, vec!["default", "mobian", "team", "work"]);

        let by_alias: BTreeMap<_, _> = registry
            .accounts
            .iter()
            .map(|entry| (entry.alias.as_str(), entry))
            .collect();
        assert_eq!(by_alias["default"].label, "Default");
        assert_eq!(
            by_alias["default"].storage_home,
            codex_home.path().to_path_buf()
        );
        assert!(by_alias["default"].auth_file_present);
        assert_eq!(
            by_alias["work"].storage_home,
            codex_home.path().join("accounts").join("work")
        );
        assert_eq!(
            by_alias["work"].credentials_store,
            AuthCredentialsStoreMode::Auto
        );
        assert_eq!(by_alias["mobian"].source, AccountRegistrySource::Directory);
        assert!(registry_path(codex_home.path()).exists());
    }

    #[test]
    fn record_usage_creates_keychain_only_alias_entry() {
        let codex_home = tempfile::tempdir().expect("tempdir");

        let registry =
            record_account_alias_use(codex_home.path(), "mobian", AuthCredentialsStoreMode::Auto)
                .expect("record alias use");

        let entry = registry
            .accounts
            .iter()
            .find(|entry| entry.alias == "mobian")
            .expect("mobian entry");
        assert_eq!(entry.source, AccountRegistrySource::Usage);
        assert_eq!(
            entry.storage_home,
            codex_home.path().join("accounts").join("mobian")
        );
        assert_eq!(
            entry.auth_file,
            codex_home
                .path()
                .join("accounts")
                .join("mobian")
                .join("auth.json")
        );
        assert!(!entry.auth_file_present);
        assert!(entry.last_used_at_unix_secs.is_some());
    }

    #[test]
    fn self_heal_registry_does_not_rewrite_unchanged_entries() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        fs::write(codex_home.path().join("auth.json"), "{}").expect("seed root auth");
        self_heal_account_registry(
            codex_home.path(),
            &AccountsConfig::default(),
            AuthCredentialsStoreMode::File,
        )
        .expect("initial self heal");

        let path = registry_path(codex_home.path());
        let mut registry: AccountRegistry =
            serde_json::from_str(&fs::read_to_string(&path).expect("registry json"))
                .expect("parse registry");
        registry.updated_at_unix_secs = 123;
        registry.accounts[0].last_seen_at_unix_secs = 456;
        fs::write(
            &path,
            serde_json::to_string_pretty(&registry).expect("serialize registry"),
        )
        .expect("rewrite registry timestamps");

        let healed = self_heal_account_registry(
            codex_home.path(),
            &AccountsConfig::default(),
            AuthCredentialsStoreMode::File,
        )
        .expect("second self heal");

        assert_eq!(healed.updated_at_unix_secs, 123);
        assert_eq!(healed.accounts[0].last_seen_at_unix_secs, 456);
        let persisted: AccountRegistry =
            serde_json::from_str(&fs::read_to_string(&path).expect("registry json"))
                .expect("parse registry");
        assert_eq!(persisted.updated_at_unix_secs, 123);
    }

    #[test]
    fn corrupt_registry_is_quarantined_and_restored_from_backup() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(accounts_dir(codex_home.path())).expect("create accounts dir");
        fs::write(registry_path(codex_home.path()), "{not json").expect("write corrupt registry");

        let backup = AccountRegistry {
            version: REGISTRY_VERSION,
            updated_at_unix_secs: 11,
            accounts: vec![AccountRegistryEntry {
                alias: "mobian".to_string(),
                label: "mobian".to_string(),
                source: AccountRegistrySource::Usage,
                storage_home: codex_home.path().join("accounts").join("mobian"),
                auth_file: codex_home
                    .path()
                    .join("accounts")
                    .join("mobian")
                    .join("auth.json"),
                auth_file_present: false,
                credentials_store: AuthCredentialsStoreMode::Auto,
                last_seen_at_unix_secs: 11,
                last_used_at_unix_secs: Some(11),
            }],
        };
        fs::write(
            registry_backup_path(codex_home.path()),
            serde_json::to_string_pretty(&backup).expect("serialize backup"),
        )
        .expect("write registry backup");

        let registry = self_heal_account_registry(
            codex_home.path(),
            &AccountsConfig::default(),
            AuthCredentialsStoreMode::File,
        )
        .expect("self heal from backup");

        let aliases: Vec<_> = registry
            .accounts
            .iter()
            .map(|entry| entry.alias.as_str())
            .collect();
        assert_eq!(aliases, vec!["default", "mobian"]);
        assert!(
            fs::read_dir(accounts_dir(codex_home.path()))
                .expect("read accounts dir")
                .any(|entry| entry
                    .expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("registry.json.corrupt."))
        );
    }

    #[test]
    fn concurrent_registry_writers_preserve_all_aliases() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let aliases: Vec<String> = (0..12).map(|index| format!("alias-{index}")).collect();

        std::thread::scope(|scope| {
            for alias in &aliases {
                let alias = alias.clone();
                let codex_home = codex_home.path().to_path_buf();
                scope.spawn(move || {
                    record_account_alias_use(&codex_home, &alias, AuthCredentialsStoreMode::Auto)
                        .expect("record concurrent account alias");
                });
            }
        });

        let registry: AccountRegistry = serde_json::from_str(
            &fs::read_to_string(registry_path(codex_home.path())).expect("registry json"),
        )
        .expect("parse registry");
        for alias in aliases {
            assert!(
                registry.accounts.iter().any(|entry| entry.alias == alias),
                "missing alias {alias}"
            );
        }
    }

    #[test]
    fn infers_alias_from_selected_auth_storage_home() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let alias_home = codex_home.path().join("accounts").join("mobian");

        assert_eq!(
            alias_from_auth_storage_home(codex_home.path(), codex_home.path()).as_deref(),
            Some(DEFAULT_ACCOUNT_ALIAS)
        );
        assert_eq!(
            alias_from_auth_storage_home(codex_home.path(), &alias_home).as_deref(),
            Some("mobian")
        );
        assert_eq!(
            alias_from_auth_storage_home(codex_home.path(), &alias_home.join("nested")),
            None
        );
    }
}
