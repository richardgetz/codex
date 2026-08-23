//! Raw Security.framework bindings and ACL normalization for macOS Keychain items.

use core_foundation::array::CFArray;
use core_foundation::array::CFArrayRef;
use core_foundation::base::CFEqual;
use core_foundation::base::CFRelease;
use core_foundation::base::CFTypeID;
use core_foundation::base::CFTypeRef;
use core_foundation::base::TCFType;
use core_foundation::data::CFData;
use core_foundation::data::CFDataRef;
use core_foundation::declare_TCFType;
use core_foundation::impl_TCFType;
use core_foundation::string::CFString;
use core_foundation::string::CFStringRef;
use keyring::Error as KeyringError;
use security_framework::base::Error as SecurityError;
use security_framework::os::macos::access::SecAccess;
use security_framework::os::macos::code_signing::Flags;
use security_framework::os::macos::code_signing::SecCode;
use security_framework::os::macos::code_signing::SecRequirement;
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::os::macos::keychain_item::SecKeychainItem;
use std::ffi::c_char;
use std::ffi::c_void;
use std::ptr;

// The fork release signs and ships the `codex` executable. A separately shipped app-server binary
// may use this policy only when it is independently signed by the same Developer ID team.
const STABLE_CODE_REQUIREMENT: &str = concat!(
    "(identifier \"codex\" or identifier \"codex-app-server\") and ",
    "anchor apple generic and ",
    "certificate 1[field.1.2.840.113635.100.6.2.6] exists and ",
    "certificate leaf[field.1.2.840.113635.100.6.1.13] exists and ",
    "certificate leaf[subject.OU] = \"W9HW8JL7CP\"",
);

type OSStatus = i32;
type SecAccessRef = *mut c_void;
type SecACLRef = *mut c_void;
type SecKeychainPromptSelector = u16;

// Apple exposes trusted-application data as opaque, but the file-backed ACL format starts with a
// 20-byte legacy CDSA code hash followed by the path and designated requirement. The
// keychain-db interoperability implementation documents that macOS accepts zero for this legacy
// field and uses it for requirement-only ACLs. Keep this compatibility behavior isolated here.
const LEGACY_CODE_HASH_LENGTH: usize = 20;
const GENERIC_PASSWORD_ITEM_CLASS: u32 = u32::from_be_bytes(*b"genp");
const SERVICE_ATTRIBUTE_TAG: u32 = u32::from_be_bytes(*b"svce");
const ACCOUNT_ATTRIBUTE_TAG: u32 = u32::from_be_bytes(*b"acct");

#[repr(C)]
struct SecKeychainAttribute {
    tag: u32,
    length: u32,
    data: *mut c_void,
}

#[repr(C)]
struct SecKeychainAttributeList {
    count: u32,
    attr: *mut SecKeychainAttribute,
}

pub enum OpaqueSecTrustedApplication {}
type SecTrustedApplicationRef = *mut OpaqueSecTrustedApplication;

declare_TCFType! {
    SecTrustedApplication, SecTrustedApplicationRef
}
impl_TCFType!(
    SecTrustedApplication,
    SecTrustedApplicationRef,
    SecTrustedApplicationGetTypeID
);

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecTrustedApplicationGetTypeID() -> CFTypeID;
    fn SecTrustedApplicationCreateFromPath(
        path: *const c_char,
        app: *mut SecTrustedApplicationRef,
    ) -> OSStatus;
    fn SecTrustedApplicationCopyData(
        app: SecTrustedApplicationRef,
        data: *mut CFDataRef,
    ) -> OSStatus;
    fn SecTrustedApplicationSetData(app: SecTrustedApplicationRef, data: CFDataRef) -> OSStatus;

    fn SecAccessCreate(
        descriptor: CFStringRef,
        trusted_list: CFArrayRef,
        access: *mut SecAccessRef,
    ) -> OSStatus;

    fn SecKeychainItemCreateFromContent(
        item_class: u32,
        attributes: *mut SecKeychainAttributeList,
        data_length: u32,
        data: *const c_void,
        keychain: *mut c_void,
        initial_access: SecAccessRef,
        item: *mut *mut c_void,
    ) -> OSStatus;
    fn SecKeychainItemModifyAttributesAndData(
        item: *mut c_void,
        attributes: *const c_void,
        data_length: u32,
        data: *const c_void,
    ) -> OSStatus;
    fn SecKeychainItemDelete(item: *mut c_void) -> OSStatus;

    fn SecKeychainItemCopyAccess(item: *mut c_void, access: *mut SecAccessRef) -> OSStatus;
    fn SecKeychainItemSetAccess(item: *mut c_void, access: SecAccessRef) -> OSStatus;
    fn SecAccessCopyMatchingACLList(
        access: SecAccessRef,
        authorization: *const c_void,
    ) -> CFArrayRef;
    fn SecACLCopyContents(
        acl: SecACLRef,
        application_list: *mut CFArrayRef,
        description: *mut CFStringRef,
        prompt_selector: *mut SecKeychainPromptSelector,
    ) -> OSStatus;
    fn SecACLCopyAuthorizations(acl: SecACLRef) -> CFArrayRef;
    fn SecACLSetContents(
        acl: SecACLRef,
        application_list: CFArrayRef,
        description: CFStringRef,
        prompt_selector: SecKeychainPromptSelector,
    ) -> OSStatus;

    static kSecACLAuthorizationKeychainItemRead: CFStringRef;
    static kSecACLAuthorizationKeychainItemModify: CFStringRef;
    static kSecACLAuthorizationKeychainItemDelete: CFStringRef;
}

pub(super) fn stable_trusted_application() -> Result<(SecTrustedApplication, Vec<u8>), KeyringError>
{
    validate_signed_codex_process()?;
    let trusted_application = trusted_application_from_self()?;
    let current_data = canonical_trusted_application_data(&trusted_application)?;
    let data = CFData::from_buffer(&current_data);
    check_status(unsafe {
        SecTrustedApplicationSetData(
            trusted_application.as_concrete_TypeRef(),
            data.as_concrete_TypeRef(),
        )
    })?;
    Ok((trusted_application, current_data))
}

pub(super) fn initial_access(
    trusted_application: &SecTrustedApplication,
    descriptor: &str,
) -> Result<SecAccess, KeyringError> {
    let trusted_applications = [clone_trusted_application(trusted_application)];
    let applications = CFArray::from_CFTypes(&trusted_applications);
    let descriptor = CFString::new(descriptor);
    let mut access = ptr::null_mut();
    check_status(unsafe {
        SecAccessCreate(
            descriptor.as_concrete_TypeRef(),
            applications.as_CFTypeRef() as CFArrayRef,
            &mut access,
        )
    })?;
    if access.is_null() {
        return Err(KeyringError::Invalid(
            "keychain access".to_string(),
            "Security.framework returned a null access object".to_string(),
        ));
    }
    Ok(unsafe { SecAccess::wrap_under_create_rule(access.cast()) })
}

pub(super) fn create_item(
    keychain: &SecKeychain,
    service: &str,
    account: &str,
    value: &str,
    access: &SecAccess,
) -> Result<SecKeychainItem, KeyringError> {
    let service_length = u32::try_from(service.len())
        .map_err(|_| KeyringError::TooLong("service".to_string(), u32::MAX))?;
    let account_length = u32::try_from(account.len())
        .map_err(|_| KeyringError::TooLong("account".to_string(), u32::MAX))?;
    let value_length = u32::try_from(value.len())
        .map_err(|_| KeyringError::TooLong("password".to_string(), u32::MAX))?;
    let mut attributes = [
        SecKeychainAttribute {
            tag: SERVICE_ATTRIBUTE_TAG,
            length: service_length,
            data: service.as_ptr() as *mut c_void,
        },
        SecKeychainAttribute {
            tag: ACCOUNT_ATTRIBUTE_TAG,
            length: account_length,
            data: account.as_ptr() as *mut c_void,
        },
    ];
    let mut attribute_list = SecKeychainAttributeList {
        count: attributes.len() as u32,
        attr: attributes.as_mut_ptr(),
    };
    let mut item = ptr::null_mut();
    check_status(unsafe {
        SecKeychainItemCreateFromContent(
            GENERIC_PASSWORD_ITEM_CLASS,
            &mut attribute_list,
            value_length,
            value.as_ptr() as *const c_void,
            keychain.as_concrete_TypeRef().cast(),
            access.as_concrete_TypeRef().cast(),
            &mut item,
        )
    })?;
    if item.is_null() {
        return Err(KeyringError::Invalid(
            "keychain item".to_string(),
            "Security.framework returned a null item".to_string(),
        ));
    }
    Ok(unsafe { SecKeychainItem::wrap_under_create_rule(item.cast()) })
}

pub(super) fn modify_item_data(item: &SecKeychainItem, value: &str) -> Result<(), KeyringError> {
    let value_length = u32::try_from(value.len())
        .map_err(|_| KeyringError::TooLong("password".to_string(), u32::MAX))?;
    check_status(unsafe {
        SecKeychainItemModifyAttributesAndData(
            item.as_concrete_TypeRef() as *mut c_void,
            ptr::null(),
            value_length,
            value.as_ptr() as *const c_void,
        )
    })
}

pub(super) fn delete_item(item: &SecKeychainItem) -> Result<(), KeyringError> {
    check_status(unsafe { SecKeychainItemDelete(item.as_concrete_TypeRef() as *mut c_void) })
}

pub(super) fn normalize_item_access(
    item: &SecKeychainItem,
    descriptor: &str,
    trusted_application: &SecTrustedApplication,
    current_data: &[u8],
) -> Result<(), KeyringError> {
    let mut access = ptr::null_mut();
    check_status(unsafe {
        SecKeychainItemCopyAccess(item.as_concrete_TypeRef() as *mut c_void, &mut access)
    })?;

    let result = (|| {
        // A generic password has operation ACLs plus a separate owner/administrative ACL. Only
        // update read/modify/delete ACLs so this migration cannot grant Codex ownership or ACL
        // administration privileges.
        let mut acl_lists = Vec::new();
        for authorization in [
            unsafe { kSecACLAuthorizationKeychainItemRead },
            unsafe { kSecACLAuthorizationKeychainItemModify },
            unsafe { kSecACLAuthorizationKeychainItemDelete },
        ] {
            let acl_list =
                unsafe { SecAccessCopyMatchingACLList(access, authorization as *const c_void) };
            if acl_list.is_null() {
                continue;
            }
            acl_lists.push(unsafe { CFArray::<*const c_void>::wrap_under_create_rule(acl_list) });
        }
        let mut selected_acls = Vec::new();
        for acl_list in &acl_lists {
            for acl in acl_list.get_all_values() {
                let acl = acl as SecACLRef;
                if !selected_acls.contains(&acl) {
                    selected_acls.push(acl);
                }
            }
        }
        if selected_acls.is_empty() {
            return Err(KeyringError::Invalid(
                "keychain access".to_string(),
                "no item operation ACLs found".to_string(),
            ));
        }
        let mut updates = Vec::new();

        for acl in selected_acls {
            ensure_supported_authorizations(acl)?;
            let mut application_list: CFArrayRef = ptr::null();
            let mut description: CFStringRef = ptr::null();
            let mut prompt_selector: SecKeychainPromptSelector = 0;
            check_status(unsafe {
                SecACLCopyContents(
                    acl,
                    &mut application_list,
                    &mut description,
                    &mut prompt_selector,
                )
            })?;

            let description = if description.is_null() {
                None
            } else {
                Some(unsafe { CFString::wrap_under_create_rule(description) })
            };
            if application_list.is_null() {
                // A null application list means unrestricted access. Preserve that explicit user
                // choice rather than silently narrowing access for other legitimate clients.
                continue;
            }
            let application_list =
                unsafe { CFArray::<*const c_void>::wrap_under_create_rule(application_list) };
            let mut trusted_applications = Vec::new();
            let mut already_trusted = false;
            for application in application_list.get_all_values() {
                let application = unsafe {
                    SecTrustedApplication::wrap_under_get_rule(
                        application as SecTrustedApplicationRef,
                    )
                };
                // Ignore the legacy code-hash prefix when comparing entries. This makes old
                // path/hash-bound entries equivalent to the requirement-only representation and
                // keeps migration idempotent across subsequent reads.
                if trusted_application_data(&application)
                    .is_ok_and(|data| canonicalize_trusted_application_data(&data) == current_data)
                {
                    already_trusted = true;
                }
                trusted_applications.push(application);
            }

            if already_trusted {
                continue;
            }

            trusted_applications.push(clone_trusted_application(trusted_application));
            let applications = CFArray::from_CFTypes(&trusted_applications);
            let description = description.unwrap_or_else(|| CFString::new(descriptor));
            updates.push((acl, applications, description, prompt_selector));
        }

        let had_updates = !updates.is_empty();
        for (acl, applications, description, prompt_selector) in updates {
            check_status(unsafe {
                SecACLSetContents(
                    acl,
                    applications.as_CFTypeRef() as CFArrayRef,
                    description.as_concrete_TypeRef(),
                    prompt_selector,
                )
            })?;
        }

        if had_updates {
            check_status(unsafe {
                SecKeychainItemSetAccess(item.as_concrete_TypeRef() as *mut c_void, access)
            })?;
        }
        Ok(())
    })();

    unsafe { CFRelease(access as *const c_void) };
    result
}

fn ensure_supported_authorizations(acl: SecACLRef) -> Result<(), KeyringError> {
    let authorizations = unsafe { SecACLCopyAuthorizations(acl) };
    if authorizations.is_null() {
        return Err(KeyringError::Invalid(
            "keychain access".to_string(),
            "ACL authorizations were unavailable".to_string(),
        ));
    }
    let authorizations = unsafe { CFArray::<CFStringRef>::wrap_under_create_rule(authorizations) };
    let supported = [
        unsafe { kSecACLAuthorizationKeychainItemRead },
        unsafe { kSecACLAuthorizationKeychainItemModify },
        unsafe { kSecACLAuthorizationKeychainItemDelete },
    ];
    if authorizations.get_all_values().is_empty()
        || authorizations.get_all_values().iter().any(|authorization| {
            !supported.iter().any(|supported| unsafe {
                CFEqual(*authorization as CFTypeRef, *supported as CFTypeRef) != 0
            })
        })
    {
        return Err(KeyringError::Invalid(
            "keychain access".to_string(),
            "ACL contains an unsupported authorization".to_string(),
        ));
    }
    Ok(())
}

fn clone_trusted_application(application: &SecTrustedApplication) -> SecTrustedApplication {
    unsafe { SecTrustedApplication::wrap_under_get_rule(application.as_concrete_TypeRef()) }
}

fn trusted_application_data(application: &SecTrustedApplication) -> Result<Vec<u8>, KeyringError> {
    let mut data: CFDataRef = ptr::null();
    check_status(unsafe {
        SecTrustedApplicationCopyData(application.as_concrete_TypeRef(), &mut data)
    })?;
    let data = unsafe { CFData::wrap_under_create_rule(data) };
    Ok(data.bytes().to_vec())
}

fn canonical_trusted_application_data(
    application: &SecTrustedApplication,
) -> Result<Vec<u8>, KeyringError> {
    let data = trusted_application_data(application)?;
    if data.len() < LEGACY_CODE_HASH_LENGTH {
        return Err(KeyringError::Invalid(
            "trusted application".to_string(),
            "unexpected trusted-application data".to_string(),
        ));
    }
    Ok(canonicalize_trusted_application_data(&data))
}

fn canonicalize_trusted_application_data(data: &[u8]) -> Vec<u8> {
    let mut canonical = data.to_vec();
    if canonical.len() >= LEGACY_CODE_HASH_LENGTH {
        canonical[..LEGACY_CODE_HASH_LENGTH].fill(0);
    }
    canonical
}

fn validate_signed_codex_process() -> Result<(), KeyringError> {
    let code = SecCode::for_self(Flags::STRICT_VALIDATE).map_err(map_security_error)?;
    let requirement: SecRequirement = STABLE_CODE_REQUIREMENT
        .parse()
        .map_err(map_security_error)?;
    code.check_validity(Flags::STRICT_VALIDATE, &requirement)
        .map_err(map_security_error)
}

fn trusted_application_from_self() -> Result<SecTrustedApplication, KeyringError> {
    let mut application = ptr::null_mut();
    // A null path asks Security.framework to bind the trusted application to this process,
    // avoiding a second path lookup after the code-signature validation above.
    let status = unsafe { SecTrustedApplicationCreateFromPath(ptr::null(), &mut application) };
    if status != 0 {
        return Err(map_security_status(status));
    }
    Ok(unsafe { SecTrustedApplication::wrap_under_create_rule(application) })
}

fn check_status(status: OSStatus) -> Result<(), KeyringError> {
    if status == 0 {
        Ok(())
    } else {
        Err(map_security_status(status))
    }
}

pub(super) fn map_security_error(error: SecurityError) -> KeyringError {
    map_security_status(error.code())
}

fn map_security_status(status: OSStatus) -> KeyringError {
    let error = SecurityError::from_code(status);
    match status {
        -25291 | -25292 | -25294 | -25295 => KeyringError::NoStorageAccess(Box::new(error)),
        -25300 => KeyringError::NoEntry,
        _ => KeyringError::PlatformFailure(Box::new(error)),
    }
}
