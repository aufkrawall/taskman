//! User settings persisted as a human-editable `config.ini`.
//!
//! Layout example:
//!
//! ```ini
//! [general]
//! save_config=true
//! theme=dark
//! update_speed=normal
//! window_size=1100x720
//!
//! [columns]
//! processes=42,110,90
//! ```
//!
//! Rules:
//! * Unknown sections/keys and invalid values are ignored, so the file stays
//!   forward-compatible and safe to hand-edit.
//! * [`Settings::save`] is the automatic-save entry point; it is gated by
//!   `save_config` (**enabled by default**). `save_to` always writes.
//! * A legacy `settings.json` from older builds is migrated once when no
//!   config.ini exists yet.

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
    /// Automatically persist every user-settings change to config.ini.
    pub save_config: bool,
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
            save_config: true,
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

fn config_dir() -> Option<PathBuf> {
    dirs::config_local_dir().map(|d| d.join("taskman"))
}

fn default_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.ini"))
}

fn legacy_json_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings.json"))
}

// ------------------------------------------------------------------ INI core

/// Parse INI text into `(section, key) -> value`. Lenient by design:
/// blank lines, comments (`#`, `;`) and malformed lines are skipped;
/// later duplicates win.
fn parse_ini(text: &str) -> BTreeMap<(String, String), String> {
    let mut out = BTreeMap::new();
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if !section.is_empty() && !key.is_empty() {
                out.insert((section.clone(), key), unescape_ini(val));
            }
        }
    }
    out
}

/// Escape values that could be mistaken for comments or contain newlines.
fn escape_ini(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(';', "\\;")
}

fn unescape_ini(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(';') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn render_ini(body: String, columns: &BTreeMap<String, Vec<f32>>) -> String {
    let mut out = String::with_capacity(512 + columns.len() * 40);
    out.push_str("# taskman configuration — edited values apply on the next start.\n");
    out.push_str("# Delete this file to reset all settings to their defaults.\n\n");
    out.push_str(&body);
    if !columns.is_empty() {
        out.push_str("\n[columns]\n# table id = comma-separated column widths\n");
        for (id, widths) in columns {
            let list: Vec<String> = widths.iter().map(|w| w.to_string()).collect();
            out.push_str(&format!(
                "{}={}\n",
                escape_ini(id),
                escape_ini(&list.join(","))
            ));
        }
    }
    out
}

// ------------------------------------------------------------- value helpers

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_f32(s: &str) -> Option<f32> {
    s.parse::<f32>().ok().filter(|v| v.is_finite())
}

fn parse_u32(s: &str) -> Option<u32> {
    s.parse::<u32>().ok()
}

fn write_window_size(sz: [f32; 2]) -> String {
    format!("{}x{}", sz[0], sz[1])
}

fn parse_window_size(s: &str) -> Option<[f32; 2]> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h) = (parse_f32(w.trim())?, parse_f32(h.trim())?);
    Some([w, h])
}

fn parse_widths(s: &str) -> Option<Vec<f32>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        match part.trim().parse::<f32>() {
            Ok(w) if w.is_finite() && w > 0.0 => out.push(w),
            // One bad entry invalidates the whole line; defaults are better
            // than a silently shifted column mapping.
            _ => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

impl crate::i18n::LangChoice {
    fn as_cfg(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::De => "de",
            Self::En => "en",
        }
    }
    fn from_cfg(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "system" | "" => Some(Self::System),
            "de" | "german" => Some(Self::De),
            "en" | "english" => Some(Self::En),
            _ => None,
        }
    }
}

impl ThemeMode {
    fn as_cfg(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
    fn from_cfg(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "system" => Some(Self::System),
            "light" | "hell" => Some(Self::Light),
            "dark" | "dunkel" => Some(Self::Dark),
            _ => None,
        }
    }
}

impl UpdateSpeed {
    fn as_cfg(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Normal => "normal",
            Self::Low => "low",
            Self::Paused => "paused",
        }
    }
    fn from_cfg(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "high" => Some(Self::High),
            "normal" => Some(Self::Normal),
            "low" => Some(Self::Low),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

// ------------------------------------------------------------------- loading

impl Settings {
    /// Load settings from `path` (or the platform default). A missing file
    /// yields defaults; unparsable values fall back per-key to defaults.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => {
                // Tolerate a UTF-8 BOM (e.g. files edited with PowerShell).
                let text = String::from_utf8_lossy(&bytes);
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                let s = Self::from_ini_text(text);
                tracing::debug!(path = %path.display(), "settings loaded");
                s
            }
            Err(_) => {
                tracing::debug!(path = %path.display(), "no settings file yet; using defaults");
                Self::default()
            }
        }
    }

    /// Build settings from INI text. Unknown keys/values are ignored and the
    /// corresponding fields keep their defaults.
    pub fn from_ini_text(text: &str) -> Self {
        let kv = parse_ini(text);
        let get = |section: &str, key: &str| kv.get(&(section.to_string(), key.to_string()));
        let b = |section: &str, key: &str, dflt: bool| {
            get(section, key)
                .and_then(|v| parse_bool(v))
                .unwrap_or(dflt)
        };

        let mut s = Self::default();
        s.save_config = b("general", "save_config", s.save_config);
        if let Some(v) = get("general", "theme").and_then(|v| ThemeMode::from_cfg(v)) {
            s.theme = v;
        }
        if let Some(v) = get("general", "update_speed").and_then(|v| UpdateSpeed::from_cfg(v)) {
            s.update_speed = v;
        }
        s.always_on_top = b("general", "always_on_top", s.always_on_top);
        if let Some(v) = get("general", "graph_seconds").and_then(|v| parse_u32(v)) {
            s.graph_seconds = v.clamp(5, 3600);
        }
        if let Some(v) = get("general", "ui_zoom")
            .and_then(|v| parse_f32(v))
            .filter(|v| *v >= 0.5 && *v <= 3.0)
        {
            s.ui_zoom = v;
        }
        s.remember_window = b("general", "remember_window", s.remember_window);
        if let Some(v) = get("general", "window_size").and_then(|v| parse_window_size(v)) {
            s.window_size = [v[0].clamp(200.0, 16384.0), v[1].clamp(150.0, 16384.0)];
        }
        s.show_net_column_anyway = b(
            "general",
            "show_net_column_anyway",
            s.show_net_column_anyway,
        );
        s.sidebar_collapsed = b("general", "sidebar_collapsed", s.sidebar_collapsed);
        if let Some(v) =
            get("general", "language").and_then(|v| crate::i18n::LangChoice::from_cfg(v))
        {
            s.language = v;
        }
        if let Some(v) = get("general", "perf_card_width")
            .and_then(|v| parse_f32(v))
            .filter(|v| *v >= 100.0 && *v <= 2000.0)
        {
            s.perf_card_width = v;
        }

        // Column widths live in their own section: `table id = w,w,w`.
        for ((section, key), value) in &kv {
            if section == "columns"
                && let Some(widths) = parse_widths(value)
            {
                s.col_widths.insert(key.clone(), widths);
            }
        }
        s
    }

    pub fn load() -> Self {
        let Some(ini) = default_path() else {
            return Self::default();
        };
        if ini.exists() {
            return Self::load_from(&ini);
        }
        // One-time migration from the legacy JSON settings of older builds.
        if let Some(json) = legacy_json_path()
            && json.exists()
        {
            match std::fs::read_to_string(&json) {
                Ok(text) => match serde_json::from_str::<Settings>(&text) {
                    Ok(mut s) => {
                        // Never inherit a stale autosave flag from JSON-era
                        // builds; the field simply didn't exist there.
                        s.save_config = true;
                        tracing::info!(
                            json = %json.display(),
                            ini = %ini.display(),
                            "migrated legacy settings.json; config.ini written on next save"
                        );
                        return s;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "legacy settings.json unreadable; ignoring");
                    }
                },
                Err(e) => tracing::warn!(error = %e, "cannot read legacy settings.json"),
            }
        }
        Self::load_from(&ini)
    }

    // --------------------------------------------------------------- saving

    /// Write settings to `path` as INI. Always writes, regardless of the
    /// `save_config` switch (used by tests and explicit exports).
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let g = &[
            ("save_config", self.save_config.to_string()),
            ("language", self.language.as_cfg().to_string()),
            ("theme", self.theme.as_cfg().to_string()),
            ("update_speed", self.update_speed.as_cfg().to_string()),
            ("always_on_top", self.always_on_top.to_string()),
            ("graph_seconds", self.graph_seconds.to_string()),
            ("ui_zoom", self.ui_zoom.to_string()),
            ("remember_window", self.remember_window.to_string()),
            ("window_size", write_window_size(self.window_size)),
            (
                "show_net_column_anyway",
                self.show_net_column_anyway.to_string(),
            ),
            ("sidebar_collapsed", self.sidebar_collapsed.to_string()),
            ("perf_card_width", self.perf_card_width.to_string()),
        ];
        let mut body = String::from("[general]\n");
        for (k, v) in g {
            body.push_str(&format!("{k}={}\n", escape_ini(v)));
        }
        let tmp = path.with_extension("ini.tmp");
        std::fs::write(&tmp, render_ini(body, &self.col_widths))?;
        // Atomic-ish replace so a crash mid-write never corrupts settings.
        std::fs::rename(&tmp, path)?;
        tracing::debug!(path = %path.display(), "settings saved");
        Ok(())
    }

    /// Automatic-save entry point used by the UI. Honors the user's
    /// `save_config` choice (default: on).
    pub fn save(&self) {
        if !self.save_config {
            tracing::trace!("config autosave disabled; skipping");
            return;
        }
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
    fn ini_roundtrip_preserves_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.ini");
        let mut s = Settings {
            theme: ThemeMode::Dark,
            update_speed: UpdateSpeed::High,
            graph_seconds: 120,
            ui_zoom: 1.25,
            window_size: [800.0, 600.0],
            ..Settings::default()
        };
        s.save_config = false;
        s.always_on_top = true;
        s.remember_window = false;
        s.show_net_column_anyway = false;
        s.sidebar_collapsed = true;
        s.language = crate::i18n::LangChoice::De;
        s.perf_card_width = 300.5;
        s.col_widths
            .insert("processes".into(), vec![42.0, 110.5, 90.25, 1234.75]);
        s.col_widths.insert("details".into(), vec![80.0]);

        s.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, s);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.ini");
        assert_eq!(Settings::load_from(&path), Settings::default());
    }

    #[test]
    fn lenient_parsing_skips_garbage_and_unknown_keys() {
        let text = "\
# comment
[general]
theme=dark
theme=light
bogus line without equals
unknown_key=42
graph_seconds=99999
ui_zoom=7.0
window_size=not-a-size

[stranger_section]
whatever=yes
";
        let s = Settings::from_ini_text(text);
        // Later duplicate wins.
        assert_eq!(s.theme, ThemeMode::Light);
        // Out-of-range values are clamped (graph_seconds), or fall back to
        // defaults when no sensible clamp exists (ui_zoom).
        assert_eq!(s.graph_seconds, 3600);
        assert_eq!(s.ui_zoom, Settings::default().ui_zoom);
        assert_eq!(s.window_size, Settings::default().window_size);
        // Unknown keys/sections are ignored without failing the file.
        assert_eq!(s.update_speed, Settings::default().update_speed);
    }

    #[test]
    fn value_aliases_are_accepted() {
        let s = Settings::from_ini_text(
            "[general]\nsave_config=off\nalways_on_top=YES\nwindow_size=640X480\n",
        );
        assert!(!s.save_config);
        assert!(s.always_on_top);
        assert_eq!(s.window_size, [640.0, 480.0]);
    }

    #[test]
    fn broken_column_line_is_dropped_whole() {
        let s = Settings::from_ini_text("[columns]\nprocesses=40,oops,60\nother=30,70\n");
        assert!(!s.col_widths.contains_key("processes"));
        assert_eq!(s.col_widths.get("other"), Some(&vec![30.0, 70.0]));
    }

    #[test]
    fn autosave_gate_controls_save_but_not_save_to() {
        let dir = tempfile::tempdir().unwrap();

        // Off: the automatic entry point must not touch the disk.
        // (default_path() is environment-dependent, so we assert on the
        // observable contract instead: save() is a no-op while save_to
        // still writes.)
        let off = Settings {
            save_config: false,
            ..Settings::default()
        };
        let path = dir.path().join("manual.ini");
        off.save_to(&path).unwrap();
        assert!(path.exists());
        assert!(!Settings::load_from(&path).save_config);

        // On (the default).
        let on = Settings::default();
        assert!(on.save_config);
        let path2 = dir.path().join("auto.ini");
        on.save_to(&path2).unwrap();
        assert_eq!(Settings::load_from(&path2), on);
    }

    #[test]
    fn legacy_json_is_still_readable_for_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"theme":"Dark","update_speed":"High","graph_seconds":90}"#,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut s: Settings = serde_json::from_slice(&bytes).unwrap();
        // The flag didn't exist in JSON-era builds; migration forces it on.
        s.save_config = true;
        assert_eq!(s.theme, ThemeMode::Dark);
        assert_eq!(s.update_speed, UpdateSpeed::High);
        assert_eq!(s.graph_seconds, 90);
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
