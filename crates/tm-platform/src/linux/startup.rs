//! XDG autostart entries. System files are never renamed or mutated:
//! disabling creates/updates the same-named user entry with `Hidden=true`,
//! which is the precedence mechanism defined by the XDG autostart spec.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tm_core::error::{Result, TmError};
use tm_core::model::{StartupImpact, StartupItem};

const TASKMAN_OVERRIDE_KEY: &str = "X-Taskman-Override=true";

fn config_home() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
}

/// Low-to-high precedence so later entries naturally override earlier ones.
fn autostart_dirs_low_to_high() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::env::var("XDG_CONFIG_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/etc/xdg".into())
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(s).join("autostart"))
        .collect();
    out.reverse();
    if let Some(home) = config_home() {
        out.push(home.join("autostart"));
    }
    out
}

#[derive(Debug, Clone, Default)]
struct DesktopFile {
    name: Option<String>,
    exec: Option<String>,
    hidden: Option<bool>,
    source: PathBuf,
}

pub fn list_autostart() -> Vec<StartupItem> {
    // Merge same-named desktop files according to XDG precedence. A minimal
    // user Hidden=true override keeps lower-precedence Name/Exec metadata for
    // display purposes, while its Hidden flag still wins semantically.
    let mut merged: BTreeMap<String, DesktopFile> = BTreeMap::new();
    for dir in autostart_dirs_low_to_high() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !file_name.ends_with(".desktop") {
                continue;
            }
            let parsed = parse_desktop(&path);
            let slot = merged.entry(file_name.to_string()).or_default();
            if parsed.name.is_some() {
                slot.name = parsed.name;
            }
            if parsed.exec.is_some() {
                slot.exec = parsed.exec;
            }
            if parsed.hidden.is_some() {
                slot.hidden = parsed.hidden;
            }
            slot.source = path;
        }
    }

    let mut items: Vec<StartupItem> = merged
        .into_iter()
        .map(|(file_name, parsed)| {
            let enabled = parsed.hidden != Some(true);
            StartupItem {
                // Stable identity is the XDG desktop-file id (basename), not
                // the currently winning physical path.
                id: format!("xdg:{file_name}"),
                name: parsed
                    .name
                    .unwrap_or_else(|| file_name.trim_end_matches(".desktop").to_string()),
                command: parsed
                    .exec
                    .unwrap_or_else(|| parsed.source.to_string_lossy().to_string()),
                location: format!("autostart ({})", parsed.source.display()),
                publisher: None,
                enabled,
                impact: if enabled {
                    StartupImpact::Unknown
                } else {
                    StartupImpact::None
                },
            }
        })
        .collect();
    items.sort_by_key(|a| a.name.to_lowercase());
    items
}

fn parse_desktop(path: &Path) -> DesktopFile {
    let mut out = DesktopFile {
        source: path.to_path_buf(),
        ..Default::default()
    };
    if let Ok(text) = std::fs::read_to_string(path) {
        let mut in_entry = false;
        for raw in text.lines() {
            let line = raw.trim();
            if line.starts_with('[') {
                in_entry = line == "[Desktop Entry]";
                continue;
            }
            if !in_entry || line.starts_with('#') {
                continue;
            }
            if let Some(v) = line.strip_prefix("Name=") {
                out.name.get_or_insert_with(|| v.to_string());
            } else if let Some(v) = line.strip_prefix("Exec=") {
                out.exec = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("Hidden=") {
                out.hidden = parse_bool(v);
            }
        }
    }
    out
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

fn is_taskman_override(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .is_some_and(|text| text.lines().any(|line| line.trim() == TASKMAN_OVERRIDE_KEY))
}

pub fn set_enabled(item_id: &str, enabled: bool) -> Result<()> {
    let Some(file_name) = item_id.strip_prefix("xdg:") else {
        return Err(TmError::platform("startup", "unknown item"));
    };
    if file_name.is_empty() || file_name.contains('/') || !file_name.ends_with(".desktop") {
        return Err(TmError::platform("startup", "invalid desktop-file id"));
    }
    let home = config_home().ok_or_else(|| TmError::platform("startup", "no XDG config home"))?;
    let dir = home.join("autostart");
    std::fs::create_dir_all(&dir)
        .map_err(|e| TmError::platform("create autostart dir", e.to_string()))?;
    let user_path = dir.join(file_name);

    if user_path.exists() {
        if enabled && is_taskman_override(&user_path) {
            // Taskman created this file only to shadow a lower-precedence
            // system entry. Removing it is the only correct way to re-enable
            // that original entry; Hidden=false would keep shadowing it with
            // an override that has no Exec line.
            std::fs::remove_file(&user_path)
                .map_err(|e| TmError::platform("remove autostart override", e.to_string()))?;
        } else {
            // User-owned desktop files stay user-owned; modify only Hidden.
            set_hidden_in_file(&user_path, !enabled)?;
        }
    } else if !enabled {
        // Same filename + Hidden=true is the standards-compliant user override
        // for a system-wide autostart entry. Mark it so Taskman can safely
        // remove only its own minimal override when re-enabling.
        let stem = file_name.trim_end_matches(".desktop");
        let text = format!(
            "[Desktop Entry]\nType=Application\nName={stem}\nHidden=true\n{TASKMAN_OVERRIDE_KEY}\n"
        );
        std::fs::write(&user_path, text)
            .map_err(|e| TmError::platform("write autostart override", e.to_string()))?;
    } else {
        // No user override means the lower-precedence system file is enabled.
        return Ok(());
    }

    tracing::info!(?user_path, enabled, "XDG autostart item toggled");
    Ok(())
}

fn set_hidden_in_file(path: &Path, hidden: bool) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| TmError::platform("read autostart entry", e.to_string()))?;
    let mut out = String::with_capacity(text.len() + 24);
    let mut in_entry = false;
    let mut wrote = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            if in_entry && !wrote {
                out.push_str(&format!(
                    "Hidden={}\n",
                    if hidden { "true" } else { "false" }
                ));
                wrote = true;
            }
            in_entry = line == "[Desktop Entry]";
        }
        if in_entry && line.starts_with("Hidden=") {
            out.push_str(&format!(
                "Hidden={}\n",
                if hidden { "true" } else { "false" }
            ));
            wrote = true;
        } else {
            out.push_str(raw);
            out.push('\n');
        }
    }
    if in_entry && !wrote {
        out.push_str(&format!(
            "Hidden={}\n",
            if hidden { "true" } else { "false" }
        ));
    }
    std::fs::write(path, out).map_err(|e| TmError::platform("write autostart entry", e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hidden_inside_desktop_entry_only() {
        let dir = std::env::temp_dir().join(format!("taskman-xdg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nName=Test\nExec=test --arg\nHidden=true\n[X-Other]\nHidden=false\n",
        )
        .unwrap();
        let d = parse_desktop(&path);
        assert_eq!(d.name.as_deref(), Some("Test"));
        assert_eq!(d.exec.as_deref(), Some("test --arg"));
        assert_eq!(d.hidden, Some(true));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hidden_update_preserves_user_desktop_file() {
        let dir = std::env::temp_dir().join(format!("taskman-xdg-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.desktop");
        std::fs::write(&path, "[Desktop Entry]\nName=Test\nExec=test\n").unwrap();
        set_hidden_in_file(&path, true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("Exec=test"));
        assert!(text.contains("Hidden=true"));
        assert!(!is_taskman_override(&path));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn owned_minimal_override_is_identifiable() {
        let dir = std::env::temp_dir().join(format!("taskman-xdg-own-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.desktop");
        std::fs::write(
            &path,
            format!("[Desktop Entry]\nHidden=true\n{TASKMAN_OVERRIDE_KEY}\n"),
        )
        .unwrap();
        assert!(is_taskman_override(&path));
        let _ = std::fs::remove_dir_all(dir);
    }
}
