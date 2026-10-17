//! Generic credentials from the Windows Credential Manager.

use windows::{
    Win32::{
        Foundation::ERROR_NOT_FOUND,
        Security::Credentials::{CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW},
    },
    core::HSTRING,
};

use crate::{Credential, Error, Result};

/// Reads a generic credential, as created by
/// `cmdkey /generic:<target> /user:<user> /pass`.
pub fn read_generic_credential(target: &str) -> Result<Credential> {
    let mut raw: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `HSTRING` is a NUL-terminated UTF-16 string that outlives the call,
    // and `raw` is a valid place for CredReadW to store its allocation.
    let read = unsafe { CredReadW(&HSTRING::from(target), CRED_TYPE_GENERIC, None, &mut raw) };
    if let Err(error) = read {
        return Err(if error.code() == ERROR_NOT_FOUND.to_hresult() {
            Error::CredentialNotFound(target.to_owned())
        } else {
            Error::Windows {
                call: "CredReadW",
                source: error.into(),
            }
        });
    }

    // Copied out first, freed second, whatever the copy found.
    // SAFETY: CredReadW succeeded, so `raw` points to a valid CREDENTIALW whose
    // user name is NUL-terminated (or null) and whose blob holds
    // `CredentialBlobSize` bytes (or is null). Both are only read before CredFree.
    let (user, password) = unsafe {
        let credential = &*raw;
        let user = if credential.UserName.is_null() {
            None
        } else {
            credential.UserName.to_string().ok()
        };
        let password = if credential.CredentialBlob.is_null() {
            Vec::new()
        } else {
            let bytes = std::slice::from_raw_parts(
                credential.CredentialBlob,
                credential.CredentialBlobSize as usize,
            );
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_le_bytes(pair))
                .collect()
        };
        CredFree(raw as *const _);
        (user, password)
    };

    let invalid = |reason| Error::InvalidCredential {
        target: target.to_owned(),
        reason,
    };
    let user = user
        .filter(|user| !user.is_empty())
        .ok_or_else(|| invalid("it has no user name"))?;
    if password.is_empty() {
        return Err(invalid("it has no password"));
    }
    Ok(Credential::new(user, password))
}
