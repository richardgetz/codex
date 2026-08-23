use keyring::Entry;
use keyring::Error as KeyringError;
use std::error::Error;
use std::fmt;
use std::fmt::Debug;
use tracing::trace;

#[cfg(target_os = "macos")]
mod macos_access;

#[derive(Debug)]
pub enum CredentialStoreError {
    /// The credential backend returned an ordinary storage error.
    Other(KeyringError),
}

/// Marks a keyring error caused by failure to establish or verify a requested access policy.
#[derive(Debug)]
pub struct AccessPolicyFailure(KeyringError);

impl fmt::Display for AccessPolicyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "keyring access policy failure: {}", self.0)
    }
}

impl Error for AccessPolicyFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// Access policy for a credential item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyringAccessPolicy {
    /// Trust the stable signed Codex executable on macOS.
    ///
    /// On other platforms this policy has no additional effect because their credential stores
    /// do not expose the macOS Keychain ACL model.
    StableSignedCodex,
}

impl CredentialStoreError {
    pub fn new(error: KeyringError) -> Self {
        Self::Other(error)
    }

    pub fn access_policy_failure(operation: KeyringError) -> Self {
        Self::Other(KeyringError::PlatformFailure(Box::new(
            AccessPolicyFailure(operation),
        )))
    }

    pub fn is_access_policy_failure(&self) -> bool {
        match self {
            Self::Other(error) => error
                .source()
                .is_some_and(|source| source.is::<AccessPolicyFailure>()),
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Other(error) => error.to_string(),
        }
    }

    pub fn into_error(self) -> KeyringError {
        match self {
            Self::Other(error) => error,
        }
    }
}

impl fmt::Display for CredentialStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(error) => write!(f, "{error}"),
        }
    }
}

impl Error for CredentialStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Other(error) => Some(error),
        }
    }
}

/// Shared credential store abstraction for keyring-backed implementations.
pub trait KeyringStore: Debug + Send + Sync {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError>;
    fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError>;
    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError>;

    /// Loads a credential while applying an optional platform-specific access policy.
    fn load_with_access_policy(
        &self,
        service: &str,
        account: &str,
        policy: KeyringAccessPolicy,
    ) -> Result<Option<String>, CredentialStoreError> {
        match policy {
            KeyringAccessPolicy::StableSignedCodex => self.load(service, account),
        }
    }

    /// Saves a credential while applying an optional platform-specific access policy.
    fn save_with_access_policy(
        &self,
        service: &str,
        account: &str,
        value: &str,
        policy: KeyringAccessPolicy,
    ) -> Result<(), CredentialStoreError> {
        match policy {
            KeyringAccessPolicy::StableSignedCodex => self.save(service, account, value),
        }
    }

    /// Deletes a credential while applying an optional platform-specific access policy.
    fn delete_with_access_policy(
        &self,
        service: &str,
        account: &str,
        policy: KeyringAccessPolicy,
    ) -> Result<bool, CredentialStoreError> {
        match policy {
            KeyringAccessPolicy::StableSignedCodex => self.delete(service, account),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DefaultKeyringStore;

impl KeyringStore for DefaultKeyringStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
        trace!("keyring.load start, service={service}, account={account}");
        let entry = Entry::new(service, account).map_err(CredentialStoreError::new)?;
        match entry.get_password() {
            Ok(password) => {
                trace!("keyring.load success, service={service}, account={account}");
                Ok(Some(password))
            }
            Err(keyring::Error::NoEntry) => {
                trace!("keyring.load no entry, service={service}, account={account}");
                Ok(None)
            }
            Err(error) => {
                trace!("keyring.load error, service={service}, account={account}, error={error}");
                Err(CredentialStoreError::new(error))
            }
        }
    }

    fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError> {
        trace!(
            "keyring.save start, service={service}, account={account}, value_len={}",
            value.len()
        );
        let entry = Entry::new(service, account).map_err(CredentialStoreError::new)?;
        match entry.set_password(value) {
            Ok(()) => {
                trace!("keyring.save success, service={service}, account={account}");
                Ok(())
            }
            Err(error) => {
                trace!("keyring.save error, service={service}, account={account}, error={error}");
                Err(CredentialStoreError::new(error))
            }
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError> {
        trace!("keyring.delete start, service={service}, account={account}");
        let entry = Entry::new(service, account).map_err(CredentialStoreError::new)?;
        match entry.delete_credential() {
            Ok(()) => {
                trace!("keyring.delete success, service={service}, account={account}");
                Ok(true)
            }
            Err(keyring::Error::NoEntry) => {
                trace!("keyring.delete no entry, service={service}, account={account}");
                Ok(false)
            }
            Err(error) => {
                trace!("keyring.delete error, service={service}, account={account}, error={error}");
                Err(CredentialStoreError::new(error))
            }
        }
    }

    fn load_with_access_policy(
        &self,
        service: &str,
        account: &str,
        policy: KeyringAccessPolicy,
    ) -> Result<Option<String>, CredentialStoreError> {
        #[cfg(target_os = "macos")]
        {
            match policy {
                KeyringAccessPolicy::StableSignedCodex => {
                    return macos_access::load_with_stable_signed_codex_access(service, account);
                }
            }
        }

        #[cfg(not(target_os = "macos"))]
        let _ = policy;
        self.load(service, account)
    }

    fn save_with_access_policy(
        &self,
        service: &str,
        account: &str,
        value: &str,
        policy: KeyringAccessPolicy,
    ) -> Result<(), CredentialStoreError> {
        #[cfg(target_os = "macos")]
        {
            match policy {
                KeyringAccessPolicy::StableSignedCodex => {
                    return macos_access::save_with_stable_signed_codex_access(
                        service, account, value,
                    );
                }
            }
        }

        #[cfg(not(target_os = "macos"))]
        let _ = policy;
        self.save(service, account, value)
    }

    fn delete_with_access_policy(
        &self,
        service: &str,
        account: &str,
        policy: KeyringAccessPolicy,
    ) -> Result<bool, CredentialStoreError> {
        #[cfg(target_os = "macos")]
        {
            match policy {
                KeyringAccessPolicy::StableSignedCodex => {
                    return macos_access::delete_with_stable_signed_codex_access(service, account);
                }
            }
        }

        #[cfg(not(target_os = "macos"))]
        let _ = policy;
        self.delete(service, account)
    }
}

pub mod tests {
    use super::CredentialStoreError;
    use super::KeyringAccessPolicy;
    use super::KeyringStore;
    use keyring::Error as KeyringError;
    use keyring::credential::CredentialApi as _;
    use keyring::mock::MockCredential;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::PoisonError;

    #[derive(Default, Clone, Debug)]
    pub struct MockKeyringStore {
        credentials: Arc<Mutex<HashMap<String, Arc<MockCredential>>>>,
        access_policies: Arc<Mutex<HashMap<String, KeyringAccessPolicy>>>,
        access_policy_errors: Arc<Mutex<HashMap<String, KeyringError>>>,
    }

    impl MockKeyringStore {
        pub fn credential(&self, account: &str) -> Arc<MockCredential> {
            let mut guard = self
                .credentials
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard
                .entry(account.to_string())
                .or_insert_with(|| Arc::new(MockCredential::default()))
                .clone()
        }

        pub fn saved_value(&self, account: &str) -> Option<String> {
            let credential = {
                let guard = self
                    .credentials
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                guard.get(account).cloned()
            }?;
            credential.get_password().ok()
        }

        pub fn set_error(&self, account: &str, error: KeyringError) {
            let credential = self.credential(account);
            credential.set_error(error);
        }

        pub fn contains(&self, account: &str) -> bool {
            let guard = self
                .credentials
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard.contains_key(account)
        }

        pub fn access_policy(&self, account: &str) -> Option<KeyringAccessPolicy> {
            let guard = self
                .access_policies
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard.get(account).copied()
        }

        pub fn set_access_policy_error(&self, account: &str, error: KeyringError) {
            self.access_policy_errors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(account.to_string(), error);
        }

        fn take_access_policy_error(&self, account: &str) -> Option<CredentialStoreError> {
            self.access_policy_errors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(account)
                .map(CredentialStoreError::access_policy_failure)
        }
    }

    impl KeyringStore for MockKeyringStore {
        fn load(
            &self,
            _service: &str,
            account: &str,
        ) -> Result<Option<String>, CredentialStoreError> {
            let credential = {
                let guard = self
                    .credentials
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                guard.get(account).cloned()
            };

            let Some(credential) = credential else {
                return Ok(None);
            };

            match credential.get_password() {
                Ok(password) => Ok(Some(password)),
                Err(KeyringError::NoEntry) => Ok(None),
                Err(error) => Err(CredentialStoreError::new(error)),
            }
        }

        fn save(
            &self,
            _service: &str,
            account: &str,
            value: &str,
        ) -> Result<(), CredentialStoreError> {
            let credential = self.credential(account);
            credential
                .set_password(value)
                .map_err(CredentialStoreError::new)
        }

        fn delete(&self, _service: &str, account: &str) -> Result<bool, CredentialStoreError> {
            let credential = {
                let guard = self
                    .credentials
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                guard.get(account).cloned()
            };

            let Some(credential) = credential else {
                return Ok(false);
            };

            let removed = match credential.delete_credential() {
                Ok(()) => Ok(true),
                Err(KeyringError::NoEntry) => Ok(false),
                Err(error) => Err(CredentialStoreError::new(error)),
            }?;

            let mut guard = self
                .credentials
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard.remove(account);
            Ok(removed)
        }

        fn load_with_access_policy(
            &self,
            service: &str,
            account: &str,
            policy: KeyringAccessPolicy,
        ) -> Result<Option<String>, CredentialStoreError> {
            self.access_policies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(account.to_string(), policy);
            if let Some(error) = self.take_access_policy_error(account) {
                return Err(error);
            }
            self.load(service, account)
        }

        fn save_with_access_policy(
            &self,
            service: &str,
            account: &str,
            value: &str,
            policy: KeyringAccessPolicy,
        ) -> Result<(), CredentialStoreError> {
            self.access_policies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(account.to_string(), policy);
            if let Some(error) = self.take_access_policy_error(account) {
                return Err(error);
            }
            self.save(service, account, value)
        }
    }
}
