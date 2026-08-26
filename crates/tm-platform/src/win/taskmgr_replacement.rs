//! Optional Windows Task Manager replacement through Image File Execution
//! Options. The registry is the source of truth; config.ini never mirrors it.

use tm_core::error::{Result, TmError};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

const IFEO_TASKMGR: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe";
const VALUE_DEBUGGER: &str = "Debugger";
pub const OWNER_MARKER: &str = "--taskmgr-replacement-launch";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Disabled,
    Enabled,
    /// Taskman owns the value, but it points at another executable location.
    Stale(String),
    /// Another product owns the IFEO debugger value. Never overwrite it.
    Conflict(String),
}

pub fn state() -> State {
    let Some(value) = read_debugger() else {
        return State::Disabled;
    };
    if !value.contains(OWNER_MARKER) {
        return State::Conflict(value);
    }
    match own_command() {
        Ok(own) if normalize_command(&value) == normalize_command(&own) => State::Enabled,
        _ => State::Stale(value),
    }
}

/// Enable/disable directly. The caller must already be elevated when a write
/// is necessary. Existing third-party replacements are never overwritten or
/// deleted.
pub fn set_direct(enabled: bool) -> Result<()> {
    let current = state();
    if enabled {
        if let State::Conflict(value) = current {
            return Err(TmError::platform(
                "Task Manager replacement",
                format!("another debugger is already registered: {value}"),
            ));
        }
        write_debugger(&own_command()?)
    } else {
        match current {
            State::Disabled => Ok(()),
            State::Enabled | State::Stale(_) => delete_debugger(),
            State::Conflict(value) => Err(TmError::platform(
                "Task Manager replacement",
                format!("refusing to remove another debugger: {value}"),
            )),
        }
    }
}

pub fn own_command() -> Result<String> {
    let exe = std::env::current_exe()
        .map_err(|e| TmError::platform("current_exe", e.to_string()))?;
    Ok(format!("\"{}\" {OWNER_MARKER}", exe.to_string_lossy()))
}

fn normalize_command(s: &str) -> String {
    s.trim().replace('/', "\\").to_ascii_lowercase()
}

fn read_debugger() -> Option<String> {
    unsafe {
        let mut key = Default::default();
        let path = wstr(IFEO_TASKMGR);
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return None;
        }
        let name = wstr(VALUE_DEBUGGER);
        let mut kind = REG_SZ;
        let mut bytes = 0u32;
        let first = RegQueryValueExW(
            key,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut bytes),
        );
        if first.is_err() || kind != REG_SZ || bytes < 2 {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut data = vec![0u8; bytes as usize];
        let ok = RegQueryValueExW(
            key,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut bytes),
        )
        .is_ok();
        let _ = RegCloseKey(key);
        if !ok {
            return None;
        }
        let wide: Vec<u16> = data[..bytes as usize]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .take_while(|&c| c != 0)
            .collect();
        Some(String::from_utf16_lossy(&wide))
    }
}

fn write_debugger(value: &str) -> Result<()> {
    unsafe {
        let path = wstr(IFEO_TASKMGR);
        let mut key = Default::default();
        let status = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut key,
            None,
        );
        if status.is_err() {
            return Err(TmError::platform("RegCreateKeyExW(IFEO)", format!("{status:?}")));
        }
        let name = wstr(VALUE_DEBUGGER);
        let wide = wstr(value);
        let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
        let status = RegSetValueExW(
            key,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            REG_SZ,
            Some(bytes),
        );
        let _ = RegCloseKey(key);
        if status.is_err() {
            Err(TmError::platform("RegSetValueExW(IFEO)", format!("{status:?}")))
        } else {
            Ok(())
        }
    }
}

fn delete_debugger() -> Result<()> {
    unsafe {
        let path = wstr(IFEO_TASKMGR);
        let mut key = Default::default();
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_READ | KEY_WRITE,
            &mut key,
        )
        .is_err()
        {
            return Ok(());
        }
        let name = wstr(VALUE_DEBUGGER);
        let status = RegDeleteValueW(key, PCWSTR::from_raw(name.as_ptr()));
        let _ = RegCloseKey(key);
        // ERROR_FILE_NOT_FOUND: already disabled.
        if status.is_err() && status.0 != 2 {
            Err(TmError::platform("RegDeleteValueW(IFEO)", format!("{status:?}")))
        } else {
            Ok(())
        }
    }
}

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_marker_survives_paths_with_spaces() {
        let own = "\"C:\\Program Files\\Taskman\\taskman.exe\" --taskmgr-replacement-launch";
        assert!(own.contains(OWNER_MARKER));
        assert_eq!(normalize_command(own), normalize_command(own));
    }

    #[test]
    fn unrelated_debugger_is_not_owned() {
        assert!(!"C:\\Sysinternals\\procexp64.exe".contains(OWNER_MARKER));
    }
}
