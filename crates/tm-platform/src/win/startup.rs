//! Startup applications: registry Run keys + Startup folders,
//! with enable/disable via the StartupApproved convention.

use std::collections::HashMap;
use tm_core::error::{Result, TmError};
use tm_core::model::{StartupImpact, StartupItem};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_BINARY,
    REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

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
        let approved = read_registry_binary(
            loc.hive,
            &format!(r"{APPROVED_KEY}\{}", loc.approved_subkey),
        );
        for (name, command) in values {
            let enabled = approved
                .get(&name)
                .is_none_or(|data| data.first().is_some_and(|b| b & 1 == 0));
            out.push(StartupItem {
                id: format!("reg:{}:{}", loc.label, name),
                name,
                command,
                location: loc.label.to_string(),
                publisher: None,
                enabled,
                // Audit §13: native TM shows "None" for disabled items — a
                // disabled app cannot have been measured at all. Real
                // Low/Medium/High telemetry (CPU ms / disk KB thresholds)
                // arrives with the SRUM-backed startup provider.
                impact: if enabled {
                    StartupImpact::Unknown
                } else {
                    StartupImpact::None
                },
            });
        }
    }

    // Startup folders (user + common). The approval state lives under the
    // FULL StartupApproved\StartupFolder subkey — passing just "StartupFolder"
    // would open a nonexistent key next to Explorer's, silently reporting
    // disabled items as enabled.
    for (scope, dir_path) in [
        (
            FolderScope::User,
            std::env::var("APPDATA")
                .map(|a| format!(r"{a}\Microsoft\Windows\Start Menu\Programs\Startup"))
                .ok(),
        ),
        (
            FolderScope::Common,
            std::env::var("PROGRAMDATA")
                .map(|p| format!(r"{p}\Microsoft\Windows\Start Menu\Programs\StartUp"))
                .ok(),
        ),
    ] {
        let Some(dir_path) = dir_path else { continue };
        let Ok(entries) = std::fs::read_dir(&dir_path) else {
            continue;
        };
        let hive = match scope {
            FolderScope::Common => HKEY_LOCAL_MACHINE,
            FolderScope::User => HKEY_CURRENT_USER,
        };
        let approved = read_registry_binary(hive, &format!(r"{APPROVED_KEY}\StartupFolder"));
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
                .is_none_or(|data| data.first().is_some_and(|b| b & 1 == 0));
            // Stable structured identity: scope is encoded explicitly instead
            // of guessed from path substrings like "ProgramData".
            out.push(StartupItem {
                id: format!("folder:{scope}:{dir_path}\\{file_name}"),
                name: std::path::Path::new(&file_name)
                    .file_stem()
                    .map_or_else(|| file_name.clone(), |s| s.to_string_lossy().to_string()),
                command: e.path().to_string_lossy().to_string(),
                location: startup_folder_label(scope),
                publisher: resolve_publisher(&e.path().to_string_lossy()),
                enabled,
                // Disabled → None; only ENABLED items without measured
                // telemetry are "Not measured" (audit §13).
                impact: if enabled {
                    StartupImpact::Unknown
                } else {
                    StartupImpact::None
                },
            });
        }
    }

    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderScope {
    User,
    Common,
}

impl std::fmt::Display for FolderScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FolderScope::User => "user",
            FolderScope::Common => "common",
        })
    }
}

fn startup_folder_label(scope: FolderScope) -> String {
    match scope {
        FolderScope::User => r"HKCU\...Explorer\StartupApproved\StartupFolder".to_string(),
        FolderScope::Common => r"HKLM\...Explorer\StartupApproved\StartupFolder".to_string(),
    }
}

/// Toggle via the StartupApproved registry mechanism:
/// - disable → write `03 00 00 00 <8-byte FILETIME>`
/// - enable  → delete the approval value (default = enabled)
pub fn set_startup_enabled(item_id: &str, _location: &str, enabled: bool) -> Result<()> {
    if let Some(rest) = item_id.strip_prefix("reg:") {
        return set_reg_startup_enabled(rest, enabled);
    }
    if let Some(rest) = item_id.strip_prefix("folder:") {
        return set_folder_startup_enabled(rest, enabled);
    }
    Err(TmError::platform("startup", "malformed item id"))
}

fn set_reg_startup_enabled(rest: &str, enabled: bool) -> Result<()> {
    // rest = "<label>:<value-name>"; our labels contain no ':'.
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

fn set_folder_startup_enabled(rest: &str, enabled: bool) -> Result<()> {
    // rest = "user|common:<dir>\\<file name>"; the file name is the approval
    // value inside StartupApproved\StartupFolder.
    let Some((scope_str, tail)) = rest.split_once(':') else {
        return Err(TmError::platform("startup", "malformed folder id"));
    };
    let scope = match scope_str {
        "user" => FolderScope::User,
        "common" => FolderScope::Common,
        _ => return Err(TmError::platform("startup", "unknown folder scope")),
    };
    let file_name = tail.rsplit('\\').next().unwrap_or_default();
    if file_name.is_empty() {
        return Err(TmError::platform("startup", "folder id without file name"));
    }
    let hive = match scope {
        FolderScope::Common => HKEY_LOCAL_MACHINE,
        FolderScope::User => HKEY_CURRENT_USER,
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
            return Err(TmError::platform(
                "RegCreateKeyExW(folder)",
                format!("{status:?}"),
            ));
        }
        let name_w = wstr(file_name);
        if enabled {
            // Missing value is fine (already enabled); everything else must
            // surface — silently swallowing access-denied made "enable"
            // appear to succeed when it did nothing.
            let status = RegDeleteValueW(key, PCWSTR::from_raw(name_w.as_ptr()));
            if status.is_err() && status.0 != 2 {
                let _ = RegCloseKey(key);
                return Err(TmError::platform(
                    "RegDeleteValueW(folder)",
                    format!("{status:?}"),
                ));
            }
        } else {
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
                return Err(TmError::platform(
                    "RegSetValueExW(folder)",
                    format!("{status:?}"),
                ));
            }
        }
        let _ = RegCloseKey(key);
    }
    tracing::info!(item = file_name, enabled, "startup folder item toggled");
    Ok(())
}

// ------------------------------------------------------------------ helpers

/// Resolve the display executable behind a startup command and query its
/// version-resource company as the publisher column value. Runs on the
/// startup worker thread; failures simply leave the publisher empty.
fn resolve_publisher(command: &str) -> Option<String> {
    let exe = resolve_command_target(command)?;
    let ver = crate::win::version::query(&exe);
    (!ver[1].is_empty()).then(|| ver[1].clone())
}

/// Best-effort target extraction from common startup command shapes:
/// quoted paths, argument-bearing commands, environment variables and
/// `.lnk` shortcuts (resolved through their link target arguments kept
/// intact). The original command stays untouched for diagnostics.
fn resolve_command_target(command: &str) -> Option<String> {
    let cmd = command.trim();
    let candidate = if let Some(rest) = cmd.strip_prefix('"') {
        rest.split('"').next()?.to_string()
    } else if cmd.to_ascii_lowercase().ends_with(".lnk") {
        cmd.to_string()
    } else {
        cmd.split_whitespace().next()?.to_string()
    };
    if candidate.is_empty() {
        return None;
    }
    // Expand %VAR% forms so version lookup finds real files.
    let expanded = expand_env_vars(&candidate);
    if std::path::Path::new(&expanded).exists() {
        return Some(expanded);
    }
    // A .lnk points at its target elsewhere; try the raw name first.
    if candidate.to_ascii_lowercase().ends_with(".exe") && std::path::Path::new(&candidate).exists()
    {
        return Some(candidate);
    }
    None
}

fn expand_env_vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => out.push_str(&rest[start..start + end + 2]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
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
        if RegOpenKeyExW(
            hive,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
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
                Some(
                    &mut kind as *mut u32 as *mut windows::Win32::System::Registry::REG_VALUE_TYPE,
                ),
                Some(data.as_mut_ptr()),
                Some(&mut actual),
            )
            .is_ok();
            let value = if ok && kind == REG_SZ.0 {
                let wide: Vec<u16> = data[..(actual as usize).min(data.len())]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                String::from_utf16_lossy(&wide)
            } else if ok {
                format!("{actual} bytes")
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
        if RegOpenKeyExW(
            hive,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
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
                Some(
                    &mut kind as *mut u32 as *mut windows::Win32::System::Registry::REG_VALUE_TYPE,
                ),
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
        .map_or(0, |d| d.as_nanos() as u64 / 100)
        // FILETIME epoch offset (1601-01-01)
        + 116_444_736_000_000_000
}
