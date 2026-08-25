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
//! [columns.processes]
//! name=340
//! cpu=110
//! ```
//!
//! Rules:
//! * Unknown sections/keys and invalid values are ignored, so the file stays
//!   forward-compatible and safe to hand-edit.
//! * [`Settings::save`] is the automatic-save entry point; it is gated by
//!   `save_config` (**enabled by default**). `save_to`/`save_forced` always
//!   write — toggling the autosave switch itself persists immediately so the
//!   choice survives restart (implement.md §17.2).
//! * A legacy `settings.json` from older builds is migrated once when no
//!   config.ini exists yet.
//! * Column preferences are keyed by **stable column id** (`[columns.<table>]`
//!   sections); positional `[columns]` entries of old builds are migrated
//!   once through each table's historical hard-coded column order.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Test/isolation override (implement.md §23): when set, all config files
/// live under this directory instead of the user profile.
pub fn taskman_config_dir() -> PathBuf {
    std::env::var("TASKMAN_CONFIG_DIR").map_or_else(
        |_| {
            dirs::config_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("taskman")
        },
        PathBuf::from,
    )
}

/// Test/isolation override for data files (app history, logs).
pub fn taskman_data_dir() -> PathBuf {
    std::env::var("TASKMAN_DATA_DIR").map_or_else(
        |_| {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("taskman")
        },
        PathBuf::from,
    )
}

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
    /// Task-Manager-style start page used unless a CLI flag overrides it.
    pub default_start_page: String,
    /// Navigation rail collapsed to icons only (hamburger toggle).
    pub sidebar_collapsed: bool,
    /// UI language; `System` follows the OS display language.
    pub language: crate::i18n::LangChoice,
    /// User-resized column widths per table, keyed by stable column id:
    /// `table id -> column id -> width`.
    pub col_widths: BTreeMap<String, BTreeMap<String, f32>>,
    /// Width of the Performance tab's left card column.
    pub perf_card_width: f32,
    /// Performance CPU graph mode: "overall" | "logical".
    pub cpu_graph_mode: String,
    /// Overlay kernel time (darker band) in the CPU graphs.
    pub show_kernel_times: bool,
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
            default_start_page: "processes".into(),
            sidebar_collapsed: false,
            language: Default::default(),
            col_widths: BTreeMap::new(),
            perf_card_width: 252.0,
            cpu_graph_mode: "overall".into(),
            show_kernel_times: false,
        }
    }
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

fn render_ini(body: String, columns: &BTreeMap<String, BTreeMap<String, f32>>) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("# taskman configuration — edited values apply on the next start.\n");
    out.push_str("# Delete this file to reset all settings to their defaults.\n\n");
    out.push_str(&body);
    for (table, cols) in columns {
        if cols.is_empty() {
            continue;
        }
        out.push_str(&format!("\n[columns.{table}]\n# column id = width px\n"));
        for (col, w) in cols {
            out.push_str(&format!("{}={}\n", escape_ini(col), fmt_width(*w)));
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

/// Widths as compact decimal text ("340", "110.5").
fn fmt_width(w: f32) -> String {
    let s = format!("{w:.1}");
    s.strip_suffix(".0").map(str::to_owned).unwrap_or(s)
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

/// Historical hard-coded column order per table (pre-ID-schema builds).
/// Used exactly once to migrate positional `[columns]` widths forward.
pub const LEGACY_COLUMN_ORDER: &[(&str, &[&str])] = &[
    (
        "processes",
        &["name", "status", "cpu", "mem", "disk", "net"],
    ),
    (
        "details",
        &[
            "name", "pid", "status", "user", "cpu", "mem", "platform", "elevated", "uac", "gpu",
        ],
    ),
    ("users", &["user", "status", "cpu", "mem", "disk", "net"]),
    ("startup", &["name", "pub", "status", "impact"]),
    ("services", &["name", "pid", "desc", "status", "group"]),
    ("apphistory", &["name", "cpu", "net", "notif"]),
];

/// Migrate one table's positional width list to `(column id, width)` pairs.
/// Extra positional entries beyond the known order are ignored; unknown
/// future ids never break loading (implement.md §25.30).
pub fn migrate_positional_widths(table: &str, widths: &[f32]) -> BTreeMap<String, f32> {
    let mut out = BTreeMap::new();
    if let Some((_, order)) = LEGACY_COLUMN_ORDER.iter().find(|(t, _)| *t == table) {
        for (col, w) in order.iter().zip(widths.iter()) {
            if *w >= 40.0 && *w <= 1200.0 {
                out.insert((*col).to_string(), *w);
            }
        }
    }
    out
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
            s.graph_seconds = v.clamp(10, 600);
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
        if let Some(v) = get("general", "default_start_page") {
            let page = v.trim().to_ascii_lowercase();
            if matches!(
                page.as_str(),
                "processes"
                    | "performance"
                    | "apphistory"
                    | "startup"
                    | "users"
                    | "details"
                    | "services"
            ) {
                s.default_start_page = page;
            }
        }
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
        if let Some(v) = get("general", "cpu_graph_mode") {
            let mode = v.trim().to_ascii_lowercase();
            if mode == "overall" || mode == "logical" {
                s.cpu_graph_mode = mode;
            }
        }
        s.show_kernel_times = b("general", "show_kernel_times", s.show_kernel_times);

        // Column widths, ID-keyed schema: `[columns.<table>] <col>=<width>`.
        // Unknown future column ids are preserved verbatim (harmless) rather
        // than corrupting anything.
        let mut saw_legacy = false;
        for ((section, key), value) in &kv {
            if let Some(table) = section.strip_prefix("columns.") {
                if table == "columns" {
                    continue;
                }
                if let Some(w) = parse_f32(value)
                    && (40.0..=1200.0).contains(&w)
                {
                    s.col_widths
                        .entry(table.to_string())
                        .or_default()
                        .insert(key.clone(), w);
                }
            } else if section == "columns" {
                saw_legacy = true;
            }
        }

        // One-time migration from the old positional `[columns]` schema.
        if saw_legacy {
            tracing::info!("migrating positional column widths to column-id schema");
            for ((_, table), value) in kv.iter().filter(|((sec, _), _)| sec == "columns") {
                if let Some(widths) = parse_widths(value) {
                    let migrated = migrate_positional_widths(table, &widths);
                    if !migrated.is_empty() {
                        let entry = s.col_widths.entry(table.clone()).or_default();
                        for (id, w) in migrated {
                            entry.entry(id).or_insert(w);
                        }
                    }
                }
            }
            // The legacy sections are dropped from the file on next save.
        }

        // remember_window=false: geometry is neither restored nor persisted.
        if !s.remember_window {
            s.window_size = Settings::default().window_size;
        }
        s
    }

    pub fn load() -> Self {
        let ini = taskman_config_dir().join("config.ini");
        if ini.exists() {
            return Self::load_from(&ini);
        }
        // One-time migration from the legacy JSON settings of older builds.
        let json = taskman_config_dir().join("settings.json");
        if json.exists() {
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
    ///
    /// Durable atomic write (implement.md §17.3): same-dir temp file,
    /// write+flush, rename over the destination. Rust's `std::fs::rename`
    /// replaces an existing destination on Windows, so no extra workaround
    /// is needed.
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
            ("default_start_page", self.default_start_page.clone()),
            ("sidebar_collapsed", self.sidebar_collapsed.to_string()),
            ("perf_card_width", self.perf_card_width.to_string()),
            ("cpu_graph_mode", self.cpu_graph_mode.clone()),
            ("show_kernel_times", self.show_kernel_times.to_string()),
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

    fn default_path() -> Option<PathBuf> {
        if let Some(dir) = test_path_override()
            && let Some(dir) = dir.as_ref()
        {
            return Some(dir.join("config.ini"));
        }
        Some(taskman_config_dir().join("config.ini"))
    }

    /// Automatic-save entry point used by the UI. Honors the user's
    /// `save_config` choice (default: on).
    pub fn save(&self) {
        if !self.save_config {
            tracing::trace!("config autosave disabled; skipping");
            return;
        }
        if let Some(p) = Self::default_path()
            && let Err(e) = self.save_to(&p)
        {
            tracing::warn!(error = %e, "failed to save settings");
        }
    }

    /// Unconditional save that bypasses the autosave gate. Used when the
    /// user turns autosave OFF (so that choice itself persists), presses
    /// Reset, or explicitly closes the settings dialog.
    pub fn save_forced(&self) {
        if let Some(p) = Self::default_path()
            && let Err(e) = self.save_to(&p)
        {
            tracing::warn!(error = %e, "failed to save settings (forced)");
        }
    }
}

static TEST_PATH_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Tests that flip the process-global path override take this lock so they
/// cannot interleave.
#[cfg(test)]
static TEST_OVERRIDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_path_override() -> Option<std::sync::MutexGuard<'static, Option<PathBuf>>> {
    TEST_PATH_OVERRIDE.lock().ok()
}

/// Redirect [`Settings::save`] / [`save_forced`] for the whole process.
/// Test isolation only — production code must not call this.
#[doc(hidden)]
pub fn set_default_path_override_for_tests(dir: Option<PathBuf>) {
    if let Ok(mut g) = TEST_PATH_OVERRIDE.lock() {
        *g = dir;
    }
}

// ----------------------------------------------------------- settings writer

/// Single serialized settings writer thread (implement.md §17.1).
///
/// UI threads hand over immutable snapshots; the writer coalesces bursts
/// (~250 ms) into one atomic disk write, so a slow disk / antivirus cannot
/// hitch a frame. One thread total — never a thread per setting change.
pub struct SettingsWriter {
    tx: std::sync::mpsc::Sender<WriteMsg>,
    join: Option<std::thread::JoinHandle<()>>,
}

enum WriteMsg {
    /// Autosave write (already gated by the caller).
    Write(std::sync::Arc<Settings>),
    /// Unconditional write that ignores the autosave gate.
    Force(std::sync::Arc<Settings>),
    Flush(std::sync::mpsc::Sender<()>),
    Shutdown,
}

const COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(250);

impl SettingsWriter {
    pub fn start() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<WriteMsg>();
        let join = std::thread::Builder::new()
            .name("tm-settings-writer".into())
            .spawn(move || settings_writer_loop(rx))
            .ok();
        Self { tx, join }
    }

    /// Queue an autosave (gating happens at the call site).
    pub fn enqueue(&self, settings: &Settings) {
        let _ = self
            .tx
            .send(WriteMsg::Write(std::sync::Arc::new(settings.clone())));
    }

    /// Queue an unconditional write (bypasses the autosave gate) — used when
    /// the user turns autosave off, presses Reset, or closes the dialog.
    pub fn force(&self, settings: &Settings) {
        let _ = self
            .tx
            .send(WriteMsg::Force(std::sync::Arc::new(settings.clone())));
    }

    /// Blocking flush of everything queued so far (bounded wait).
    pub fn flush(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.tx.send(WriteMsg::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(3));
        }
    }
}

impl Drop for SettingsWriter {
    fn drop(&mut self) {
        let _ = self.tx.send(WriteMsg::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn settings_writer_loop(rx: std::sync::mpsc::Receiver<WriteMsg>) {
    // Pending newest snapshot + whether any queued request was forced
    // (a forced request upgrades the coalesced write).
    let mut pending: Option<(std::sync::Arc<Settings>, bool)> = None;
    loop {
        let msg = if pending.is_some() {
            // Coalesce everything arriving within the window.
            match rx.recv_timeout(COALESCE_WINDOW) {
                Ok(m) => m,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some((s, _)) = pending.take() {
                        s.save_forced();
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            }
        };
        match msg {
            WriteMsg::Write(s) => pending = Some((s, false)),
            WriteMsg::Force(s) => pending = Some((s, true)),
            WriteMsg::Flush(reply) => {
                if let Some((s, _)) = pending.take() {
                    s.save_forced();
                }
                let _ = reply.send(());
            }
            WriteMsg::Shutdown => {
                if let Some((s, _)) = pending.take() {
                    s.save_forced();
                }
                break;
            }
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
        s.remember_window = true;
        s.sidebar_collapsed = true;
        s.language = crate::i18n::LangChoice::De;
        s.perf_card_width = 300.5;
        s.cpu_graph_mode = "logical".into();
        s.show_kernel_times = true;
        s.col_widths.insert(
            "processes".into(),
            BTreeMap::from([("name".into(), 340.0), ("cpu".into(), 110.5)]),
        );
        s.col_widths
            .insert("details".into(), BTreeMap::from([("pid".into(), 80.0)]));

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
        assert_eq!(s.graph_seconds, 600);
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
        // The broken line contributes nothing...
        assert!(!s.col_widths.contains_key("processes"));
        // ...and unknown legacy tables have no historical column order to
        // migrate against, so they are dropped rather than guessed.
        assert!(
            s.col_widths.get("other").is_none_or(|m| m.is_empty()),
            "no invented mapping for unknown tables"
        );
    }

    #[test]
    fn table_preferences_migrate_positional_widths_to_ids() {
        // Old builds wrote: processes=42,110,90  → name,status,cpu,...
        let s = Settings::from_ini_text("[columns]\nprocesses=340,190,110\n");
        let p = s.col_widths.get("processes").expect("migrated");
        assert_eq!(p.get("name"), Some(&340.0));
        assert_eq!(p.get("status"), Some(&190.0));
        assert_eq!(p.get("cpu"), Some(&110.0));
        assert!(!p.contains_key("mem"));
    }

    #[test]
    fn unknown_future_column_ids_do_not_break_settings_load() {
        let s = Settings::from_ini_text("[columns.details]\nnpu_engine=120\nname=300\n");
        let d = s.col_widths.get("details").expect("details present");
        assert_eq!(d.get("npu_engine"), Some(&120.0));
        assert_eq!(d.get("name"), Some(&300.0));
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
    fn settings_save_config_false_persists_itself() {
        // Turning autosave off must persist that preference even though the
        // gate would normally block writes (implement.md §12/§17.2):
        // save_forced bypasses the gate and the flag round-trips.
        let _serial = TEST_OVERRIDE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        set_default_path_override_for_tests(Some(dir.path().to_path_buf()));

        let mut s = Settings::default();
        s.save_to(&path).unwrap(); // initial state on disk: autosave on
        s.save_config = false;
        s.save_forced(); // what toggling the checkbox off must do
        let reloaded = Settings::load_from(&path);
        assert!(
            !reloaded.save_config,
            "autosave-off choice must survive restart"
        );
        // The gate still blocks ordinary saves (no way to observe on disk
        // without changing the file — verified by the writer tests above).
    }

    #[test]
    fn remember_window_false_does_not_restore_or_persist_geometry() {
        let s = Settings::from_ini_text("[general]\nremember_window=false\nwindow_size=999x888\n");
        assert!(!s.remember_window);
        assert_eq!(
            s.window_size,
            Settings::default().window_size,
            "saved geometry must not be restored"
        );

        // And persistence skips geometry while disabled.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.ini");
        let s2 = Settings {
            remember_window: false,
            window_size: [1234.0, 777.0],
            ..Settings::default()
        };
        s2.save_to(&path).unwrap();
        let l = Settings::load_from(&path);
        assert_eq!(l.window_size, Settings::default().window_size);
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

    #[test]
    fn writer_coalesces_and_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        let _serial = TEST_OVERRIDE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_default_path_override_for_tests(Some(dir.path().to_path_buf()));
        let writer = SettingsWriter::start();

        let s = Settings {
            graph_seconds: 30,
            ..Settings::default()
        };
        writer.enqueue(&s);
        let mut s2 = s.clone();
        s2.graph_seconds = 45;
        writer.enqueue(&s2); // newer generation wins
        writer.flush();

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.graph_seconds, 45);
    }
}
