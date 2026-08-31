//! Per-user logon startup for the TaskMan GUI.
//!
//! This deliberately uses HKCU rather than Task Scheduler: it needs no
//! elevation and never creates a persistent high-privilege launch path.

use tm_core::error::{Result, TmError};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "TaskMan";

pub fn command_for(exe: &std::path::Path, start_minimized: bool) -> String {
    let mut command = format!("\"{}\"", exe.to_string_lossy());
    if start_minimized {
        command.push_str(" --minimized-to-tray");
    }
    command
}

pub fn set_enabled(enabled: bool, start_minimized: bool) -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|error| TmError::platform("current_exe", error.to_string()))?;
    set_enabled_for_exe(&exe, enabled, start_minimized)
}

fn set_enabled_for_exe(exe: &std::path::Path, enabled: bool, start_minimized: bool) -> Result<()> {
    let startup_command = enabled.then(|| command_for(exe, start_minimized));
    let path = wstr(RUN_KEY);
    let name = wstr(VALUE_NAME);
    unsafe {
        let mut key = Default::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
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
                "RegCreateKeyExW(Run)",
                format!("{status:?}"),
            ));
        }
        let result = if enabled {
            let value = wstr(startup_command.as_deref().unwrap_or_default());
            let bytes = std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2);
            let status = RegSetValueExW(
                key,
                PCWSTR::from_raw(name.as_ptr()),
                None,
                REG_SZ,
                Some(bytes),
            );
            if status.is_err() {
                Err(TmError::platform(
                    "RegSetValueExW(Run)",
                    format!("{status:?}"),
                ))
            } else {
                Ok(())
            }
        } else {
            let status = RegDeleteValueW(key, PCWSTR::from_raw(name.as_ptr()));
            if status.is_err() && status.0 != 2 {
                Err(TmError::platform(
                    "RegDeleteValueW(Run)",
                    format!("{status:?}"),
                ))
            } else {
                Ok(())
            }
        };
        let _ = RegCloseKey(key);
        result
    }
}

/// Preserve an existing TaskMan autostart preference while moving its command
/// from a portable/download path to the protected Program Files GUI.
pub fn retarget_if_registered(exe: &std::path::Path) -> Result<()> {
    let path = wstr(RUN_KEY);
    let name = wstr(VALUE_NAME);
    let existing = unsafe {
        let mut key = Default::default();
        let status = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        );
        if status.0 == 2 {
            return Ok(());
        }
        if status.is_err() {
            return Err(TmError::platform(
                "RegOpenKeyExW(Run)",
                format!("{status:?}"),
            ));
        }
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
            return Ok(());
        }
        if first.is_err()
            || kind != REG_SZ
            || !(2..=64 * 1024).contains(&bytes)
            || !bytes.is_multiple_of(2)
        {
            let _ = RegCloseKey(key);
            return Err(TmError::platform(
                "RegQueryValueExW(Run)",
                "existing TaskMan startup value is malformed",
            ));
        }
        let mut data = vec![0u8; bytes as usize];
        let read = RegQueryValueExW(
            key,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut bytes),
        );
        let _ = RegCloseKey(key);
        if read.is_err()
            || kind != REG_SZ
            || bytes < 2
            || !bytes.is_multiple_of(2)
            || bytes as usize > data.len()
        {
            return Err(TmError::platform(
                "RegQueryValueExW(Run)",
                "could not read the existing TaskMan startup value",
            ));
        }
        let units = data[..bytes as usize]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|unit| *unit != 0)
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&units)
    };
    let current = std::env::current_exe()
        .map_err(|error| TmError::platform("current_exe", error.to_string()))?;
    let start_minimized = registered_start_minimized(&existing, &current, exe)?;
    set_enabled_for_exe(exe, true, start_minimized)
}

fn registered_start_minimized(
    existing: &str,
    current: &std::path::Path,
    installed: &std::path::Path,
) -> Result<bool> {
    let current_visible = command_for(current, false);
    let current_hidden = command_for(current, true);
    let installed_visible = command_for(installed, false);
    let installed_hidden = command_for(installed, true);
    if existing.eq_ignore_ascii_case(&current_hidden)
        || existing.eq_ignore_ascii_case(&installed_hidden)
    {
        Ok(true)
    } else if existing.eq_ignore_ascii_case(&current_visible)
        || existing.eq_ignore_ascii_case(&installed_visible)
    {
        Ok(false)
    } else {
        Err(TmError::platform(
            "retarget TaskMan autostart",
            "the existing Run value is not owned by this TaskMan executable",
        ))
    }
}

fn wstr(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_command_quotes_path_and_optionally_hides() {
        let exe = std::path::Path::new(r"C:\Program Files\TaskMan\taskman.exe");
        assert_eq!(
            command_for(exe, false),
            r#""C:\Program Files\TaskMan\taskman.exe""#
        );
        assert_eq!(
            command_for(exe, true),
            r#""C:\Program Files\TaskMan\taskman.exe" --minimized-to-tray"#
        );
    }

    #[test]
    fn retarget_only_accepts_owned_commands() {
        let current = std::path::Path::new(r"C:\Users\me\Downloads\taskman.exe");
        let installed = std::path::Path::new(r"C:\Program Files\TaskMan\taskman.exe");
        assert!(
            !registered_start_minimized(&command_for(current, false), current, installed).unwrap()
        );
        assert!(
            registered_start_minimized(&command_for(installed, true), current, installed).unwrap()
        );
        assert!(
            registered_start_minimized(
                r#""C:\Other\taskman.exe" --minimized-to-tray"#,
                current,
                installed
            )
            .is_err()
        );
    }
}
