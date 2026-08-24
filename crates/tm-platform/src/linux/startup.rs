//! XDG autostart entries (~/.config/autostart + /etc/xdg/autostart).
//! Disable = rename `.desktop` → `.desktop.disabled`.

use std::path::{Path, PathBuf};
use tm_core::error::{Result, TmError};
use tm_core::model::{StartupImpact, StartupItem};

fn autostart_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")));
    if let Some(c) = config_home {
        out.push(c.join("autostart"));
    }
    out.push(PathBuf::from("/etc/xdg/autostart"));
    out
}

pub fn list_autostart() -> Vec<StartupItem> {
    let mut items = Vec::new();
    for dir in autostart_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            let enabled = !name.ends_with(".disabled");
            if !(name.ends_with(".desktop") || name.ends_with(".desktop.disabled")) {
                continue;
            }
            let parsed = parse_desktop(&path);
            items.push(StartupItem {
                id: format!("xdg:{}/{}", dir.display(), name),
                name: parsed.name.unwrap_or_else(|| {
                    name.trim_end_matches(".disabled")
                        .trim_end_matches(".desktop")
                        .to_string()
                }),
                command: parsed.exec.unwrap_or_else(|| path.to_string_lossy().to_string()),
                location: format!("autostart ({})", dir.display()),
                publisher: None,
                enabled,
                impact: StartupImpact::Unknown,
            });
        }
    }
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    items
}

struct DesktopFile {
    name: Option<String>,
    exec: Option<String>,
}

fn parse_desktop(path: &Path) -> DesktopFile {
    let mut out = DesktopFile { name: None, exec: None };
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("Name=") {
                out.name.get_or_insert_with(|| v.to_string());
            } else if let Some(v) = line.strip_prefix("Exec=") {
                out.exec = Some(v.to_string());
            }
        }
    }
    out
}

pub fn set_enabled(item_id: &str, enabled: bool) -> Result<()> {
    let Some(path_str) = item_id.strip_prefix("xdg:") else {
        return Err(TmError::platform("startup", "unknown item"));
    };
    let path = PathBuf::from(path_str);
    let target = if enabled {
        path.with_file_name(
            path.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.trim_end_matches(".disabled").to_string())
                .unwrap_or_default(),
        )
    } else {
        PathBuf::from(format!("{}.disabled", path.display()))
    };

    // Never enable a system-level entry into the user dir — rename in place.
    std::fs::rename(&path, &target).map_err(|e| {
        TmError::platform(
            "autostart toggle",
            format!("{} → {}: {}", path.display(), target.display(), e),
        )
    })?;
    tracing::info!(?path, enabled, "autostart toggled");
    Ok(())
}
