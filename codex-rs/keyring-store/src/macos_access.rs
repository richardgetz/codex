//! macOS Keychain access policy for credentials owned by signed Codex executables.

#[path = "macos_acl.rs"]
mod macos_acl;

use keyring::Error as KeyringError;
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::os::macos::keychain::SecPreferencesDomain;
use security_framework::os::macos::passwords::find_generic_password;
use std::sync::Mutex;

static ACCESS_POLICY_LOCK: Mutex<()> = Mutex::new(());

pub fn load_with_stable_signed_codex_access(
    service: &str,
    account: &str,
) -> Result<Option<String>, super::CredentialStoreError> {
    let _lock = ACCESS_POLICY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (trusted_application, current_data) = macos_acl::stable_trusted_application()
        .map_err(super::CredentialStoreError::access_policy_failure)?;
    let keychain = SecKeychain::default_for_domain(SecPreferencesDomain::User)
        .map_err(macos_acl::map_security_error)
        .map_err(super::CredentialStoreError::new)?;
    let keychains = [keychain];
    let (password, item) = match find_generic_password(Some(&keychains), service, account) {
        Ok(result) => result,
        Err(error) if error.code() == -25300 => return Ok(None),
        Err(error) => {
            return Err(super::CredentialStoreError::new(
                macos_acl::map_security_error(error),
            ));
        }
    };

    macos_acl::normalize_item_access(&item, service, &trusted_application, &current_data)
        .map_err(super::CredentialStoreError::access_policy_failure)?;
    let value = String::from_utf8(password.as_ref().to_vec()).map_err(|error| {
        super::CredentialStoreError::new(KeyringError::BadEncoding(error.into_bytes()))
    })?;
    Ok(Some(value))
}

pub fn save_with_stable_signed_codex_access(
    service: &str,
    account: &str,
    value: &str,
) -> Result<(), super::CredentialStoreError> {
    let _lock = ACCESS_POLICY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (trusted_application, current_data) = macos_acl::stable_trusted_application()
        .map_err(super::CredentialStoreError::access_policy_failure)?;
    let keychain = SecKeychain::default_for_domain(SecPreferencesDomain::User)
        .map_err(macos_acl::map_security_error)
        .map_err(super::CredentialStoreError::new)?;
    let keychains = [keychain.clone()];

    let existing_item = match find_generic_password(Some(&keychains), service, account) {
        Ok((_, item)) => Some(item),
        Err(error) if error.code() == -25300 => None,
        Err(error) => {
            return Err(super::CredentialStoreError::new(
                macos_acl::map_security_error(error),
            ));
        }
    };

    if let Some(item) = existing_item {
        macos_acl::normalize_item_access(&item, service, &trusted_application, &current_data)
            .map_err(super::CredentialStoreError::access_policy_failure)?;
        // Update the item reference returned by the exact lookup above. Entry::set_password would
        // perform a second service/account lookup and could write to a replacement item after a
        // concurrent delete/recreate. Any value-update failure is policy-fatal so Auto cannot
        // write a newer value to a fallback file that loses to this stale keychain item.
        macos_acl::modify_item_data(&item, value)
            .map_err(super::CredentialStoreError::access_policy_failure)?;
        return Ok(());
    }

    // Supply the signed application in the initial access object so the OAuth value is never
    // created under Keychain's default access policy. The returned item is then normalized by
    // reference; no service/account relookup or broad cleanup is needed.
    let access = macos_acl::initial_access(&trusted_application, service)
        .map_err(super::CredentialStoreError::access_policy_failure)?;
    let item = macos_acl::create_item(&keychain, service, account, value, &access)
        .map_err(super::CredentialStoreError::access_policy_failure)?;
    macos_acl::normalize_item_access(&item, service, &trusted_application, &current_data)
        .map_err(super::CredentialStoreError::access_policy_failure)?;
    Ok(())
}

pub fn delete_with_stable_signed_codex_access(
    service: &str,
    account: &str,
) -> Result<bool, super::CredentialStoreError> {
    let _lock = ACCESS_POLICY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    macos_acl::stable_trusted_application()
        .map_err(super::CredentialStoreError::access_policy_failure)?;
    let keychain = SecKeychain::default_for_domain(SecPreferencesDomain::User)
        .map_err(macos_acl::map_security_error)
        .map_err(super::CredentialStoreError::new)?;
    let keychains = [keychain];
    let (_, item) = match find_generic_password(Some(&keychains), service, account) {
        Ok(result) => result,
        Err(error) if error.code() == -25300 => return Ok(false),
        Err(error) => {
            return Err(super::CredentialStoreError::new(
                macos_acl::map_security_error(error),
            ));
        }
    };
    macos_acl::delete_item(&item).map_err(super::CredentialStoreError::new)?;
    match find_generic_password(Some(&keychains), service, account) {
        Err(error) if error.code() == -25300 => Ok(true),
        Ok(_) => Err(super::CredentialStoreError::access_policy_failure(
            KeyringError::Invalid(
                "keychain item".to_string(),
                "item remained after deletion".to_string(),
            ),
        )),
        Err(error) => Err(super::CredentialStoreError::new(
            macos_acl::map_security_error(error),
        )),
    }
}
