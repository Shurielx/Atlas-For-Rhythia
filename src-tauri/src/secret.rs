//! Secrets in the system's own store: Windows Credential Manager (a generic credential of this user on
//! this computer). Atlas keeps nothing secret in its files. Elsewhere there is no store yet, so saving
//! fails and the caller keeps the value in memory only.

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    /// The store refuses blobs above 5 × 512 bytes.
    const MAX_BLOB: usize = 2560;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn save(name: &str, value: &str) -> Result<(), String> {
        if value.len() > MAX_BLOB {
            return Err("The value is too long for the system's password store.".into());
        }
        let mut target = wide(name);
        let mut user = wide("Atlas for Rhythia");
        let mut blob = value.as_bytes().to_vec();
        let cred = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: std::ptr::null_mut(),
            LastWritten: FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 },
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: user.as_mut_ptr(),
        };
        // SAFETY: every pointer refers to a buffer that lives until the call returns.
        let ok = unsafe { CredWriteW(&cred, 0) };
        blob.fill(0);
        if ok == 0 {
            Err("The system's password store refused the value.".into())
        } else {
            Ok(())
        }
    }

    pub fn load(name: &str) -> Option<String> {
        let target = wide(name);
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: CredReadW fills `found` with memory the system owns, freed below with CredFree.
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) } == 0 || found.is_null() {
            return None;
        }
        let value = unsafe {
            let c = &*found;
            let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
            let value = String::from_utf8(bytes.to_vec()).ok();
            CredFree(found as *const core::ffi::c_void);
            value
        };
        value
    }

    pub fn delete(name: &str) {
        let target = wide(name);
        // SAFETY: a plain call with a nul-terminated name.
        unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn save(_: &str, _: &str) -> Result<(), String> {
        Err("No password store on this system yet.".into())
    }
    pub fn load(_: &str) -> Option<String> {
        None
    }
    pub fn delete(_: &str) {}
}

pub use imp::{delete, load, save};

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn saves_reads_and_deletes() {
        let name = "AtlasForRhythia/test-secret";
        super::save(name, "value-123").unwrap();
        assert_eq!(super::load(name).as_deref(), Some("value-123"));
        super::delete(name);
        assert_eq!(super::load(name), None);
        assert!(super::save(name, &"x".repeat(3000)).is_err());
    }
}
