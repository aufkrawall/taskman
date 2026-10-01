//! Elevated helpers must never be extracted into user-writable staging.

use std::path::{Path, PathBuf};
use tm_core::error::{Result, TmError};

pub struct Staging(PathBuf);

impl Staging {
    pub fn create() -> Result<Self> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows::Win32::Storage::FileSystem::CreateDirectoryW;
        use windows::Win32::System::Com::CoCreateGuid;
        use windows::core::PCWSTR;

        let parent = crate::win::install_dir()?
            .parent()
            .expect("Program Files parent")
            .to_path_buf();
        let guid = unsafe { CoCreateGuid() }
            .map_err(|e| TmError::platform("staging identity", e.to_string()))?;
        let path = parent.join(format!(".TaskMan-setup-{guid:?}"));
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let acl: Vec<u16> = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(acl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .map_err(|e| TmError::platform("staging ACL", e.to_string()))?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            };
            let result = CreateDirectoryW(PCWSTR(wide.as_ptr()), Some(&attributes));
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            result.map_err(|e| TmError::platform("create protected staging", e.to_string()))?;
        }
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            crate::win::append_setup_log(&format!("protected staging cleanup failed: {error}"));
        }
    }
}
