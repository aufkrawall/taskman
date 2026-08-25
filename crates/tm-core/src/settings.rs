//! User settings with atomic JSON persistence.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateSpeed {
    /// 500 ms
    High,
    /// 1000 ms (Task Manager default)
    Normal,
    /// 4000 ms
    Low,
    Paused,
}

impl UpdateSpeed {
    pub fn interval(self) -> std::time::Duration {
        match self {
            UpdateSpeed::High => std::time::Duration::from_millis(500),
            UpdateSpeed::Normal => std::time::Duration::from_millis(1000),
            UpdateSpeed::Low => std::time::Duration::from_millis(4000),
            UpdateSpeed::Paused => std::time::Duration::from_secs(3600),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeMode,
    pub update_speed: UpdateSpeed,
    pub always_on_top: bool,
    /// Length of the visible graph window in seconds.
    pub graph_seconds: u32,
    /// UI scale multiplier on top of OS DPI.
    pub ui_zoom: f32,
    /// Remember window size/position between runs.
    pub remember_window: bool,
    pub window_size: [f32; 2],
    /// Show per-process network column even when the platform can't measure it.
    pub show_net_column_anyway: bool,
    /// Navigation rail collapsed to icons only (hamburger toggle).
    pub sidebar_collapsed: bool,
    /// UI language; `System` follows the OS display language.
    pub language: crate::i18n::LangChoice,
    /// User-resized column widths per table (`table id -> widths`).
    pub col_widths: BTreeMap<String, Vec<f32>>,
    /// Width of the Performance tab's left card column.
    pub perf_card_width: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            update_speed: UpdateSpeed::Normal,
            always_on_top: false,
            graph_seconds: 60,
            ui_zoom: 1.0,
            remember_window: true,
            window_size: [1100.0, 720.0],
            show_net_column_anyway: true,
            sidebar_collapsed: false,
            language: Default::default(),
            col_widths: BTreeMap::new(),
            perf_card_width: 252.0,
        }
    }
}

fn default_path() -> Option<PathBuf> {
    dirs::config_local_dir().map(|d| d.join("taskman").join("settings.json"))
}

impl Settings {
    /// Load settings from `path` (or the platform default). A missing file
    /// yields defaults; a corrupt file is renamed aside and defaults returned.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => {
                // Tolerate a UTF-8 BOM (e.g. files edited with PowerShell).
                let text = String::from_utf8_lossy(&bytes);
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                match serde_json::from_str(text) {
                    Ok(s) => {
                        tracing::debug!(path = %path.display(), "settings loaded");
                        s
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "settings corrupt; using defaults");
                        let bak = path.with_extension("json.bad");
                        let _ = std::fs::rename(path, bak);
                        Self::default()
                    }
                }
            }
            Err(_) => {
                tracing::debug!(path = %path.display(), "no settings file yet; using defaults");
                Self::default()
            }
        }
    }

    pub fn load() -> Self {
        match default_path() {
            Some(p) => Self::load_from(&p),
            None => Self::default(),
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        // Atomic-ish replace so a crash mid-write never corrupts settings.
        std::fs::rename(&tmp, path)?;
        tracing::debug!(path = %path.display(), "settings saved");
        Ok(())
    }

    pub fn save(&self) {
        if let Some(p) = default_path()
            && let Err(e) = self.save_to(&p)
        {
            tracing::warn!(error = %e, "failed to save settings");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.json");
        let s = Settings {
            theme: ThemeMode::Dark,
            update_speed: UpdateSpeed::High,
            graph_seconds: 120,
            ui_zoom: 1.25,
            window_size: [800.0, 600.0],
            ..Settings::default()
        };
        s.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.theme, ThemeMode::Dark);
        assert_eq!(loaded.update_speed, UpdateSpeed::High);
        assert_eq!(loaded.graph_seconds, 120);
        assert_eq!(loaded.window_size, [800.0, 600.0]);
        assert!((loaded.ui_zoom - 1.25).abs() < 1e-6);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let s = Settings::load_from(&path);
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn corrupt_file_falls_back_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s, Settings::default());
        assert!(path.with_extension("json.bad").exists());
    }

    #[test]
    fn intervals_sane() {
        assert_eq!(
            UpdateSpeed::High.interval(),
            std::time::Duration::from_millis(500)
        );
        assert_eq!(
            UpdateSpeed::Normal.interval(),
            std::time::Duration::from_secs(1)
        );
        assert_eq!(
            UpdateSpeed::Low.interval(),
            std::time::Duration::from_secs(4)
        );
    }
}
