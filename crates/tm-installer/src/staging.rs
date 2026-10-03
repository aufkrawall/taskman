//! Elevated helpers must never be extracted into user-writable staging.

use std::path::{Path, PathBuf};
use tm_core::error::{Result, TmError};

pub struct Staging(PathBuf);

impl Staging {
    pub fn create() -> Result<Self> {
        use windows::Win32::System::Com::CoCreateGuid;

        let parent = crate::win::install_dir()?
            .parent()
            .expect("Program Files parent")
            .to_path_buf();
        let guid = unsafe { CoCreateGuid() }
            .map_err(|e| TmError::platform("staging identity", e.to_string()))?;
        let path = parent.join(format!(".TaskMan-setup-{guid:?}"));
        crate::win::create_protected_dir(&path)?;
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
