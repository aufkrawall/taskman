//! Optional Windows Task Manager replacement through Image File Execution
//! Options. The registry is the source of truth; config.ini never mirrors it.

use std::path::Path;
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

enum DebuggerValue {
    Absent,
    Text(String),
    Invalid,
}

pub fn state_for_exe(exe: &Path) -> State {
    let value = match read_debugger() {
        DebuggerValue::Absent => return State::Disabled,
        DebuggerValue::Text(value) => value,
        DebuggerValue::Invalid => {
            return State::Conflict(
                "the existing debugger registration is unreadable or malformed".into(),
            );
        }
    };
    if !is_owned_command(&value) {
        return State::Conflict(value);
    }
    let own = own_command_for(exe);
    if normalize_command(&value) == normalize_command(&own) {
        State::Enabled
    } else {
        State::Stale(value)
    }
}

/// Enable/disable directly. The caller must already be elevated when a write
/// is necessary. Existing third-party replacements are never overwritten or
/// deleted.
pub fn set_direct(enabled: bool) -> Result<()> {
    let exe =
        std::env::current_exe().map_err(|e| TmError::platform("current_exe", e.to_string()))?;
    set_direct_for_exe(enabled, &exe)
}

pub fn set_direct_for_exe(enabled: bool, exe: &Path) -> Result<()> {
    let current = state_for_exe(exe);
    if enabled {
        if let State::Conflict(value) = current {
            return Err(TmError::platform(
                "Task Manager replacement",
                format!("another debugger is already registered: {value}"),
            ));
        }
        write_debugger(&own_command_for(exe))
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

fn own_command_for(exe: &Path) -> String {
    format!("\"{}\" {OWNER_MARKER}", exe.to_string_lossy())
}

fn is_owned_command(value: &str) -> bool {
    let Some(quoted_path) = value.trim().strip_suffix(OWNER_MARKER).map(str::trim_end) else {
        return false;
    };
    let Some(path) = quoted_path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
    else {
        return false;
    };
    !path.is_empty() && !path.contains('"')
}

fn normalize_command(s: &str) -> String {
    s.trim().replace('/', "\\").to_lowercase()
}

fn read_debugger() -> DebuggerValue {
    unsafe {
        let mut key = Default::default();
        let path = wstr(IFEO_TASKMGR);
        let opened = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        );
        if opened.0 == 2 {
            return DebuggerValue::Absent;
        }
        if opened.is_err() {
            return DebuggerValue::Invalid;
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
        if first.0 == 2 {
            let _ = RegCloseKey(key);
            return DebuggerValue::Absent;
        }
        if first.is_err()
            || kind != REG_SZ
            || !(2..=64 * 1024).contains(&bytes)
            || !bytes.is_multiple_of(2)
        {
            let _ = RegCloseKey(key);
            return DebuggerValue::Invalid;
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
        if !ok
            || kind != REG_SZ
            || bytes < 2
            || !bytes.is_multiple_of(2)
            || bytes as usize > data.len()
        {
            return DebuggerValue::Invalid;
        }
        let wide: Vec<u16> = data[..bytes as usize]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .take_while(|&c| c != 0)
            .collect();
        DebuggerValue::Text(String::from_utf16_lossy(&wide))
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
            return Err(TmError::platform(
                "RegCreateKeyExW(IFEO)",
                format!("{status:?}"),
            ));
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
            Err(TmError::platform(
                "RegSetValueExW(IFEO)",
                format!("{status:?}"),
            ))
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
            Err(TmError::platform(
                "RegDeleteValueW(IFEO)",
                format!("{status:?}"),
            ))
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
        let path = Path::new(r"C:\Program Files\Taskman\taskman.exe");
        let own = own_command_for(path);
        assert_eq!(
            own,
            "\"C:\\Program Files\\Taskman\\taskman.exe\" --taskmgr-replacement-launch"
        );
        assert!(own.contains(OWNER_MARKER));
        assert_eq!(normalize_command(&own), normalize_command(&own));
    }

    #[test]
    fn unrelated_debugger_is_not_owned() {
        assert!(!is_owned_command(r"C:\Sysinternals\procexp64.exe"));
        assert!(!is_owned_command(&format!(
            r#""C:\Sysinternals\procexp64.exe" {OWNER_MARKER} --extra"#
        )));
        assert!(is_owned_command(&own_command_for(Path::new(
            r"C:\Program Files\TaskMan\taskman.exe"
        ))));
    }
}
