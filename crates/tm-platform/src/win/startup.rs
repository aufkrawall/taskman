//! Startup applications: registry Run keys + Startup folders,
//! with enable/disable via the StartupApproved convention.

use std::collections::HashMap;
use tm_core::error::{Result, TmError};
use tm_core::model::{StartupImpact, StartupItem};
use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE,
    REG_BINARY, REG_OPTION_NON_VOLATILE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

struct RunLocation {
    hive: HKEY,
    subkey: &'static str,
    approved_subkey: &'static str,
    label: &'static str,
}

const LOCATIONS: &[RunLocation] = &[
    RunLocation {
        hive: HKEY_CURRENT_USER,
        subkey: RUN_KEY,
        approved_subkey: "Run",
        label: r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
    },
    RunLocation {
        hive: HKEY_LOCAL_MACHINE,
        subkey: RUN_KEY,
        approved_subkey: "Run",
        label: r"HKLM\Software\Microsoft\Windows\CurrentVersion\Run",
    },
    RunLocation {
        hive: HKEY_LOCAL_MACHINE,
        subkey: r"Software\Wow6432Node\Microsoft\Windows\CurrentVersion\Run",
        approved_subkey: "Run32",
        label: r"HKLM\Software\Wow6432Node\...\Run",
    },
];

pub fn list_startup() -> Vec<StartupItem> {
    let mut out = Vec::new();

    // Registry run keys.
    for loc in LOCATIONS {
        let values = read_registry_values(loc.hive, loc.subkey);
        let approved = read_registry_binary(loc.hive, &format!(r"{APPROVED_KEY}\{}", loc.approved_subkey));
        for (name, command) in values {
            let enabled = approved
                .get(&name)
                .map(|data| data.first().is_some_and(|b| b & 1 == 0))
                .unwrap_or(true);
            out.push(StartupItem {
                id: format!("reg:{}:{}", loc.label, name),
                name,
                command,
                location: loc.label.to_string(),
                publisher: None,
                enabled,
                impact: StartupImpact::Unknown,
            });
        }
    }

    // Startup folders (user + common).
    for (dir_path, is_common) in [
        (
            std::env::var("APPDATA")
                .map(|a| format!(r"{a}\Microsoft\Windows\Start Menu\Programs\Startup"))
                .ok(),
            false,
        ),
        (
            std::env::var("PROGRAMDATA")
                .map(|p| format!(r"{p}\Microsoft\Windows\Start Menu\Programs\StartUp"))
                .ok(),
            true,
        ),
    ] {
        let Some(dir_path) = dir_path else { continue };
        let Ok(entries) = std::fs::read_dir(&dir_path) else { continue };
        let folder_approved_key = if is_common {
            format!(r"HKLM\{APPROVED_KEY}\StartupFolder")
        } else {
            format!(r"HKCU\{APPROVED_KEY}\StartupFolder")
        };
        let hive = if is_common { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER };
        let approved = read_registry_binary(hive, "StartupFolder");
        for e in entries.flatten() {
            let file_name = e.file_name().to_string_lossy().to_string();
            if !file_name.to_ascii_lowercase().ends_with(".lnk")
                && !file_name.to_ascii_lowercase().ends_with(".url")
                && !file_name.to_ascii_lowercase().ends_with(".exe")
            {
                continue;
            }
            let enabled = approved
                .get(&file_name)
                .map(|data| data.first().is_some_and(|b| b & 1 == 0))
                .unwrap_or(true);
            out.push(StartupItem {
                id: format!("folder:{dir_path}:{file_name}"),
                name: std::path::Path::new(&file_name)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| file_name.clone()),
                command: e.path().to_string_lossy().to_string(),
                location: folder_approved_key.clone(),
                publisher: None,
                enabled,
                impact: StartupImpact::Unknown,
            });
        }
    }

    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Toggle via the StartupApproved registry mechanism:
/// - disable → write `03 00 00 00 <8-byte FILETIME>`
/// - enable  → delete the approval value (default = enabled)
pub fn set_startup_enabled(item_id: &str, _location: &str, enabled: bool) -> Result<()> {
    let Some(rest) = item_id.strip_prefix("reg:") else {
        return set_folder_startup_enabled(item_id, enabled);
    };
    // rest = "<label>:<value-name>"; labels contain ':'? No — our labels don't.
    let Some((label, value_name)) = rest.split_once(':') else {
        return Err(TmError::platform("startup", "malformed item id"));
    };

    let loc = LOCATIONS
        .iter()
        .find(|l| l.label == label)
        .ok_or_else(|| TmError::platform("startup", "unknown location"))?;

    unsafe {
        let key_path_wide = wstr(&format!(r"{APPROVED_KEY}\{}", loc.approved_subkey));
        let mut key = HKEY::default();
        let status = RegCreateKeyExW(
            loc.hive,
            PCWSTR::from_raw(key_path_wide.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_READ,
            None,
            &mut key,
            None,
        );
        if status.is_err() {
            return Err(TmError::platform("RegCreateKeyExW", format!("{status:?}")));
        }
        let name_w = wstr(value_name);
        if enabled {
            let status = RegDeleteValueW(key, PCWSTR::from_raw(name_w.as_ptr()));
            // Missing value is fine.
            if status.is_err() && status.0 != 2 {
                let _ = RegCloseKey(key);
                return Err(TmError::platform("RegDeleteValueW", format!("{status:?}")));
            }
        } else {
            // FILETIME of now.
            let now_ft = systemtime_to_filetime(std::time::SystemTime::now());
            let mut data: [u8; 12] = [0; 12];
            data[0] = 0x03;
            data[4..12].copy_from_slice(&now_ft.to_le_bytes());
            let status = RegSetValueExW(
                key,
                PCWSTR::from_raw(name_w.as_ptr()),
                None,
                REG_BINARY,
                Some(&data),
            );
            if status.is_err() {
                let _ = RegCloseKey(key);
                return Err(TmError::platform("RegSetValueExW", format!("{status:?}")));
            }
        }
        let _ = RegCloseKey(key);
    }
    tracing::info!(item = value_name, enabled, "startup item toggled");
    Ok(())
}

fn set_folder_startup_enabled(_item_id: &str, enabled: bool) -> Result<()> {
    // Folder items use the StartupFolder approvals key with the *file name*.
    // The caller passes the full id; extract trailing component.
    let file_name = _item_id.rsplit(':').next().unwrap_or_default();
    let (hive, _label) = if _item_id.contains(r"HKLM") || _item_id.starts_with("folder:") && _item_id.contains("ProgramData") {
        (HKEY_LOCAL_MACHINE, "common")
    } else {
        (HKEY_CURRENT_USER, "user")
    };
    unsafe {
        let mut key = HKEY::default();
        let full = format!(r"{APPROVED_KEY}\StartupFolder");
        let path_w = wstr(&full);
        let status = RegCreateKeyExW(
            hive,
            PCWSTR::from_raw(path_w.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_READ,
            None,
            &mut key,
            None,
        );
        if status.is_err() {
            return Err(TmError::platform("RegCreateKeyExW(folder)", format!("{status:?}")));
        }
        let name_w = wstr(file_name);
        if enabled {
            let _ = RegDeleteValueW(key, PCWSTR::from_raw(name_w.as_ptr()));
        } else {
            let now_ft = systemtime_to_filetime(std::time::SystemTime::now());
            let mut data: [u8; 12] = [0; 12];
            data[0] = 0x03;
            data[4..12].copy_from_slice(&now_ft.to_le_bytes());
            let status =
                RegSetValueExW(key, PCWSTR::from_raw(name_w.as_ptr()), None, REG_BINARY, Some(&data));
            if status.is_err() {
                let _ = RegCloseKey(key);
                return Err(TmError::platform("RegSetValueExW(folder)", format!("{status:?}")));
            }
        }
        let _ = RegCloseKey(key);
    }
    tracing::info!(item = file_name, enabled, "startup folder item toggled");
    Ok(())
}

// ------------------------------------------------------------------ registry helpers

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn read_registry_values(hive: HKEY, subkey: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let path = wstr(subkey);
        let mut key = HKEY::default();
        if RegOpenKeyExW(hive, PCWSTR::from_raw(path.as_ptr()), None, KEY_READ, &mut key).is_err() {
            return out;
        }
        let mut index: u32 = 0;
        loop {
            let mut name_buf = [0u16; 512];
            let mut name_len: u32 = name_buf.len() as u32;
            let mut kind: u32 = 0;
            let mut data_len: u32 = 0;
            let status = RegEnumValueW(
                key,
                index,
                Some(windows::core::PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut kind),
                None,
                Some(&mut data_len),
            );
            if status.is_err() {
                break;
            }
            let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
            let mut data = vec![0u8; data_len.max(1) as usize];
            let mut actual = data_len;
            let name_query = wstr(&name);
            let ok = RegQueryValueExW(
                key,
                PCWSTR::from_raw(name_query.as_ptr()),
                None,
                Some(&mut kind as *mut u32 as *mut windows::Win32::System::Registry::REG_VALUE_TYPE),
                Some(data.as_mut_ptr()),
                Some(&mut actual),
            )
            .is_ok();
            let value = if ok && kind == REG_SZ.0 {
                let wide: Vec<u16> = data[..(actual as usize).min(data.len())]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                String::from_utf16_lossy(&wide)
            } else if ok {
                format!("{} bytes", actual)
            } else {
                String::new()
            };
            out.push((name, value));
            index += 1;
            if index > 200 {
                break;
            }
        }
        let _ = RegCloseKey(key);
    }
    out
}

fn read_registry_binary(hive: HKEY, subkey: &str) -> HashMap<String, Vec<u8>> {
    let mut out = HashMap::new();
    unsafe {
        let path = wstr(subkey);
        let mut key = HKEY::default();
        if RegOpenKeyExW(hive, PCWSTR::from_raw(path.as_ptr()), None, KEY_READ, &mut key).is_err() {
            return out;
        }
        let mut index: u32 = 0;
        loop {
            let mut name_buf = [0u16; 512];
            let mut name_len: u32 = name_buf.len() as u32;
            let mut kind: u32 = 0;
            let mut data_len: u32 = 0;
            let status = RegEnumValueW(
                key,
                index,
                Some(windows::core::PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut kind),
                None,
                Some(&mut data_len),
            );
            if status.is_err() {
                break;
            }
            let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
            let mut data = vec![0u8; data_len.max(1) as usize];
            let mut actual = data_len;
            let name_query = wstr(&name);
            if RegQueryValueExW(
                key,
                PCWSTR::from_raw(name_query.as_ptr()),
                None,
                Some(&mut kind as *mut u32 as *mut windows::Win32::System::Registry::REG_VALUE_TYPE),
                Some(data.as_mut_ptr()),
                Some(&mut actual),
            )
            .is_ok()
            {
                out.insert(name, data[..(actual as usize).min(data.len())].to_vec());
            }
            index += 1;
            if index > 200 {
                break;
            }
        }
        let _ = RegCloseKey(key);
    }
    out
}

fn systemtime_to_filetime(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 / 100)
        .unwrap_or(0)
        // FILETIME epoch offset (1601-01-01)
        + 116_444_736_000_000_000
}
