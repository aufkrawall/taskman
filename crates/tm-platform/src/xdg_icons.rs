//! Freedesktop (XDG) icon-theme resolution: map an executable to the icon
//! the OS's current icon theme would show for it, and decode that icon to
//! straight-alpha RGBA for the GUI.
//!
//! The lookup follows the icon theme spec (https://specifications.freedesktop.org/icon-theme-spec):
//! an executable basename is matched against the `Exec=`/`TryExec=` line (or
//! the desktop file's own id) of the `.desktop` files in the XDG data dirs;
//! the resulting `Icon=` value is then looked up in the icon theme chain (the
//! configured theme and its transitive `Inherits`, or the `default` theme when
//! no theme is configured, then `hicolor`) under `$XDG_DATA_HOME/icons`,
//! `$XDG_DATA_DIRS/icons` and finally the legacy pixmaps dirs. An absolute
//! `Icon=` path is used directly.
//!
//! Only PNG icons are decoded (the overwhelmingly common case in hicolor and
//! shipped themes); SVG/XPM candidates are skipped while a size directory is
//! scanned — the next candidate size or theme still gets its turn — so an app
//! whose icon exists only as SVG falls back to the generic glyph. The module is
//! platform-neutral on purpose: only the Linux backend calls it, but compiling
//! it everywhere lets its unit tests run on the Windows dev box and CI.

use std::borrow::Borrow;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Maximum edge of a decoded icon; larger PNGs are box-downsampled so the
/// GUI-side texture cache stays small (64×64×4 ≈ 16 KiB per icon).
const MAX_ICON_EDGE: u32 = 64;
/// Refuse absurd source images before allocating their decoded buffer. A
/// 2048×2048 RGBA image is already 16 MiB — far more than any icon needs,
/// and it still gets downsampled to [`MAX_ICON_EDGE`].
const MAX_ICON_SOURCE_EDGE: u32 = 2048;
/// Decoder allocation ceiling. The dimension cap above already bounds the
/// useful case; this also keeps a PNG with malicious chunk sizes from
/// reserving the library's 64 MiB default.
const MAX_ICON_DECODE_BYTES: usize = 32 * 1024 * 1024;
/// The GUI paints icons at ~18 px; 32 is the size desktop shells hand out.
const TARGET_SIZE: u32 = 32;

/// Bounds for the caches of a lookup's per-process static answers: a theme's
/// layout, an icon name's file and a decoded PNG do not change while the
/// process runs. The caps only keep a session that meets thousands of distinct
/// names from growing them without limit; 128 decoded icons is at most ~2 MiB
/// (the 64 px cap allows 16 KiB each, a 32 px theme icon is 4 KiB).
const THEME_DIR_CACHE_CAP: usize = 64;
const FILE_CACHE_CAP: usize = 1024;
const DECODE_CACHE_CAP: usize = 128;

/// Theme that stands in for "the desktop's configured theme" when nothing
/// names one: `/usr/share/icons/default/index.theme` is the spec's designated
/// default and usually just inherits the real theme (`Inherits=Adwaita` on a
/// stock GNOME box). Without it `hicolor` — which ships almost no application
/// icons — was the only theme left, so nearly every row fell back to the
/// generic glyph, or to nothing at all when even that one is missing.
const DEFAULT_THEME: &str = "default";
/// Mandatory last entry of every chain (icon theme spec).
const FALLBACK_THEME: &str = "hicolor";

/// `Exec=` binaries that merely launch another application. A `.desktop`
/// entry for a Python/Java/Wine/Flatpak app must not make the interpreter's
/// own process show that app's icon.
const WRAPPER_BINARIES: &[&str] = &[
    "python",
    "python2",
    "python3",
    "java",
    "wine",
    "wine64",
    "wine-preloader",
    "flatpak",
    "snap",
    "sh",
    "bash",
    "dash",
    "node",
    "nodejs",
    "perl",
    "ruby",
    "mono",
    "electron",
    "steam",
    "xdg-open",
    // Execution prefixes: they are followed by the real binary, so the prefix
    // itself must never be indexed as an application — one shared `env` or
    // `sudo` would otherwise collect every app that uses it.
    "env",
    "sudo",
    "setsid",
    "nohup",
    "nice",
];

/// `Exec=` prefixes that are followed by the binary they run, so the prefix
/// token is skipped instead of becoming the executable name.
const EXEC_PREFIXES: &[&str] = &["env", "sudo", "setsid", "nohup", "nice"];

/// Prefix flags that take a separate value (`nice -n 10`, `sudo -u root`), so
/// the value is not mistaken for the binary the prefix runs.
const PREFIX_FLAG_VALUES: &[&str] = &["-n", "-u", "-g", "-p", "-c", "-C", "-t", "-U"];

/// The Flatpak launcher. Its entries run `<launcher> run … --command=<app>`;
/// inside the sandbox the process is the command, not the launcher.
const FLATPAK_LAUNCHER: &str = "flatpak";

/// Icon names that stand for "an application without an icon of its own", in
/// fallback order: the standard executable icon every theme ships (the
/// freedesktop equivalent of the Windows shell's generic application icon),
/// then the spec's missing-image icon.
pub const DEFAULT_APP_ICON_NAMES: [&str; 3] = [
    "application-x-executable",
    "application-default-icon",
    "image-missing",
];

/// Where to look for desktop files and icon themes.
#[derive(Debug, Clone, Default)]
pub struct IconLookup {
    /// XDG data dirs, highest precedence first (`$XDG_DATA_HOME`, then
    /// `$XDG_DATA_DIRS`). Desktop files live in `<dir>/applications`, icon
    /// themes in `<dir>/icons`.
    pub data_dirs: Vec<PathBuf>,
    /// Legacy pixmaps dirs, highest precedence first (`<data dir>/pixmaps`
    /// followed by the historical `/usr/share/pixmaps`): icons installed as
    /// plain files instead of through a theme.
    pub pixmaps: Vec<PathBuf>,
    /// Name of the current icon theme (e.g. from the GTK settings), if known.
    pub theme: Option<String>,
    /// `$HOME/.icons`: the spec's legacy per-user icon dir, which hand-installed
    /// themes and icons still use. Searched with the themes and again for plain
    /// pixmap files.
    pub home_icons: Option<PathBuf>,
}

impl IconLookup {
    /// Build the lookup from the process environment (XDG variables, the
    /// desktop's settings files for the current icon theme).
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local").join("share")))
            .unwrap_or_default();
        let mut data_dirs: Vec<PathBuf> = Vec::new();
        if !data_home.as_os_str().is_empty() {
            data_dirs.push(data_home);
        }
        let dirs = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
        for d in dirs.split(':').filter(|s| !s.is_empty()) {
            let p = PathBuf::from(d);
            if !data_dirs.contains(&p) {
                data_dirs.push(p);
            }
        }
        let theme = current_icon_theme(home.as_deref());
        let home_icons = home.as_ref().map(|h| h.join(".icons"));
        let pixmaps = pixmap_dirs(&data_dirs, home_icons.as_deref());
        Self {
            data_dirs,
            pixmaps,
            theme,
            home_icons,
        }
    }

    /// Icon theme dirs, highest precedence first: each `<data dir>/icons` plus
    /// [`IconLookup::home_icons`], which belongs with the per-user data home
    /// (index 0) and not at the end where the system themes would win.
    fn icon_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self.data_dirs.iter().map(|d| d.join("icons")).collect();
        if let Some(legacy) = &self.home_icons
            && !dirs.contains(legacy)
        {
            let at = dirs.len().min(1);
            dirs.insert(at, legacy.clone());
        }
        dirs
    }
}

/// Legacy pixmaps dirs for `data_dirs`, highest precedence first.
///
/// Hand-installed icons land in `<data dir>/pixmaps` (`~/.local/share/pixmaps`
/// on a stock desktop, `/usr/local/share/pixmaps` for local packages), loose
/// files of hand-installed apps in `$HOME/.icons`, and packaged ones in the
/// historical `/usr/share/pixmaps`, which is not necessarily an entry of
/// `$XDG_DATA_DIRS`.
fn pixmap_dirs(data_dirs: &[PathBuf], home_icons: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = data_dirs.iter().map(|d| d.join("pixmaps")).collect();
    // `$HOME/.icons` also holds loose files of hand-installed apps: it ranks
    // with the per-user data home, ahead of the system dirs.
    if let Some(home_icons) = home_icons {
        let home_icons = home_icons.to_path_buf();
        if !dirs.contains(&home_icons) {
            let at = dirs.len().min(1);
            dirs.insert(at, home_icons);
        }
    }
    let legacy = PathBuf::from("/usr/share/pixmaps");
    if !dirs.contains(&legacy) {
        dirs.push(legacy);
    }
    dirs
}

/// Prefer the running desktop's XSettings owner to GTK's fallback files.
#[cfg(target_os = "linux")]
fn x11_icon_theme() -> Option<String> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (conn, screen) = x11rb::connect(None).ok()?;
    let selection = conn
        .intern_atom(true, format!("_XSETTINGS_S{screen}").as_bytes())
        .ok()?
        .reply()
        .ok()?
        .atom;
    let window = conn
        .get_selection_owner(selection)
        .ok()?
        .reply()
        .ok()?
        .owner;
    if window == x11rb::NONE {
        return None;
    }
    let settings = conn
        .intern_atom(true, b"_XSETTINGS_SETTINGS")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let property = conn
        .get_property(false, window, settings, settings, 0, 16_384)
        .ok()?
        .reply()
        .ok()?;
    if property.format != 8 || property.bytes_after != 0 {
        return None;
    }
    parse_xsettings_icon_theme(&property.value).map(str::to_owned)
}

/// XSettings property format, shared with the parser tests on every host.
#[cfg(any(target_os = "linux", test))]
fn parse_xsettings_icon_theme(mut data: &[u8]) -> Option<&str> {
    fn take<'a>(data: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
        let (value, rest) = data.split_at_checked(len)?;
        *data = rest;
        Some(value)
    }

    let header = take(&mut data, 12)?;
    let big_endian = match header[0] {
        0 => false,
        1 => true,
        _ => return None,
    };
    let as_u16 = |bytes: [u8; 2]| {
        if big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        }
    };
    let as_u32 = |bytes: [u8; 4]| {
        if big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    };

    let count = as_u32(header[8..12].try_into().ok()?);
    let mut theme = None;
    for _ in 0..count {
        let record = take(&mut data, 4)?;
        let name_len = usize::from(as_u16(record[2..4].try_into().ok()?));
        let name = &take(&mut data, name_len.next_multiple_of(4))?[..name_len];
        take(&mut data, 4)?; // last-change serial
        match record[0] {
            0 => {
                take(&mut data, 4)?;
            }
            1 => {
                let len = as_u32(take(&mut data, 4)?.try_into().ok()?) as usize;
                let padded = len.checked_add(3)? & !3;
                let value = &take(&mut data, padded)?[..len];
                if name == b"Net/IconThemeName" {
                    theme = std::str::from_utf8(value)
                        .ok()
                        .filter(|name| !name.is_empty());
                }
            }
            2 => {
                take(&mut data, 8)?;
            }
            _ => return None,
        }
    }
    theme
}

/// `gtk-icon-theme-name` from the GTK 3/4 settings.ini or `Theme=` from KDE's
/// `kdeglobals`, if present.
///
/// Order matters: the XSettings owner is what the running desktop has applied
/// right now, GTK's own file is the next best statement of intent, and KDE's
/// `kdeglobals` is the only one Plasma still writes on Wayland (it has no
/// XSettings property there and may not have generated a GTK settings.ini).
fn current_icon_theme(home: Option<&Path>) -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Some(theme) = x11_icon_theme() {
        return Some(theme);
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".config")))?;
    for settings in [
        config_home.join("gtk-4.0").join("settings.ini"),
        config_home.join("gtk-3.0").join("settings.ini"),
    ] {
        let Ok(text) = std::fs::read_to_string(&settings) else {
            continue;
        };
        for line in text.lines() {
            if let Some(v) = line
                .trim()
                .strip_prefix("gtk-icon-theme-name")
                .and_then(|r| r.trim().strip_prefix('='))
            {
                let name = v.trim().trim_matches('"').to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    let kdeglobals = std::fs::read_to_string(config_home.join("kdeglobals")).ok();
    kdeglobals.as_deref().and_then(parse_kde_icon_theme)
}

/// `Theme=` of the `[Icons]` section of a `kdeglobals`.
fn parse_kde_icon_theme(text: &str) -> Option<String> {
    let mut in_icons = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_icons = line == "[Icons]";
            continue;
        }
        if in_icons && let Some(v) = line.strip_prefix("Theme=") {
            let name = v.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// A small fixed-capacity map that drops its oldest insertion once full.
///
/// Every entry it holds is per-process static, so eviction only matters for a
/// session that meets thousands of distinct names; FIFO keeps it predictable.
struct Bounded<K, V> {
    entries: HashMap<K, V>,
    order: VecDeque<K>,
    cap: usize,
}

impl<K: Eq + Hash + Clone, V> Bounded<K, V> {
    fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.entries.get(key)
    }

    fn insert(&mut self, key: K, value: V) {
        // Replacing a known key keeps its position, so `order` and `entries`
        // stay in lockstep and the replacement does not allocate.
        if let Some(slot) = self.entries.get_mut(&key) {
            *slot = value;
            return;
        }
        self.entries.insert(key.clone(), value);
        self.order.push_back(key);
        while self.entries.len() > self.cap {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.entries.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// Lazily filled answers: a theme's parsed layout, an icon name's file and a
/// PNG's decoded pixels.
///
/// Only successes are stored. A miss must stay retryable: the GUI keeps a
/// failed key out of the queue for `FAILURE_RETRY_TTL` and then asks again, so
/// an icon, theme file or temporarily unreadable directory that shows up later
/// still resolves. Caching the negative for the process lifetime would turn
/// that bounded retry into "never again", and a directory read can fail for
/// reasons that have nothing to do with the icon existing (EACCES, EMFILE, an
/// I/O error on a network home).
struct Caches {
    /// `<icon dir>/<theme>` → its candidate size dirs, best first.
    theme_dirs: Bounded<PathBuf, Arc<Vec<SizeDir>>>,
    /// Icon name → resolved file.
    files: Bounded<String, PathBuf>,
    /// Resolved file → decoded RGBA.
    decoded: Bounded<PathBuf, (u32, u32, Vec<u8>)>,
}

impl Default for Caches {
    fn default() -> Self {
        Self {
            theme_dirs: Bounded::new(THEME_DIR_CACHE_CAP),
            files: Bounded::new(FILE_CACHE_CAP),
            decoded: Bounded::new(DECODE_CACHE_CAP),
        }
    }
}

/// Resolver with a lazily built desktop-file index (exe basename → icon
/// name), so repeated lookups do not rescan `applications/`, plus bounded
/// caches for the rest of a lookup's static answers: a theme's parsed layout,
/// an icon name's file, and a decoded PNG (one `Icon=` value is shared by many
/// executables — every Electron helper, every launcher entry).
pub struct IconResolver {
    lookup: IconLookup,
    /// exe basename (lowercase) → `Icon=` value, highest-precedence desktop
    /// file wins.
    desktop_index: HashMap<String, String>,
    /// Themes to search, in order: the configured one (or the spec's
    /// `default`), its transitive `Inherits`, then `hicolor`.
    ///
    /// Resolved with the resolver rather than per lookup: the chain costs an
    /// XSettings round-trip plus one `index.theme` read per theme, and the file
    /// cache below answers by icon name, which is theme-independent. A theme
    /// the user switches to at runtime is therefore picked up when TaskMan next
    /// starts, not mid-session.
    themes: Vec<String>,
    /// Only the icon worker resolves icons, so this is never contended.
    caches: Mutex<Caches>,
}

impl IconResolver {
    /// Process-wide resolver built from the environment; icon extraction runs
    /// on a worker thread, so the index is built once and shared.
    pub fn shared() -> &'static Self {
        static RESOLVER: OnceLock<IconResolver> = OnceLock::new();
        RESOLVER.get_or_init(|| Self::new(IconLookup::from_env()))
    }

    pub fn new(lookup: IconLookup) -> Self {
        let desktop_index = build_desktop_index(&lookup.data_dirs);
        let themes = theme_chain(&lookup.icon_dirs(), lookup.theme.as_deref());
        Self {
            lookup,
            desktop_index,
            themes,
            caches: Mutex::new(Caches::default()),
        }
    }

    /// Resolve `exe_path` (an executable path, a `.desktop` file path, or a
    /// bare command name) to a decoded RGBA icon.
    pub fn icon_rgba_for_exe(&self, exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
        let name = self.icon_name_for(exe_path)?;
        let file = self.find_icon_file(&name)?;
        self.icon_rgba_for_file(&file)
    }

    /// The icon the theme shows for an executable that has no icon of its own
    /// (a kernel thread, an app without a `.desktop` entry): the standard
    /// "this is an application" icon, then the spec's missing-image icon.
    pub fn default_app_icon_rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
        DEFAULT_APP_ICON_NAMES.iter().find_map(|name| {
            self.find_icon_file(name)
                .and_then(|f| self.icon_rgba_for_file(&f))
        })
    }

    /// The icon name (or absolute path) the OS would use for `exe_path`.
    pub fn icon_name_for(&self, exe_path: &str) -> Option<String> {
        let path = Path::new(exe_path);
        // A .desktop file given directly (XDG autostart items): its own
        // Icon= entry is authoritative.
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("desktop"))
            && let Some(icon) = desktop_file_icon(path)
        {
            return Some(icon);
        }
        let base = path.file_name().map(|n| n.to_string_lossy().into_owned())?;
        // The index is keyed lowercase so a case-sensitive filesystem cannot
        // hide `MyApp.desktop` from a `myapp` process.
        if let Some(icon) = self.desktop_index.get(&base.to_ascii_lowercase()) {
            return Some(icon.clone());
        }
        // No desktop entry: the icon theme spec's fallback is the executable
        // basename as a themed icon name (hicolor ships one for most apps).
        // That name keeps its original case — a theme file is `MyApp.png`, not
        // `myapp.png`, on the case-sensitive filesystems Linux uses.
        Some(base)
    }

    /// Locate the file for an icon `name` following the theme spec: absolute
    /// path used as-is; otherwise the theme chain across all icon dirs, then
    /// the pixmaps dirs. PNG files only.
    ///
    /// The answer is cached by name, so the walk runs once per distinct name
    /// rather than once per executable that uses it. A miss is not cached: the
    /// GUI retries a failed lookup on its own bounded schedule.
    pub fn find_icon_file(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() {
            return None;
        }
        let cached = tm_core::sync::lock(&self.caches).files.get(name).cloned();
        if let Some(cached) = cached {
            return Some(cached);
        }
        let file = self.resolve_icon_file(name)?;
        tm_core::sync::lock(&self.caches)
            .files
            .insert(name.to_string(), file.clone());
        Some(file)
    }

    /// Uncached [`Self::find_icon_file`]: the walk over the theme chain, then
    /// the pixmaps dirs.
    fn resolve_icon_file(&self, name: &str) -> Option<PathBuf> {
        // Per the spec an icon name that is an absolute path is used directly.
        if Path::new(name).is_absolute() {
            let p = PathBuf::from(name);
            return p.is_file().then_some(p);
        }
        // A themed icon name is one path component. `Icon=` and `Inherits=`
        // reach `Path::join` below, where a relative `..` or a separator would
        // resolve outside the icon dirs the spec says to search.
        if !is_themed_icon_name(name) {
            return None;
        }
        let file_name = png_file_name(name);
        let icon_dirs = self.lookup.icon_dirs();
        for theme in &self.themes {
            for dir in &icon_dirs {
                if let Some(f) = self.find_in_theme(dir, theme, &file_name) {
                    return Some(f);
                }
            }
        }
        // Legacy pixmaps dirs hold plain files: `<pixmaps>/<name>.png`.
        self.lookup
            .pixmaps
            .iter()
            .map(|px| px.join(&file_name))
            .find(|f| f.is_file())
    }

    /// Find `<icon_dir>/<theme>/<size>/<context>/<file_name>`, choosing the size
    /// closest to [`TARGET_SIZE`]. Uses the theme's `index.theme` directory list
    /// when present; otherwise walks the two-level `<size>/<context>` layout.
    fn find_in_theme(&self, icon_dir: &Path, theme: &str, file_name: &str) -> Option<PathBuf> {
        let theme_dir = icon_dir.join(theme);
        if !theme_dir.is_dir() {
            return None;
        }
        for dir in self.size_dirs(&theme_dir).iter() {
            // The icon may sit directly in the size dir (rare) or one context
            // level deeper (the standard layout).
            let direct = dir.path.join(file_name);
            if direct.is_file() {
                return Some(direct);
            }
            // Sorted like every other directory walk here: one icon name can
            // exist in more than one context of the same size dir, and a
            // filesystem-defined order must not decide which file wins.
            if let Some(f) = first_in_contexts(&sorted_dir_paths(&dir.path), file_name) {
                return Some(f);
            }
        }
        None
    }

    /// Candidate size dirs of `theme_dir`, best first, parsed once per theme
    /// dir: a theme's layout is static for the process, while it used to be
    /// re-read from `index.theme` — and its `<size>` dirs re-listed — for every
    /// theme of every icon dir, on every lookup.
    fn size_dirs(&self, theme_dir: &Path) -> Arc<Vec<SizeDir>> {
        let cached = {
            let caches = tm_core::sync::lock(&self.caches);
            caches.theme_dirs.get(theme_dir).cloned()
        };
        if let Some(cached) = cached {
            return cached;
        }
        // Parsed outside the guard: reading `index.theme` is filesystem work,
        // and the lock is meant to cover the map, not the I/O.
        let dirs = Arc::new(size_dirs_of_theme(theme_dir));
        // An empty layout is not a success: it is what a failed `read_dir`, a
        // half-written `index.theme` or a `Directories=` list that has not been
        // filled in yet all look like, and those clear. Keeping it would make
        // that theme resolve nothing for the rest of the session, which is the
        // exact failure this cache was fixed to avoid.
        if !dirs.is_empty() {
            tm_core::sync::lock(&self.caches)
                .theme_dirs
                .insert(theme_dir.to_path_buf(), dirs.clone());
        }
        dirs
    }

    /// Decoded pixels of an icon file, decoded once per process.
    ///
    /// Many executables share one `Icon=` value (every Electron helper, every
    /// Wine process of the same entry), so the same PNG used to be read and
    /// downsampled once per distinct executable path. A file that does not
    /// decode is *not* remembered: the caller retries on its own schedule and a
    /// miss stays `None`, so the row draws nothing rather than a fabricated
    /// icon.
    fn icon_rgba_for_file(&self, file: &Path) -> Option<(u32, u32, Vec<u8>)> {
        let cached = {
            let caches = tm_core::sync::lock(&self.caches);
            caches.decoded.get(file).cloned()
        };
        if let Some(cached) = cached {
            return Some(cached);
        }
        // Decoded outside the guard: a PNG read and decode is the expensive
        // half and must not hold the map lock.
        let decoded = decode_png_rgba(file)?;
        tm_core::sync::lock(&self.caches)
            .decoded
            .insert(file.to_path_buf(), decoded.clone());
        Some(decoded)
    }
}

/// Entries of `dir`, sorted by path.
///
/// `read_dir` order is filesystem-defined (hash-ordered on ext4, name-ordered on
/// NTFS), so it must not decide which desktop file wins a shared executable
/// name — that made one app's icon depend on the machine or on a reinstall.
fn sorted_dir_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    sorted(paths)
}

/// `paths` sorted by name — the one rule that decides a shared icon name.
///
/// Kept apart from the directory walk so the order itself can be pinned: a walk
/// can only be tested through the filesystem, and a host whose `read_dir`
/// already lists directories in name order (NTFS collates names) makes a sorted
/// and an unsorted walk indistinguishable.
fn sorted(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths
}

/// First of `contexts` that holds `file_name`, in the order given.
///
/// A name that exists in several contexts of one size dir — `apps` and
/// `mimetypes` both ship generic ones — is decided by this order alone, so the
/// caller passes [`sorted_dir_paths`] and the winner never depends on the
/// filesystem's `read_dir` order. The scan is separate from that walk so a test
/// can hand it a fixed order.
fn first_in_contexts(contexts: &[PathBuf], file_name: &str) -> Option<PathBuf> {
    contexts
        .iter()
        .map(|context| context.join(file_name))
        .find(|candidate| candidate.is_file())
}

/// Scan `<data_dir>/applications` (recursively, one level of subdirs is
/// enough for KDE's legacy layout) for `.desktop` files and index their
/// `Icon=` by exe basename. Matching rules: the desktop file's own id, plus
/// every executable name its entry declares ([`DesktopEntry::executable_names`]).
/// Higher-precedence data dirs are scanned first and win.
fn build_desktop_index(data_dirs: &[PathBuf]) -> HashMap<String, String> {
    // `applications/` is depth 0; two levels below it cover legacy layouts
    // such as `applications/kde4/` or `applications/vendor/foo/`.
    const MAX_DEPTH: u8 = 2;
    let mut index = HashMap::new();
    for data_dir in data_dirs {
        let apps = data_dir.join("applications");
        let mut stack = vec![(apps, 0u8)];
        while let Some((dir, depth)) = stack.pop() {
            let mut subdirs: Vec<(PathBuf, u8)> = Vec::new();
            for p in sorted_dir_paths(&dir) {
                if p.is_dir() {
                    if depth < MAX_DEPTH {
                        subdirs.push((p, depth + 1));
                    }
                    continue;
                }
                let Some(file_name) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !file_name.ends_with(".desktop") {
                    continue;
                }
                let Some(entry) = desktop_file_entry(&p) else {
                    continue;
                };
                let Some(icon) = entry.icon.as_deref() else {
                    continue;
                };
                let stem = file_name.trim_end_matches(".desktop").to_ascii_lowercase();
                index.entry(stem).or_insert_with(|| icon.to_string());
                for exe_base in entry.executable_names() {
                    index.entry(exe_base).or_insert_with(|| icon.to_string());
                }
            }
            // Reversed onto the LIFO stack, so the alphabetically first
            // subdirectory is scanned first — the same rule that decides a
            // shared name inside one directory. A plain push order let the last
            // `applications/<vendor>/` subdirectory win instead.
            stack.extend(subdirs.into_iter().rev());
        }
    }
    index
}

/// Parse a `.desktop` file's `[Desktop Entry]` for `Icon=` (plain key first,
/// then any localized `Icon[..]=`).
fn desktop_file_icon(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let mut icon: Option<String> = None;
    let mut localized: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("Icon=") {
            icon.get_or_insert_with(|| v.trim().to_string());
        } else if line.starts_with("Icon[") {
            localized.get_or_insert_with(|| {
                line.split_once('=')
                    .map(|(_, v)| v.trim().to_string())
                    .unwrap_or_default()
            });
        }
    }
    icon.or(localized).filter(|s| !s.is_empty())
}

/// The parts of a `.desktop` file's `[Desktop Entry]` the icon index needs.
struct DesktopEntry {
    exec: Option<String>,
    try_exec: Option<String>,
    icon: Option<String>,
}

impl DesktopEntry {
    /// Executable basenames (lowercase) this entry can appear as in a process
    /// list.
    ///
    /// `Exec=` wins whenever it names the program itself. When it names a
    /// launcher instead — a Flatpak launcher, a shell, an interpreter — the
    /// launcher must not be relabelled with this app's icon, and the entry's
    /// own `TryExec=` (the spec's "this entry's program") is the only remaining
    /// honest name. A Flatpak launcher additionally names the command it starts
    /// *inside* the sandbox, because that is the executable the process list
    /// shows.
    fn executable_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let base = self
            .exec
            .as_deref()
            .and_then(exec_first_binary)
            .and_then(|binary| binary_basename(&binary));
        match base.as_deref() {
            Some(base) if base == FLATPAK_LAUNCHER => {
                names.extend(self.exec.as_deref().and_then(flatpak_command));
                push_try_exec(&mut names, self.try_exec.as_deref());
            }
            Some(base) if is_wrapper_binary(base) => {
                push_try_exec(&mut names, self.try_exec.as_deref());
            }
            Some(base) => names.push(base.to_string()),
            // No usable `Exec=` at all (DBus-activated entries declare it
            // anyway, but an empty or prefix-only one collapses here): the
            // entry's own `TryExec=` is still the program it announces.
            None => push_try_exec(&mut names, self.try_exec.as_deref()),
        }
        names
    }
}

/// `TryExec=` as a fallback executable name, used only where `Exec=` cannot
/// name the program: a program the entry merely launches through a launcher is
/// still that entry's program, but anything else would widen attribution past
/// what the entry declares.
fn push_try_exec(names: &mut Vec<String>, try_exec: Option<&str>) {
    if let Some(base) = try_exec.and_then(binary_basename)
        && !is_wrapper_binary(&base)
        && !names.contains(&base)
    {
        names.push(base);
    }
}

/// Parse a `.desktop` file's `[Desktop Entry]` for `Exec=`, `TryExec=` and
/// `Icon=`.
fn desktop_file_entry(path: &Path) -> Option<DesktopEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let mut entry = DesktopEntry {
        exec: None,
        try_exec: None,
        icon: None,
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("Exec=") {
            entry.exec.get_or_insert_with(|| v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("TryExec=") {
            entry.try_exec.get_or_insert_with(|| v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Icon=") {
            // An empty `Icon=` is not an icon. Keeping it would store "" in the
            // index and shadow another entry's real icon for the same
            // executable, because the first value inserted wins.
            let icon = v.trim();
            if !icon.is_empty() {
                entry.icon.get_or_insert_with(|| icon.to_string());
            }
        }
    }
    Some(entry)
}

/// Lowercase basename of an executable path or name, when it has one.
fn binary_basename(binary: &str) -> Option<String> {
    binary
        .rsplit('/')
        .next()
        .map(|b| b.trim_matches('"').to_ascii_lowercase())
        .filter(|b| !b.is_empty())
}

/// Whether `name` is an interpreter or launcher that only *starts* another
/// application: a listed wrapper, also with a version suffix (`python3.12`,
/// `nodejs18`) so versioned interpreters cannot claim an app's icon.
fn is_wrapper_binary(name: &str) -> bool {
    WRAPPER_BINARIES.contains(&name)
        || WRAPPER_BINARIES.iter().any(|wrapper| {
            name.strip_prefix(wrapper).is_some_and(|rest| {
                !rest.is_empty()
                    && rest.chars().all(|c| c.is_ascii_digit() || c == '.')
                    && rest.chars().any(|c| c.is_ascii_digit())
            })
        })
}

/// The executable a `flatpak run` command line starts inside its sandbox,
/// from `--command=<name>`: the sandboxed process is that binary, while
/// `flatpak` itself is only a launcher. Only a launcher command line is
/// scraped — a shell wrapper that happens to mention `--command=` in its
/// argument string must not name an executable.
fn flatpak_command(exec: &str) -> Option<String> {
    let tokens: Vec<&str> = exec.split_whitespace().skip(1).collect();
    for (index, token) in tokens.iter().enumerate() {
        let value = if let Some(v) = token.strip_prefix("--command=") {
            v
        } else if *token == "--command" {
            tokens.get(index + 1).copied().unwrap_or_default()
        } else {
            continue;
        };
        // `--command` without a usable value leaves the next token as an
        // option (`--arch=x86_64`), which is no executable name.
        if value.starts_with('-') {
            continue;
        }
        if let Some(base) = binary_basename(value)
            && !is_wrapper_binary(&base)
        {
            return Some(base);
        }
    }
    None
}

/// Whether `key` is the name half of an `env`-style `VAR=value` prefix before
/// an `Exec=` binary.
///
/// Only the name decides this: a shell variable name is ASCII alphanumerics and
/// underscores, so a value holding a path or a field code cannot turn the
/// assignment itself into the executable.
fn is_env_assignment_name(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// First meaningful token of a desktop `Exec=` line: strip field codes
/// (`%u`, `%F`, ...), then skip execution prefixes (`env`, `sudo`, `setsid`,
/// `nohup`, `nice` — also their absolute paths and flags) and `VAR=value`
/// assignments.
fn exec_first_binary(exec: &str) -> Option<String> {
    let mut tokens = exec.split_whitespace();
    let mut first = tokens.next()?;
    loop {
        let is_prefix =
            binary_basename(first).is_some_and(|base| EXEC_PREFIXES.contains(&base.as_str()));
        if is_prefix {
            first = tokens.next()?;
            // `env -i FOO=1 app`, `sudo -E app`: flags are not the binary, and
            // a flag that takes a value (`nice -n 10`, `sudo -u root`) carries
            // that value in the next token.
            while first.starts_with('-') {
                if PREFIX_FLAG_VALUES.contains(&first) {
                    tokens.next()?;
                }
                first = tokens.next()?;
            }
            continue;
        }
        if let Some((key, _)) = first.split_once('=')
            && is_env_assignment_name(key)
        {
            // env-style assignment prefix (only before the binary). The test is
            // on the assignment's name, not on its value: a value such as
            // `LD_LIBRARY_PATH=/opt/app/lib` contains slashes, and reading the
            // whole token as the executable made the entry claim an icon for a
            // binary named `lib`.
            first = tokens.next()?;
            continue;
        }
        break;
    }
    // Strip trailing field codes like "foo %u" -> handled by tokenization;
    // a token that is only a field code cannot be the binary.
    let token = first.trim_matches('"');
    (!token.is_empty() && !token.starts_with('%')).then(|| token.to_string())
}

/// `Inherits=` list of a theme, read from its `index.theme` in any icon dir.
fn theme_inherits(icon_dirs: &[PathBuf], theme: &str) -> Vec<String> {
    for dir in icon_dirs {
        let index = dir.join(theme).join("index.theme");
        let Ok(text) = std::fs::read_to_string(&index) else {
            continue;
        };
        let mut in_theme = false;
        for raw in text.lines() {
            let line = raw.trim();
            if line.starts_with('[') {
                in_theme = line == "[Icon Theme]";
                continue;
            }
            if in_theme && let Some(v) = line.strip_prefix("Inherits=") {
                return v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
    }
    Vec::new()
}

/// The themes to search, in order: the configured theme (or the spec's
/// [`DEFAULT_THEME`] when nothing names one), that theme's `Inherits` expanded
/// depth-first, then [`FALLBACK_THEME`] last.
///
/// The chain has to be transitive: real desktops nest it two or three deep
/// (`default` → `Adwaita` → `AdwaitaLegacy` → `hicolor` on a stock GNOME box,
/// `Breeze-Dark` → `Breeze` → `hicolor` on Plasma), and stopping after one
/// level made those themes' icons invisible. `visited` also makes a malformed
/// `Inherits` cycle terminate.
fn theme_chain(icon_dirs: &[PathBuf], current: Option<&str>) -> Vec<String> {
    let start = current.unwrap_or(DEFAULT_THEME).to_string();
    let mut chain: Vec<String> = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![start];
    while let Some(theme) = stack.pop() {
        // A theme name is one path component, and this one is also joined onto
        // every icon dir. A malformed `Inherits=` value must not redirect the
        // search outside them.
        if !is_themed_icon_name(&theme) {
            continue;
        }
        if !visited.insert(theme.clone()) {
            continue;
        }
        // Reversed so the first `Inherits` entry is expanded first: a theme's
        // own parents outrank the mandatory hicolor fallback.
        for parent in theme_inherits(icon_dirs, &theme).into_iter().rev() {
            stack.push(parent);
        }
        chain.push(theme);
    }
    if visited.insert(FALLBACK_THEME.to_string()) {
        chain.push(FALLBACK_THEME.to_string());
    }
    chain
}

/// Whether `name` is a themed icon name or theme name: exactly one path
/// component.
///
/// `Icon=` values and `Inherits=` entries end up in a `Path::join` against the
/// icon dirs, so a name carrying a separator or a `.`/`..` component would read
/// outside the icon dirs the spec says to search. Such a name is not a themed
/// name, and treating it as one would report an unrelated file as the icon.
fn is_themed_icon_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

/// The file name to look for inside a theme: `<name>.png`, unless the icon
/// name already carries the extension (some desktop files name a file instead
/// of a themed icon, and `<name>.png.png` never exists).
fn png_file_name(name: &str) -> String {
    if name.ends_with(".png") {
        name.to_string()
    } else {
        format!("{name}.png")
    }
}

/// One candidate size directory of a theme.
struct SizeDir {
    /// `Size × Scale`: the number of pixels its files hold.
    pixel_size: u32,
    /// The declared `Size=` itself, i.e. the size the directory is native at.
    size: u32,
    path: PathBuf,
}

/// Candidate size dirs of `theme_dir`, best first: the `index.theme` directory
/// list when it declares one, else the `<size>` subdirectory names.
///
/// Pixel size closest to the target, then the size closest to it, then the
/// larger pixel size, then the path: a `read_dir` order must never decide
/// between equal candidates. The nominal size breaks a pixel-size tie —
/// `16x16@2` and `32x32` both hold 32 px files, but only `32x32` is native at
/// that size. When two directories are equally far from the target the larger
/// one wins, so a 32 px target takes a native 48 px asset (scaled down) over a
/// 16 px one (scaled up).
fn size_dirs_of_theme(theme_dir: &Path) -> Vec<SizeDir> {
    let mut dirs: Vec<SizeDir> = Vec::new();
    if let Some(declared) = theme_directories(theme_dir) {
        for d in declared {
            dirs.push(SizeDir {
                pixel_size: d.pixel_size,
                size: d.size,
                path: theme_dir.join(d.subdir),
            });
        }
    } else if let Ok(sizes) = std::fs::read_dir(theme_dir) {
        // No index.theme: scan one level of `<size>` dirs, then `<context>`.
        for size_entry in sizes.flatten() {
            let Ok(meta) = size_entry.metadata() else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let Some(size_name) = size_entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            dirs.push(SizeDir {
                pixel_size: dir_size_from_name(&size_name),
                size: nominal_size_from_name(&size_name),
                path: size_entry.path(),
            });
        }
    }
    dirs.sort_by(|a, b| {
        a.pixel_size
            .abs_diff(TARGET_SIZE)
            .cmp(&b.pixel_size.abs_diff(TARGET_SIZE))
            .then_with(|| {
                a.size
                    .abs_diff(TARGET_SIZE)
                    .cmp(&b.size.abs_diff(TARGET_SIZE))
            })
            .then_with(|| b.pixel_size.cmp(&a.pixel_size))
            .then_with(|| a.path.cmp(&b.path))
    });
    dirs
}

/// One directory declared by a theme's `index.theme`.
struct ThemeDir {
    /// `Size × Scale`: the number of pixels its files hold.
    pixel_size: u32,
    /// The declared `Size=` itself, i.e. the size the directory is native at.
    size: u32,
    subdir: String,
}

/// Pixel size encoded in a size-directory name: `16x16` → 16, `48x48/apps` →
/// 48, `16x16@2` → 32 (an `@N` directory holds `N ×` the pixels), and anything
/// unparseable → 0, i.e. ranked last.
fn dir_size_from_name(size_name: &str) -> u32 {
    nominal_size_from_name(size_name).saturating_mul(name_scale(size_name))
}

/// Nominal size encoded in a size-directory name (`16x16@2/apps` → 16), i.e. the
/// size the directory is native at.
fn nominal_size_from_name(size_name: &str) -> u32 {
    let base = match size_name.split_once('@') {
        Some((base, _)) => base,
        None => size_name,
    };
    base.split('x')
        .next()
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0)
}

/// Pixel multiplier encoded in a theme directory name (`16x16@2/apps` → 2), for
/// themes that list their hidpi dirs in `ScaledDirectories=` without a matching
/// `Scale=` key.
fn name_scale(dir: &str) -> u32 {
    dir.split_once('@')
        .and_then(|(_, rest)| rest.split('/').next())
        .and_then(|rest| rest.parse::<u32>().ok())
        .filter(|scale| *scale > 0)
        .unwrap_or(1)
}

/// Comma-separated `index.theme` list value, trimmed and without empty items.
fn index_theme_list(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
}

/// The directories a theme's `index.theme` declares: the `Directories=` and
/// `ScaledDirectories=` lists of `[Icon Theme]`, each with the `Size=` and
/// `Scale=` of its own section.
///
/// The pixel size is `Size × Scale`: a `16x16@2` directory holds 32 px files
/// (`Size=16 Scale=2`), and ranking on `Size` alone preferred a 16 px asset over
/// that sharper one. `ScaledDirectories=` is where Adwaita/Breeze-style themes
/// declare their hidpi dirs, and a missing `Scale=` is taken from the `@N` in
/// the directory name. `Size` is kept next to the pixel size so a same-pixel
/// tie can prefer the directory that is native at that size.
fn theme_directories(theme_dir: &Path) -> Option<Vec<ThemeDir>> {
    let text = std::fs::read_to_string(theme_dir.join("index.theme")).ok()?;
    let mut dirs: Vec<String> = Vec::new();
    let mut scaled_dirs: Vec<String> = Vec::new();
    let mut sizes: HashMap<String, u32> = HashMap::new();
    let mut scales: HashMap<String, u32> = HashMap::new();
    let mut in_theme = false;
    let mut section: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = Some(line[1..line.len() - 1].to_string());
            in_theme = section.as_deref() == Some("Icon Theme");
            continue;
        }
        if in_theme && let Some(v) = line.strip_prefix("Directories=") {
            dirs.extend(index_theme_list(v));
        }
        if in_theme && let Some(v) = line.strip_prefix("ScaledDirectories=") {
            scaled_dirs.extend(index_theme_list(v));
        }
        if !in_theme
            && let (Some(sec), Some(v)) = (&section, line.strip_prefix("Size="))
            && let Ok(n) = v.trim().parse::<u32>()
        {
            sizes.insert(sec.clone(), n);
        }
        if !in_theme
            && let (Some(sec), Some(v)) = (&section, line.strip_prefix("Scale="))
            && let Ok(n) = v.trim().parse::<u32>()
            && n > 0
        {
            // Folded in below so one lookup covers `Size=` and `Scale=`
            // regardless of the order they appear in the section.
            scales.insert(sec.clone(), n);
        }
    }
    if dirs.is_empty() && scaled_dirs.is_empty() {
        return None;
    }
    dirs.extend(scaled_dirs);
    Some(
        dirs.into_iter()
            // A theme lists its directories relative to itself. An absolute
            // entry, or one that climbs with `..`, would point the size-dir
            // walk — and the `Path::join` of every icon file under it — outside
            // the theme and outside the icon dirs, so it is not a directory of
            // this theme.
            .filter(|d| is_theme_subdirectory(d))
            .map(|d| {
                let size = sizes.get(&d).copied().unwrap_or(0);
                let scale = scales.get(&d).copied().unwrap_or_else(|| name_scale(&d));
                ThemeDir {
                    pixel_size: size.saturating_mul(scale),
                    size,
                    subdir: d,
                }
            })
            .collect(),
    )
}

/// Whether a theme's declared directory stays inside the theme: a relative path
/// whose every component is a normal name.
fn is_theme_subdirectory(subdir: &str) -> bool {
    !subdir.is_empty()
        && Path::new(subdir)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

/// Decode a PNG file to straight-alpha RGBA, box-downsampling anything larger
/// than [`MAX_ICON_EDGE`] on a side.
pub fn decode_png_rgba(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    // normalize_to_color8 = EXPAND | STRIP_16: palettes and low-bit
    // grayscale become 8-bit RGB, tRNS becomes an alpha channel, and 16-bit
    // samples are stripped to 8 bits. Without STRIP_16 a legal 16-bit theme
    // icon produces two bytes per channel and the GUI's texture upload
    // asserts on the length (release builds abort).
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    decoder.set_limits(png::Limits {
        bytes: MAX_ICON_DECODE_BYTES,
    });
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || w > MAX_ICON_SOURCE_EDGE || h > MAX_ICON_SOURCE_EDGE {
        return None;
    }
    let out_size = reader.output_buffer_size()?;
    let mut buf = vec![0u8; out_size];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let px = &buf[..out_size];
    let src = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(px.len() / 3 * 4);
            for c in px.as_chunks::<3>().0 {
                out.extend_from_slice(c);
                out.push(255);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(px.len() * 4);
            for &g in px {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(px.len() / 2 * 4);
            for c in px.as_chunks::<2>().0 {
                out.extend_from_slice(&[c[0], c[0], c[0], c[1]]);
            }
            out
        }
        _ => return None,
    };
    // Every branch above claims 8-bit RGBA. Reject anything else instead of
    // handing `ColorImage::from_rgba_unmultiplied` a buffer whose length
    // trips its assert on the UI thread.
    let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if src.len() != expected {
        return None;
    }
    Some(if w.max(h) > MAX_ICON_EDGE {
        downscale_rgba(&src, w, h)
    } else {
        (w, h, src)
    })
}

/// Box-filter downscale so the longest edge is at most [`MAX_ICON_EDGE`].
///
/// RGB is averaged weighted by alpha and divided by the summed alpha, so a
/// transparent border (usually RGB 0) cannot darken the edge pixels into a
/// black halo around a large theme icon.
fn downscale_rgba(rgba: &[u8], w: u32, h: u32) -> (u32, u32, Vec<u8>) {
    let scale = (w.max(h) as f32 / MAX_ICON_EDGE as f32).ceil();
    let (nw, nh) = (
        (w as f32 / scale).round().max(1.0) as u32,
        (h as f32 / scale).round().max(1.0) as u32,
    );
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            // Source box covered by this output pixel.
            let x0 = (x as f32 * w as f32 / nw as f32) as u32;
            let x1 = (((x + 1) as f32 * w as f32 / nw as f32) as u32)
                .max(x0 + 1)
                .min(w);
            let y0 = (y as f32 * h as f32 / nh as f32) as u32;
            let y1 = (((y + 1) as f32 * h as f32 / nh as f32) as u32)
                .max(y0 + 1)
                .min(h);
            // [alpha, r*alpha, g*alpha, b*alpha]
            let mut acc = [0u64; 4];
            let mut count = 0u64;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * w + sx) * 4) as usize;
                    let a = u64::from(rgba[i + 3]);
                    acc[0] += a;
                    acc[1] += u64::from(rgba[i]) * a;
                    acc[2] += u64::from(rgba[i + 1]) * a;
                    acc[3] += u64::from(rgba[i + 2]) * a;
                    count += 1;
                }
            }
            let o = ((y * nw + x) * 4) as usize;
            let alpha_sum = acc[0];
            if let (Some(r), Some(g), Some(b)) = (
                acc[1].checked_div(alpha_sum),
                acc[2].checked_div(alpha_sum),
                acc[3].checked_div(alpha_sum),
            ) {
                out[o] = r as u8;
                out[o + 1] = g as u8;
                out[o + 2] = b as u8;
            }
            out[o + 3] = (alpha_sum / count.max(1)) as u8;
        }
    }
    (nw, nh, out)
}

/// Linux backend entry point: env-based shared resolver. The desktop index
/// is built once process-wide (first call scans the applications dirs).
#[cfg(target_os = "linux")]
pub fn process_icon_rgba(exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
    IconResolver::shared().icon_rgba_for_exe(exe_path)
}

/// Linux backend entry point for the theme's default application icon.
#[cfg(target_os = "linux")]
pub fn default_app_icon_rgba() -> Option<(u32, u32, Vec<u8>)> {
    IconResolver::shared().default_app_icon_rgba()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn xsettings_icon_theme_is_read_in_both_byte_orders() {
        let name = b"Net/IconThemeName";
        for big_endian in [false, true] {
            let u16_bytes = |value: u16| {
                if big_endian {
                    value.to_be_bytes()
                } else {
                    value.to_le_bytes()
                }
            };
            let u32_bytes = |value: u32| {
                if big_endian {
                    value.to_be_bytes()
                } else {
                    value.to_le_bytes()
                }
            };
            let mut property = vec![u8::from(big_endian), 0, 0, 0];
            property.extend(u32_bytes(1));
            property.extend(u32_bytes(1));
            property.extend([1, 0]);
            property.extend(u16_bytes(name.len() as u16));
            property.extend(name);
            property.resize(property.len() + (4 - name.len() % 4) % 4, 0);
            property.extend(u32_bytes(1));
            property.extend(u32_bytes(6));
            property.extend(b"Breeze\0\0");
            assert_eq!(parse_xsettings_icon_theme(&property), Some("Breeze"));
            property.pop();
            assert_eq!(parse_xsettings_icon_theme(&property), None);
        }
    }

    fn write_png(path: &Path, w: u32, h: u32, rgba: [u8; 4]) {
        write_png_pixels(path, w, h, &rgba.repeat((w * h) as usize));
    }

    fn write_png_pixels(path: &Path, w: u32, h: u32, pixels: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let file = std::fs::File::create(path).unwrap();
        let mut enc = png::Encoder::new(file, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        assert_eq!(pixels.len(), (w * h * 4) as usize);
        writer.write_image_data(pixels).unwrap();
        writer.finish().unwrap();
    }

    /// A 16-bit RGBA PNG: legal, common in some third-party themes, and the
    /// input that used to trip `ColorImage::from_rgba_unmultiplied`'s length
    /// assert (release builds abort).
    fn write_png_16(path: &Path, w: u32, h: u32, rgba: [u16; 4]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let file = std::fs::File::create(path).unwrap();
        let mut enc = png::Encoder::new(file, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Sixteen);
        let mut writer = enc.write_header().unwrap();
        let mut data = Vec::with_capacity((w * h * 8) as usize);
        for _ in 0..(w * h) {
            for channel in rgba {
                data.extend_from_slice(&channel.to_be_bytes());
            }
        }
        writer.write_image_data(&data).unwrap();
        writer.finish().unwrap();
    }

    fn write_desktop(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    struct Sandbox {
        root: PathBuf,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("tm-xdg-test-{}-{}", name, std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn lookup(&self) -> IconLookup {
            IconLookup {
                data_dirs: vec![self.root.join("share")],
                pixmaps: vec![self.root.join("pixmaps")],
                theme: Some("custom".into()),
                home_icons: Some(self.root.join("home").join(".icons")),
            }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const HICOLOR_INDEX: &str = "[Icon Theme]\nName=Hicolor\nDirectories=48x48/apps,32x32/apps\n\n[48x48/apps]\nSize=48\nContext=Applications\nType=Threshold\n\n[32x32/apps]\nSize=32\nContext=Applications\nType=Threshold\n";

    #[test]
    fn desktop_exec_match_resolves_theme_icon() {
        let sb = Sandbox::new("exec-match");
        write_desktop(
            &sb.root.join("share/applications/testapp.desktop"),
            "[Desktop Entry]\nName=Test App\nExec=/usr/bin/testapp %u\nIcon=testapp\n",
        );
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(
            &sb.root.join("share/icons/hicolor/48x48/apps/testapp.png"),
            48,
            48,
            [200, 10, 10, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let (w, h, rgba) = r.icon_rgba_for_exe("/usr/bin/testapp").expect("icon");
        assert_eq!((w, h), (48, 48));
        assert_eq!(&rgba[..4], &[200, 10, 10, 255]);
    }

    #[test]
    fn bare_exe_basename_falls_back_to_themed_icon_name() {
        let sb = Sandbox::new("basename-fallback");
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(
            &sb.root.join("share/icons/hicolor/48x48/apps/mybin.png"),
            48,
            48,
            [1, 2, 3, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let (w, h, rgba) = r.icon_rgba_for_exe("/opt/whatever/mybin").expect("icon");
        assert_eq!((w, h), (48, 48));
        assert_eq!(&rgba[..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn desktop_file_input_uses_its_own_icon_entry() {
        let sb = Sandbox::new("desktop-input");
        let icon_abs = sb.root.join("abs/path/foo.png");
        write_png(&icon_abs, 32, 32, [9, 9, 9, 255]);
        write_desktop(
            &sb.root.join("autostart/foo.desktop"),
            &format!(
                "[Desktop Entry]\nName=Foo\nExec=/usr/bin/foo\nIcon={}\n",
                icon_abs.display()
            ),
        );
        let r = IconResolver::new(sb.lookup());
        let (w, h, _) = r
            .icon_rgba_for_exe(sb.root.join("autostart/foo.desktop").to_str().unwrap())
            .expect("icon");
        assert_eq!((w, h), (32, 32));
    }

    #[test]
    fn theme_inherits_chain_reaches_hicolor() {
        let sb = Sandbox::new("inherits");
        // Current theme "custom" has no icons itself but inherits hicolor.
        write_desktop(
            &sb.root.join("share/icons/custom/index.theme"),
            "[Icon Theme]\nName=Custom\nInherits=hicolor\nDirectories=48x48/apps\n\n[48x48/apps]\nSize=48\n",
        );
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(
            &sb.root.join("share/icons/hicolor/32x32/apps/inherited.png"),
            32,
            32,
            [7, 7, 7, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let name = r.icon_name_for("/usr/bin/inherited").unwrap();
        assert_eq!(name, "inherited");
        let f = r.find_icon_file(&name).expect("via inherits");
        assert!(f.ends_with("32x32/apps/inherited.png"), "{f:?}");
    }

    #[test]
    fn closest_size_to_32_is_preferred() {
        let sb = Sandbox::new("sizes");
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(
            &sb.root.join("share/icons/hicolor/48x48/apps/sized.png"),
            48,
            48,
            [1, 1, 1, 255],
        );
        write_png(
            &sb.root.join("share/icons/hicolor/32x32/apps/sized.png"),
            32,
            32,
            [2, 2, 2, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let f = r.find_icon_file("sized").expect("found");
        assert!(f.ends_with("32x32/apps/sized.png"), "{f:?}");
    }

    #[test]
    fn pixmaps_dir_is_the_last_resort() {
        let sb = Sandbox::new("pixmaps");
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(&sb.root.join("pixmaps/legacy.png"), 24, 24, [5, 5, 5, 255]);
        let r = IconResolver::new(sb.lookup());
        let f = r.find_icon_file("legacy").expect("pixmaps fallback");
        assert!(f.starts_with(sb.root.join("pixmaps")), "{f:?}");
    }

    #[test]
    fn env_style_exec_prefix_is_skipped() {
        let sb = Sandbox::new("env-exec");
        write_desktop(
            &sb.root.join("share/applications/thing.desktop"),
            "[Desktop Entry]\nExec=env QT_SCALE_FACTOR=2 /usr/bin/thing %f\nIcon=thingicon\n",
        );
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(
            &sb.root.join("share/icons/hicolor/48x48/apps/thingicon.png"),
            48,
            48,
            [3, 3, 3, 255],
        );
        let r = IconResolver::new(sb.lookup());
        assert_eq!(
            r.icon_name_for("/usr/bin/thing").as_deref(),
            Some("thingicon")
        );
        assert!(r.icon_rgba_for_exe("/usr/bin/thing").is_some());
    }

    #[test]
    fn desktop_file_stem_matches_exe_basename() {
        let sb = Sandbox::new("stem-match");
        // VS Code style: desktop id "code" but binary path .../bin/code.
        write_desktop(
            &sb.root.join("share/applications/code.desktop"),
            "[Desktop Entry]\nExec=/usr/share/code/code %F\nIcon=vscode\n",
        );
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(
            &sb.root.join("share/icons/hicolor/48x48/apps/vscode.png"),
            48,
            48,
            [4, 4, 4, 255],
        );
        let r = IconResolver::new(sb.lookup());
        // exe basename "code" matches the desktop-file stem.
        assert_eq!(
            r.icon_name_for("/usr/share/code/bin/code").as_deref(),
            Some("vscode")
        );
    }

    #[test]
    fn large_icons_are_downscaled_to_the_cache_cap() {
        let sb = Sandbox::new("downscale");
        write_png(&sb.root.join("big.png"), 256, 128, [10, 20, 30, 255]);
        let (w, h, rgba) = decode_png_rgba(&sb.root.join("big.png")).unwrap();
        assert!(w <= MAX_ICON_EDGE && h <= MAX_ICON_EDGE, "{w}x{h}");
        assert_eq!(w, 64);
        assert_eq!(h, 32);
        assert_eq!(&rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn missing_icon_is_none_not_a_fabricated_pixel() {
        let sb = Sandbox::new("missing");
        let r = IconResolver::new(sb.lookup());
        assert!(
            r.icon_rgba_for_exe("/usr/bin/definitely-not-installed-xyz")
                .is_none()
        );
    }

    /// An executable with no icon of its own still gets the theme's standard
    /// application icon, so a row never falls back to a drawn placeholder.
    #[test]
    fn default_app_icon_is_the_themes_executable_icon() {
        let sb = Sandbox::new("default-app");
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(
            &sb.root
                .join("share/icons/hicolor/32x32/apps/application-x-executable.png"),
            32,
            32,
            [7, 8, 9, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let (w, h, rgba) = r.default_app_icon_rgba().expect("executable icon");
        assert_eq!((w, h), (32, 32));
        assert_eq!(&rgba[..4], &[7, 8, 9, 255]);
        // And the row lookup itself stays strict: the app layer decides to
        // fall back, so a missing app icon must not masquerade as a real one.
        assert!(
            r.icon_rgba_for_exe("/usr/bin/definitely-not-installed-xyz")
                .is_none()
        );
    }

    /// Without any theme icon there is nothing to paint; the caller draws
    /// nothing rather than inventing an icon.
    #[test]
    fn default_app_icon_is_none_when_the_theme_has_none() {
        let sb = Sandbox::new("default-app-missing");
        let r = IconResolver::new(sb.lookup());
        assert!(r.default_app_icon_rgba().is_none());
    }

    /// Regression: a legal 16-bit theme PNG must decode to 8-bit RGBA, not
    /// hand 8 bytes per pixel to the GUI's texture upload.
    #[test]
    fn sixteen_bit_png_is_stripped_to_eight_bit_rgba() {
        let sb = Sandbox::new("png16");
        write_png_16(
            &sb.root.join("icon16.png"),
            32,
            32,
            [0xABCD, 0x1234, 0x5678, 0xFFFF],
        );
        let (w, h, rgba) = decode_png_rgba(&sb.root.join("icon16.png")).expect("decode");
        assert_eq!((w, h), (32, 32));
        assert_eq!(rgba.len(), 32 * 32 * 4);
    }

    /// A large 16-bit PNG is downsampled, never left at 8 bytes/pixel.
    #[test]
    fn large_sixteen_bit_png_is_downsampled_to_eight_bit_rgba() {
        let sb = Sandbox::new("png16-large");
        write_png_16(&sb.root.join("big16.png"), 128, 64, [0xFFFF, 0, 0, 0xFFFF]);
        let (w, h, rgba) = decode_png_rgba(&sb.root.join("big16.png")).expect("decode");
        assert_eq!((w, h), (64, 32));
        assert_eq!(rgba.len(), 64 * 32 * 4);
    }

    /// Transparent pixels are RGB 0 in most theme PNGs; averaging straight
    /// RGB darkens the edge into a black halo. The box filter must weight
    /// RGB by alpha.
    #[test]
    fn downscale_does_not_darken_transparent_edges() {
        let sb = Sandbox::new("downscale-alpha");
        let (w, h) = (128, 128);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                if x % 2 == 0 {
                    // Opaque red in every other column.
                    pixels[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
                }
            }
        }
        write_png_pixels(&sb.root.join("edge.png"), w, h, &pixels);
        let (ow, oh, rgba) = decode_png_rgba(&sb.root.join("edge.png")).expect("decode");
        assert_eq!((ow, oh), (64, 64));
        let (r, g, b, a) = (rgba[0], rgba[1], rgba[2], rgba[3]);
        assert_eq!(a, 127, "half the source box is transparent");
        assert!(
            r > 250 && g < 5 && b < 5,
            "opaque red must stay red, not be averaged with transparent black: {r},{g},{b}"
        );
    }

    /// Three sibling subdirectories must all be scanned; the old DFS-wide
    /// two-step counter stopped after whichever two `read_dir` returned
    /// first, and the third app lost its theme icon.
    #[test]
    fn desktop_index_scans_every_subdirectory() {
        let sb = Sandbox::new("index-depth");
        for name in ["alpha", "beta", "gamma"] {
            write_desktop(
                &sb.root
                    .join(format!("share/applications/{name}/{name}.desktop")),
                &format!("[Desktop Entry]\nName={name}\nExec=/usr/bin/{name}\nIcon={name}-icon\n"),
            );
        }
        let index = build_desktop_index(&[sb.root.join("share")]);
        for name in ["alpha", "beta", "gamma"] {
            let expected = format!("{name}-icon");
            assert_eq!(
                index.get(name).map(String::as_str),
                Some(expected.as_str()),
                "{name} was not indexed"
            );
        }
    }

    /// A desktop entry whose Exec is a shared interpreter must not claim
    /// that interpreter's process icon.
    #[test]
    fn desktop_index_skips_shared_interpreters() {
        let sb = Sandbox::new("index-wrapper");
        write_desktop(
            &sb.root.join("share/applications/pyapp.desktop"),
            "[Desktop Entry]\nName=PyApp\nExec=python3 /opt/pyapp/main.py\nIcon=pyapp\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert_eq!(index.get("pyapp").map(String::as_str), Some("pyapp"));
        assert!(
            !index.contains_key("python3"),
            "python3 is a wrapper, not PyApp's image"
        );
    }

    /// `read_dir` order is filesystem-defined; when two desktop files claim the
    /// same executable name, the alphabetically first must win, so the icon
    /// cannot differ between machines or reinstallations.
    #[test]
    fn desktop_index_is_deterministic_for_shared_exec_names() {
        let sb = Sandbox::new("index-order");
        for (file, icon) in [
            ("zzz.desktop", "second-icon"),
            ("aaa.desktop", "first-icon"),
        ] {
            write_desktop(
                &sb.root.join(format!("share/applications/{file}")),
                &format!("[Desktop Entry]\nName=Shared\nExec=/usr/bin/sharedapp\nIcon={icon}\n"),
            );
        }
        // The invariant itself, so it holds on NTFS (name-ordered enumeration)
        // exactly as on a hash-ordered filesystem: the candidate list is sorted
        // and the tie-break therefore defined.
        let paths = sorted_dir_paths(&sb.root.join("share/applications"));
        assert_eq!(paths.len(), 2);
        assert!(paths.windows(2).all(|pair| pair[0] < pair[1]), "{paths:?}");
        assert!(paths[0].ends_with("aaa.desktop"), "{paths:?}");
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert_eq!(
            index.get("sharedapp").map(String::as_str),
            Some("first-icon")
        );
    }

    /// A versioned interpreter (`python3.12`) is the same wrapper as the
    /// unversioned one; indexing it made a Python app's icon follow the
    /// interpreter's process.
    #[test]
    fn desktop_index_skips_versioned_interpreters() {
        let sb = Sandbox::new("index-versioned-wrapper");
        write_desktop(
            &sb.root.join("share/applications/pyapp.desktop"),
            "[Desktop Entry]\nName=PyApp\nExec=python3.12 /opt/pyapp/main.py\nIcon=pyapp\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert!(
            !index.contains_key("python3.12"),
            "python3.12 is a wrapper, not PyApp's image"
        );
        assert_eq!(index.get("pyapp").map(String::as_str), Some("pyapp"));
    }

    /// Wrapper detection is a whole-name match plus a version suffix, so an
    /// application whose name merely starts with a wrapper's stays its own
    /// image.
    #[test]
    fn wrapper_detection_keeps_similarly_named_apps() {
        for wrapper in [
            "python",
            "python3",
            "python3.12",
            "nodejs18",
            "sh",
            "flatpak",
            "env",
            "sudo",
            "setsid",
            "nohup",
            "nice",
        ] {
            assert!(is_wrapper_binary(wrapper), "{wrapper} is a wrapper");
        }
        for app in [
            "shotcut",
            "python-tool",
            "code",
            "firefox",
            "gnome-terminal",
        ] {
            assert!(!is_wrapper_binary(app), "{app} is an app");
        }
    }

    /// A Flatpak entry's `Exec=` starts `flatpak`; the binary the process list
    /// shows is the one the launcher starts inside the sandbox.
    #[test]
    fn flatpak_command_names_the_sandboxed_executable() {
        let sb = Sandbox::new("flatpak");
        write_desktop(
            &sb.root
                .join("share/applications/org.gnome.Calculator.desktop"),
            "[Desktop Entry]\nName=Calculator\n\
             Exec=/usr/bin/flatpak run --branch=stable --arch=x86_64 \
             --command=gnome-calculator --file-forwarding org.gnome.Calculator %U\n\
             Icon=org.gnome.Calculator\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert_eq!(
            index.get("gnome-calculator").map(String::as_str),
            Some("org.gnome.Calculator")
        );
        assert!(
            !index.contains_key("flatpak"),
            "flatpak is only a launcher, not the app's image"
        );
    }

    /// `TryExec=` is only a fallback: an entry whose `Exec=` names its program
    /// must not additionally claim the `TryExec` binary, which belongs to a
    /// different process.
    #[test]
    fn try_exec_does_not_widen_a_trustworthy_exec() {
        let sb = Sandbox::new("try-exec-honest");
        write_desktop(
            &sb.root.join("share/applications/net.example.Foo.desktop"),
            "[Desktop Entry]\nName=Foo\nExec=/usr/bin/foo\nTryExec=/usr/bin/bar\nIcon=foo-icon\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert_eq!(index.get("foo").map(String::as_str), Some("foo-icon"));
        assert!(
            !index.contains_key("bar"),
            "bar is a different program, not Foo's image"
        );
    }

    /// An entry whose `Exec=` names nothing at all — DBus-activated service
    /// entries, an empty value, a command line of prefixes only — still
    /// announces its program through `TryExec=`.
    #[test]
    fn try_exec_names_an_entry_without_a_usable_exec() {
        let sb = Sandbox::new("try-exec-no-exec");
        write_desktop(
            &sb.root
                .join("share/applications/net.example.Service.desktop"),
            "[Desktop Entry]\nName=Service\nDBusActivatable=true\n\
             TryExec=/usr/bin/service\nIcon=service-icon\n",
        );
        write_desktop(
            &sb.root
                .join("share/applications/net.example.Prefixed.desktop"),
            "[Desktop Entry]\nName=Prefixed\nExec=env\nTryExec=/usr/bin/prefixed\n\
             Icon=prefixed-icon\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert_eq!(
            index.get("service").map(String::as_str),
            Some("service-icon")
        );
        assert_eq!(
            index.get("prefixed").map(String::as_str),
            Some("prefixed-icon")
        );
        assert!(!index.contains_key("env"));
    }

    /// Prefix flags that take a value must not leave the value behind as a
    /// binary name: `nice -n 10 app` and `sudo -u root app` run `app`.
    #[test]
    fn prefix_flag_values_are_not_executable_names() {
        let sb = Sandbox::new("prefix-flags");
        write_desktop(
            &sb.root.join("share/applications/net.example.Niced.desktop"),
            "[Desktop Entry]\nName=Niced\nExec=nice -n 10 /opt/niced/bin/niced %U\n\
             Icon=niced-icon\n",
        );
        write_desktop(
            &sb.root
                .join("share/applications/net.example.Rooted.desktop"),
            "[Desktop Entry]\nName=Rooted\nExec=/usr/bin/sudo -u root /opt/rooted/bin/rooted\n\
             Icon=rooted-icon\n",
        );
        write_desktop(
            &sb.root
                .join("share/applications/net.example.Envved.desktop"),
            "[Desktop Entry]\nName=Envved\nExec=env -u FOO /opt/envved/bin/envved\n\
             Icon=envved-icon\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert_eq!(index.get("niced").map(String::as_str), Some("niced-icon"));
        assert_eq!(index.get("rooted").map(String::as_str), Some("rooted-icon"));
        assert_eq!(index.get("envved").map(String::as_str), Some("envved-icon"));
        assert!(!index.contains_key("10"), "nice level is not a binary");
        assert!(!index.contains_key("root"), "sudo user is not a binary");
        assert!(!index.contains_key("foo"), "env var is not a binary");
    }

    /// An absolute `/usr/bin/env` prefix is the same execution prefix as a bare
    /// `env`: it must be skipped, not indexed as the shared `env` binary.
    #[test]
    fn absolute_env_prefix_is_skipped_not_indexed() {
        let sb = Sandbox::new("env-prefix");
        write_desktop(
            &sb.root
                .join("share/applications/net.example.Wrapped.desktop"),
            "[Desktop Entry]\nName=Wrapped\nExec=/usr/bin/env FOO=1 /opt/wrapped/bin/wrapped %U\n\
             Icon=wrapped-icon\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert_eq!(
            index.get("wrapped").map(String::as_str),
            Some("wrapped-icon")
        );
        assert!(
            !index.contains_key("env"),
            "env only starts the app and must not collect its icon"
        );
    }

    /// Only a Flatpak launcher is scraped for `--command=`: a shell wrapper that
    /// merely mentions that switch in its argument string must not name an
    /// executable (`sh -c "… --command=spotify"` is not a Spotify entry).
    #[test]
    fn flatpak_command_is_only_read_from_the_launcher() {
        let sb = Sandbox::new("flatpak-negative");
        write_desktop(
            &sb.root.join("share/applications/net.example.Guise.desktop"),
            "[Desktop Entry]\nName=Guise\n\
             Exec=sh -c \"/opt/guise/bin/guise --command=spotify %f\"\nIcon=guise-icon\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert!(
            !index.contains_key("spotify"),
            "spotify is another application's image"
        );
        assert!(!index.contains_key("sh"));
    }

    /// A `--command` whose value is another option is not an executable name.
    #[test]
    fn flatpak_command_ignores_option_values() {
        assert_eq!(
            flatpak_command("flatpak run --command=app"),
            Some("app".into())
        );
        assert_eq!(
            flatpak_command("flatpak run --command app"),
            Some("app".into())
        );
        assert_eq!(
            flatpak_command("flatpak run --command --arch=x86_64 org.example.App"),
            None
        );
        assert_eq!(flatpak_command("flatpak run org.example.App"), None);
    }

    /// `TryExec=` names the program an entry needs, so an entry whose `Exec=`
    /// hides the binary behind a shell still maps to that binary.
    #[test]
    fn try_exec_names_the_entry_when_exec_hides_the_binary() {
        let sb = Sandbox::new("try-exec");
        write_desktop(
            &sb.root
                .join("share/applications/net.example.Bundled.desktop"),
            "[Desktop Entry]\nName=Bundled\nExec=sh -c \"/opt/bundled/bin/bundled %f\"\n\
             TryExec=/opt/bundled/bin/bundled\nIcon=bundled-icon\n",
        );
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(
            &sb.root
                .join("share/icons/hicolor/48x48/apps/bundled-icon.png"),
            48,
            48,
            [3, 3, 3, 255],
        );
        let r = IconResolver::new(sb.lookup());
        assert_eq!(
            r.icon_name_for("/opt/bundled/bin/bundled").as_deref(),
            Some("bundled-icon")
        );
        assert!(r.icon_rgba_for_exe("/opt/bundled/bin/bundled").is_some());
    }

    /// An icon named like a file (`Icon=legacy.png`) must not turn into
    /// `legacy.png.png`, which never exists.
    #[test]
    fn icon_name_with_a_png_extension_is_not_doubled() {
        let sb = Sandbox::new("png-name");
        write_desktop(
            &sb.root.join("share/applications/legacy.desktop"),
            "[Desktop Entry]\nName=Legacy\nExec=/usr/bin/legacy\nIcon=legacy.png\n",
        );
        write_png(&sb.root.join("pixmaps/legacy.png"), 24, 24, [5, 5, 5, 255]);
        let r = IconResolver::new(sb.lookup());
        let (w, h, _) = r.icon_rgba_for_exe("/usr/bin/legacy").expect("pixmap icon");
        assert_eq!((w, h), (24, 24));
    }

    /// Pixmaps are read from every data dir (a per-user install lives in
    /// `~/.local/share/pixmaps`), then from the legacy `$HOME/.icons` of
    /// hand-installed apps, with the historical system path last and only once.
    #[test]
    fn pixmaps_are_read_from_every_data_dir() {
        let dirs = pixmap_dirs(
            &[
                PathBuf::from("/home/u/.local/share"),
                PathBuf::from("/usr/share"),
            ],
            Some(Path::new("/home/u/.icons")),
        );
        assert_eq!(
            dirs,
            [
                PathBuf::from("/home/u/.local/share/pixmaps"),
                PathBuf::from("/home/u/.icons"),
                PathBuf::from("/usr/share/pixmaps"),
            ]
        );
        // Without a home dir the list is only the data dirs' pixmaps plus the
        // legacy tail.
        assert_eq!(
            pixmap_dirs(&[PathBuf::from("/usr/share")], None),
            [PathBuf::from("/usr/share/pixmaps")]
        );
    }

    /// `$HOME/.icons` is the spec's legacy per-user icon dir; hand-installed
    /// themes live there and nowhere else.
    #[test]
    fn legacy_home_icons_dir_is_searched_for_themes() {
        let sb = Sandbox::new("home-icons");
        write_desktop(
            &sb.root.join("home/.icons/custom/index.theme"),
            "[Icon Theme]\nName=Custom\nDirectories=32x32/apps\n\n\
             [32x32/apps]\nSize=32\nType=Fixed\n",
        );
        write_png(
            &sb.root.join("home/.icons/custom/32x32/apps/handmade.png"),
            32,
            32,
            [8, 8, 8, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let f = r
            .find_icon_file("handmade")
            .expect("theme below $HOME/.icons");
        assert!(f.ends_with(".icons/custom/32x32/apps/handmade.png"));
    }

    /// `$HOME/.icons` ranks with the per-user data home: it is inserted right
    /// after `<data home>/icons`, never appended behind the system dirs, or a
    /// hand-installed theme would lose to the system's copy of the same name.
    #[test]
    fn legacy_home_icons_outranks_the_system_icon_dirs() {
        let lookup = IconLookup {
            data_dirs: vec![
                PathBuf::from("/home/u/.local/share"),
                PathBuf::from("/usr/share"),
            ],
            pixmaps: Vec::new(),
            theme: None,
            home_icons: Some(PathBuf::from("/home/u/.icons")),
        };
        assert_eq!(
            lookup.icon_dirs(),
            [
                PathBuf::from("/home/u/.local/share/icons"),
                PathBuf::from("/home/u/.icons"),
                PathBuf::from("/usr/share/icons"),
            ]
        );
    }

    /// Without a configured theme the spec's `default` theme supplies the real
    /// one, and its `Inherits` chain is followed to the end: `default` →
    /// `brand` → `legacy` is two levels and used to resolve nothing, which left
    /// every row on the generic glyph (or on no glyph at all).
    #[test]
    fn unknown_theme_resolves_through_the_default_theme() {
        let sb = Sandbox::new("default-theme");
        write_desktop(
            &sb.root.join("share/icons/default/index.theme"),
            "[Icon Theme]\nInherits=brand\n",
        );
        write_desktop(
            &sb.root.join("share/icons/brand/index.theme"),
            "[Icon Theme]\nName=Brand\nInherits=legacy\nDirectories=48x48/apps\n\n\
             [48x48/apps]\nSize=48\nType=Threshold\n",
        );
        write_desktop(
            &sb.root.join("share/icons/legacy/index.theme"),
            "[Icon Theme]\nName=Legacy\nInherits=hicolor\nDirectories=32x32/apps\n\n\
             [32x32/apps]\nSize=32\nType=Threshold\n",
        );
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        write_png(
            &sb.root.join("share/icons/legacy/32x32/apps/branded.png"),
            32,
            32,
            [6, 6, 6, 255],
        );
        let mut lookup = sb.lookup();
        lookup.theme = None;
        let r = IconResolver::new(lookup);
        let f = r
            .find_icon_file("branded")
            .expect("two Inherits levels below the default theme");
        assert!(f.ends_with("legacy/32x32/apps/branded.png"), "{f:?}");
    }

    /// The chain is `default` → its `Inherits` depth-first → `hicolor` last.
    #[test]
    fn theme_chain_is_transitive_and_keeps_hicolor_last() {
        let sb = Sandbox::new("chain-order");
        write_desktop(
            &sb.root.join("share/icons/default/index.theme"),
            "[Icon Theme]\nInherits=adwaita\n",
        );
        write_desktop(
            &sb.root.join("share/icons/adwaita/index.theme"),
            "[Icon Theme]\nName=Adwaita\nInherits=legacy,hicolor\n",
        );
        write_desktop(
            &sb.root.join("share/icons/legacy/index.theme"),
            "[Icon Theme]\nName=Legacy\n",
        );
        let icon_dirs = sb.lookup().icon_dirs();
        let themes = theme_chain(&icon_dirs, None);
        let chain: Vec<&str> = themes.iter().map(String::as_str).collect();
        assert_eq!(chain, ["default", "adwaita", "legacy", "hicolor"]);
    }

    /// A malformed `Inherits` cycle must terminate, not hang the icon worker.
    #[test]
    fn theme_chain_terminates_on_an_inherits_cycle() {
        let sb = Sandbox::new("chain-cycle");
        write_desktop(
            &sb.root.join("share/icons/a/index.theme"),
            "[Icon Theme]\nName=A\nInherits=b\n",
        );
        write_desktop(
            &sb.root.join("share/icons/b/index.theme"),
            "[Icon Theme]\nName=B\nInherits=a\n",
        );
        let icon_dirs = sb.lookup().icon_dirs();
        let themes = theme_chain(&icon_dirs, Some("a"));
        let chain: Vec<&str> = themes.iter().map(String::as_str).collect();
        assert_eq!(chain, ["a", "b", "hicolor"]);
    }

    /// A hidpi directory holds `Scale ×` the pixels of its `Size=`, and themes
    /// declare those dirs in `ScaledDirectories=` — ranking on the `Directories=`
    /// list with `Size=` alone missed them entirely.
    #[test]
    fn scaled_size_dirs_are_ranked_by_pixel_size() {
        let sb = Sandbox::new("scale");
        write_desktop(
            &sb.root.join("share/icons/custom/index.theme"),
            "[Icon Theme]\nName=Custom\nDirectories=16x16/apps\n\
             ScaledDirectories=16x16@2/apps\n\n\
             [16x16/apps]\nSize=16\nType=Fixed\n\n\
             [16x16@2/apps]\nSize=16\nScale=2\nType=Fixed\n",
        );
        write_png(
            &sb.root.join("share/icons/custom/16x16/apps/sharp.png"),
            16,
            16,
            [1, 1, 1, 255],
        );
        write_png(
            &sb.root.join("share/icons/custom/16x16@2/apps/sharp.png"),
            32,
            32,
            [2, 2, 2, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let f = r.find_icon_file("sharp").expect("found");
        assert!(f.ends_with("16x16@2/apps/sharp.png"), "{f:?}");
    }

    /// A theme without a `Scale=` key still marks the multiplier in the
    /// directory name — and its hidpi dirs may be listed in
    /// `ScaledDirectories=` only: `16x16@3` must rank as 48 px, not as 16.
    #[test]
    fn scaled_directory_name_supplies_a_missing_scale() {
        let sb = Sandbox::new("scale-name");
        write_desktop(
            &sb.root.join("share/icons/custom/index.theme"),
            "[Icon Theme]\nName=Custom\nDirectories=8x8/apps\n\
             ScaledDirectories=16x16@3/apps\n\n\
             [8x8/apps]\nSize=8\nType=Fixed\n\n\
             [16x16@3/apps]\nSize=16\nType=Fixed\n",
        );
        write_png(
            &sb.root.join("share/icons/custom/8x8/apps/sharp.png"),
            8,
            8,
            [1, 1, 1, 255],
        );
        write_png(
            &sb.root.join("share/icons/custom/16x16@3/apps/sharp.png"),
            48,
            48,
            [3, 3, 3, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let f = r.find_icon_file("sharp").expect("found");
        assert!(f.ends_with("16x16@3/apps/sharp.png"), "{f:?}");
    }

    /// The size-directory scan (no `index.theme`) takes the multiplier from the
    /// name too, otherwise `16x16@2` ties with `16x16` and `read_dir` order
    /// decides which file the row gets.
    #[test]
    fn size_directory_names_carry_the_hidpi_multiplier() {
        assert_eq!(dir_size_from_name("16x16"), 16);
        assert_eq!(dir_size_from_name("48x48/apps"), 48);
        assert_eq!(dir_size_from_name("16x16@2"), 32);
        assert_eq!(dir_size_from_name("16x16@2/apps"), 32);
        assert_eq!(dir_size_from_name("scalable"), 0);
        assert_eq!(dir_size_from_name("16x16@x"), 16);
        assert_eq!(name_scale("16x16@2/apps"), 2);
        assert_eq!(name_scale("16x16/apps"), 1);
        assert_eq!(name_scale("scalable/apps"), 1);
        assert_eq!(name_scale("16x16@0/apps"), 1);
        assert_eq!(nominal_size_from_name("16x16@2/apps"), 16);
        assert_eq!(nominal_size_from_name("32x32/apps"), 32);
        assert_eq!(nominal_size_from_name("scalable/apps"), 0);
    }

    /// A hidpi directory and a native one can hold the same number of pixels
    /// (`16x16@2` and `32x32` are both 32 px): the native one must win, or a
    /// 16 px asset gets upscaled where a real 32 px file exists.
    #[test]
    fn native_size_directory_wins_a_pixel_size_tie() {
        let sb = Sandbox::new("scale-tie");
        write_desktop(
            &sb.root.join("share/icons/custom/index.theme"),
            "[Icon Theme]\nName=Custom\nDirectories=32x32/apps\n\
             ScaledDirectories=16x16@2/apps\n\n\
             [32x32/apps]\nSize=32\nType=Fixed\n\n\
             [16x16@2/apps]\nSize=16\nScale=2\nType=Fixed\n",
        );
        write_png(
            &sb.root.join("share/icons/custom/32x32/apps/native.png"),
            32,
            32,
            [9, 9, 9, 255],
        );
        write_png(
            &sb.root.join("share/icons/custom/16x16@2/apps/native.png"),
            32,
            32,
            [1, 1, 1, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let f = r.find_icon_file("native").expect("found");
        assert!(f.ends_with("32x32/apps/native.png"), "{f:?}");
    }

    /// End to end through the no-index.theme branch: the sharper `@2` directory
    /// wins on its pixel size, not on its position in the directory listing.
    #[test]
    fn hidpi_size_directory_wins_without_an_index_theme() {
        let sb = Sandbox::new("scale-noindex");
        write_png(
            &sb.root.join("share/icons/custom/16x16/apps/plain.png"),
            16,
            16,
            [1, 1, 1, 255],
        );
        write_png(
            &sb.root.join("share/icons/custom/16x16@2/apps/plain.png"),
            32,
            32,
            [2, 2, 2, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let f = r.find_icon_file("plain").expect("found");
        assert!(f.ends_with("16x16@2/apps/plain.png"), "{f:?}");
    }

    /// SVG is skipped while a size directory is scanned, so the PNG in the
    /// next-best size still gets its turn instead of the row losing its icon.
    #[test]
    fn svg_only_size_dir_falls_through_to_a_png() {
        let sb = Sandbox::new("svg-fallthrough");
        write_desktop(
            &sb.root.join("share/icons/custom/index.theme"),
            "[Icon Theme]\nName=Custom\nDirectories=32x32/apps,48x48/apps\n\n\
             [32x32/apps]\nSize=32\nType=Fixed\n\n\
             [48x48/apps]\nSize=48\nType=Fixed\n",
        );
        // A text stand-in is enough: the resolver looks for `.png` only.
        write_desktop(
            &sb.root.join("share/icons/custom/32x32/apps/vector.svg"),
            "<svg/>\n",
        );
        write_png(
            &sb.root.join("share/icons/custom/48x48/apps/vector.png"),
            48,
            48,
            [4, 4, 4, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let f = r.find_icon_file("vector").expect("png fallback");
        assert!(f.ends_with("48x48/apps/vector.png"), "{f:?}");
    }

    /// The `<context>` directory a resolved icon file lives in.
    fn context_dir_of(path: &Path) -> Option<&str> {
        path.parent()?.file_name()?.to_str()
    }

    /// The sort that decides a shared icon name, pinned without the filesystem.
    ///
    /// `read_dir` cannot be trusted to expose an unsorted order here: NTFS
    /// collates names, so a walk-based test would pass with the sort removed.
    #[test]
    fn the_context_sort_orders_by_name() {
        let size_dir = Path::new("/icons/32x32");
        let mimetypes = size_dir.join("mimetypes");
        let apps = size_dir.join("apps");
        assert_eq!(
            sorted(vec![mimetypes.clone(), apps.clone()]),
            vec![apps, mimetypes]
        );
    }

    /// One icon name can exist in several contexts of the same size dir, so the
    /// scan must not let the filesystem's directory order decide: it takes the
    /// candidates in the order it is given, and `find_in_theme` gives it a
    /// sorted list. Feeding it `mimetypes` first proves the scan obeys its input
    /// order — the order `read_dir` used to supply — and the sorted list then
    /// has to pick `apps` on every filesystem, whatever order the two dirs were
    /// created in.
    #[test]
    fn a_shared_icon_name_resolves_to_the_sorted_first_context() {
        let sb = Sandbox::new("context-order");
        let size_dir = sb.root.join("share/icons/custom/32x32");
        write_png(&size_dir.join("apps/ctxapp.png"), 32, 32, [0, 200, 0, 255]);
        write_png(
            &size_dir.join("mimetypes/ctxapp.png"),
            32,
            32,
            [200, 0, 0, 255],
        );
        let apps = size_dir.join("apps");
        let mimetypes = size_dir.join("mimetypes");

        // Unsorted candidates: whoever comes first wins, so this is the order
        // the old `read_dir` walk would have exposed.
        assert_eq!(
            first_in_contexts(&[mimetypes.clone(), apps.clone()], "ctxapp.png"),
            Some(mimetypes.join("ctxapp.png"))
        );
        // The production order is sorted, so `apps` precedes `mimetypes`.
        let sorted = sorted_dir_paths(&size_dir);
        assert_eq!(sorted, vec![apps.clone(), mimetypes]);
        let found = first_in_contexts(&sorted, "ctxapp.png").expect("context icon");
        assert_eq!(found, apps.join("ctxapp.png"));

        // End to end: the resolver's walk hands the sorted list to the scan.
        let r = IconResolver::new(sb.lookup());
        let resolved = r.find_icon_file("ctxapp").expect("context icon");
        assert_eq!(context_dir_of(&resolved), Some("apps"), "{resolved:?}");
        assert_eq!(
            decode_png_rgba(&resolved).map(|(_, _, rgba)| rgba[..4].to_vec()),
            Some(vec![0, 200, 0, 255]),
            "the apps context must supply the pixels: {resolved:?}"
        );
    }

    /// Plasma writes the icon theme to `kdeglobals`, not to a GTK file; without
    /// reading it a Wayland Plasma session had no theme at all.
    #[test]
    fn kde_icon_theme_is_read_from_the_icons_section() {
        let text = "[General]\nDesktopTheme=Breeze\nTheme=ignored\n\n[Icons]\nTheme=breeze-dark\n";
        assert_eq!(parse_kde_icon_theme(text).as_deref(), Some("breeze-dark"));
        assert_eq!(parse_kde_icon_theme("[Icons]\nTheme=\n"), None);
        assert_eq!(parse_kde_icon_theme("[General]\nTheme=ignored\n"), None);
    }

    /// A theme's `index.theme` is read and parsed once per theme dir, not once
    /// per icon. The declared layout is only reachable through that file (the
    /// no-index fallback walks one level of `<size>` and one of `<context>`,
    /// this one is deeper), so the second icon of the same theme still resolves
    /// after the file disappears — while a fresh resolver, which has to read it
    /// again, finds nothing.
    #[test]
    fn theme_layout_is_parsed_once_per_theme_dir() {
        let sb = Sandbox::new("theme-layout-cache");
        write_desktop(
            &sb.root.join("share/icons/custom/index.theme"),
            "[Icon Theme]\nName=Custom\nDirectories=vendor/32x32/apps\n\n\
             [vendor/32x32/apps]\nSize=32\nType=Fixed\n",
        );
        for (name, rgba) in [("first", [1, 1, 1, 255]), ("second", [2, 2, 2, 255])] {
            write_png(
                &sb.root
                    .join(format!("share/icons/custom/vendor/32x32/apps/{name}.png")),
                32,
                32,
                rgba,
            );
        }
        let r = IconResolver::new(sb.lookup());
        let first = r.find_icon_file("first").expect("declared layout");
        assert!(first.ends_with("vendor/32x32/apps/first.png"), "{first:?}");
        std::fs::remove_file(sb.root.join("share/icons/custom/index.theme")).unwrap();
        let second = r.find_icon_file("second").expect("cached theme layout");
        assert!(
            second.ends_with("vendor/32x32/apps/second.png"),
            "{second:?}"
        );
        // Control: without the cached parse the same tree yields nothing.
        let fresh = IconResolver::new(sb.lookup());
        assert_eq!(fresh.find_icon_file("second"), None);
    }

    /// One `Icon=` value is shared by many executables (every Chrome/Electron
    /// helper, every Wine process of the same entry). The PNG is decoded once:
    /// the pixels survive the file being deleted, so a second executable that
    /// maps to the same icon still paints it. Requires both the resolved-file
    /// cache and the decoded-pixels cache — either alone re-reads the gone file.
    #[test]
    fn shared_icon_value_is_decoded_once() {
        let sb = Sandbox::new("decode-cache");
        write_desktop(
            &sb.root.join("share/applications/aaa.desktop"),
            "[Desktop Entry]\nName=Aaa\nExec=/usr/bin/aaa\nIcon=shared\n",
        );
        write_desktop(
            &sb.root.join("share/applications/bbb.desktop"),
            "[Desktop Entry]\nName=Bbb\nExec=/usr/bin/bbb\nIcon=shared\n",
        );
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        let png = sb.root.join("share/icons/hicolor/32x32/apps/shared.png");
        write_png(&png, 32, 32, [9, 9, 9, 255]);
        let r = IconResolver::new(sb.lookup());
        let icon = |exe: &str| r.icon_rgba_for_exe(exe).expect("icon");
        let first = icon("/usr/bin/aaa");
        assert_eq!((first.0, first.1), (32, 32));
        std::fs::remove_file(&png).unwrap();
        let second = icon("/usr/bin/bbb");
        assert_eq!(second, first);
    }

    /// The decoded-icon cache is bounded: past its cap the oldest insertion is
    /// dropped, so a session meeting thousands of distinct icons cannot grow it
    /// without limit. An evicted icon is simply decoded again — still not a
    /// fabricated one.
    #[test]
    fn decoded_cache_evicts_oldest_past_its_cap() {
        let sb = Sandbox::new("decode-cap");
        let r = IconResolver::new(sb.lookup());
        let icon = |i: usize| sb.root.join(format!("icons/{i}.png"));
        for i in 0..DECODE_CACHE_CAP + 4 {
            write_png(&icon(i), 8, 8, [1, 2, 3, 255]);
            assert!(r.icon_rgba_for_file(&icon(i)).is_some(), "icon {i}");
        }
        let caches = tm_core::sync::lock(&r.caches);
        assert_eq!(caches.decoded.entries.len(), DECODE_CACHE_CAP);
        for i in 0..4 {
            assert!(
                caches.decoded.get(&icon(i)).is_none(),
                "the four oldest insertions are gone (icon {i})"
            );
        }
        assert!(
            caches.decoded.get(&icon(4)).is_some(),
            "the next-oldest insertion stays cached"
        );
        assert!(caches.decoded.get(&icon(DECODE_CACHE_CAP + 3)).is_some());
        drop(caches);
        assert!(r.icon_rgba_for_file(&icon(0)).is_some());
    }

    /// A file that does not decode is a miss on every call and is not
    /// remembered: the row draws nothing instead of a fabricated icon, and the
    /// caller's own retry schedule decides when to try again.
    #[test]
    fn a_failed_decode_is_not_cached_and_stays_none() {
        let sb = Sandbox::new("decode-fail");
        let broken = sb.root.join("broken.png");
        std::fs::write(&broken, b"not a png at all").unwrap();
        let r = IconResolver::new(sb.lookup());
        assert_eq!(r.icon_rgba_for_file(&broken), None);
        assert_eq!(r.icon_rgba_for_file(&broken), None);
        let caches = tm_core::sync::lock(&r.caches);
        assert!(caches.decoded.get(&broken).is_none());
    }

    /// A lookup that misses must stay retryable: the GUI keeps a failed key out
    /// of the queue only for `FAILURE_RETRY_TTL`, so an icon that appears later
    /// (an installed package, a file restored over the network, a directory
    /// that was briefly unreadable) has to resolve on the next attempt. This
    /// pins that the negative answer is not cached for the process lifetime.
    #[test]
    fn a_missed_lookup_resolves_once_the_file_appears() {
        let sb = Sandbox::new("miss-retry");
        write_desktop(
            &sb.root.join("share/icons/custom/index.theme"),
            "[Icon Theme]\nName=Custom\nDirectories=vendor/32x32/apps\n\n\
             [vendor/32x32/apps]\nSize=32\nType=Fixed\n",
        );
        let r = IconResolver::new(sb.lookup());
        assert_eq!(r.find_icon_file("late"), None);
        write_png(
            &sb.root
                .join("share/icons/custom/vendor/32x32/apps/late.png"),
            32,
            32,
            [7, 8, 9, 255],
        );
        let late = r.find_icon_file("late").expect("retried lookup finds it");
        assert!(late.ends_with("vendor/32x32/apps/late.png"), "{late:?}");
    }

    /// A themed icon name is one path component: it is joined onto every icon
    /// dir, so a relative `..` reads — and paints — a file outside them. The
    /// plain name must keep resolving, or the guard would have removed the
    /// lookup instead of the traversal.
    #[test]
    fn an_icon_name_that_climbs_out_of_the_theme_resolves_nothing() {
        let sb = Sandbox::new("icon-traversal");
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        // Exactly the file the climb would reach: `32x32/apps/../…`.
        write_png(
            &sb.root.join("share/icons/hicolor/32x32/escape.png"),
            32,
            32,
            [9, 9, 9, 255],
        );
        write_png(
            &sb.root.join("share/icons/hicolor/32x32/apps/escape.png"),
            32,
            32,
            [1, 2, 3, 255],
        );
        let r = IconResolver::new(sb.lookup());
        assert_eq!(r.find_icon_file("../../escape"), None);
        let plain = r.find_icon_file("escape").expect("the plain name resolves");
        assert!(plain.ends_with("32x32/apps/escape.png"), "{plain:?}");
    }

    /// An `Inherits=` entry becomes a theme name and is joined onto every icon
    /// dir too, so a climbing value must not enter the chain.
    #[test]
    fn an_inherits_entry_that_climbs_is_not_a_theme() {
        let sb = Sandbox::new("chain-traversal");
        write_desktop(
            &sb.root.join("share/icons/custom/index.theme"),
            "[Icon Theme]\nName=Custom\nInherits=../../escape,hicolor\nDirectories=32x32/apps\n\n\
             [32x32/apps]\nSize=32\n",
        );
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            HICOLOR_INDEX,
        );
        let icon_dirs = sb.lookup().icon_dirs();
        let themes = theme_chain(&icon_dirs, Some("custom"));
        let chain: Vec<&str> = themes.iter().map(String::as_str).collect();
        assert_eq!(chain, ["custom", "hicolor"]);
    }

    /// A theme's declared directory is joined onto the theme dir and every icon
    /// file under it is probed there, so an absolute entry or one climbing with
    /// `..` is not a directory of that theme.
    #[test]
    fn declared_theme_directories_stay_inside_the_theme() {
        let sb = Sandbox::new("theme-dirs");
        let theme_dir = sb.root.join("share/icons/custom");
        write_desktop(
            &theme_dir.join("index.theme"),
            "[Icon Theme]\nName=Custom\nDirectories=32x32/apps,/etc,../../b\n\n\
             [32x32/apps]\nSize=32\n\n[/etc]\nSize=48\n\n[../../b]\nSize=16\n",
        );
        let dirs = size_dirs_of_theme(&theme_dir);
        assert_eq!(
            dirs.iter().map(|dir| &dir.path).collect::<Vec<_>>(),
            [&theme_dir.join("32x32/apps")],
            "only the directory inside the theme is a candidate"
        );
        assert_eq!(dirs[0].pixel_size, 32);
    }

    /// An empty `Icon=` is not an icon. Indexing it stored `""` for the entry's
    /// id and for every executable it names, and since the first value inserted
    /// wins, a later entry's real icon never applied.
    #[test]
    fn an_empty_icon_does_not_shadow_a_later_entrys_icon() {
        let sb = Sandbox::new("empty-icon");
        write_desktop(
            &sb.root.join("share/applications/aaa-empty.desktop"),
            "[Desktop Entry]\nExec=/usr/bin/shared\nIcon=\n",
        );
        write_desktop(
            &sb.root.join("share/applications/zzz-real.desktop"),
            "[Desktop Entry]\nExec=/usr/bin/shared\nIcon=real-icon\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert_eq!(index.get("shared").map(String::as_str), Some("real-icon"));
        assert!(
            !index.contains_key("aaa-empty"),
            "an entry without an icon names no icon"
        );
    }

    /// The env-assignment prefix is recognized by its NAME. Reading the whole
    /// token instead made a value holding a path the executable, so the entry
    /// claimed an icon for a binary named after the value's last component.
    #[test]
    fn an_env_assignment_with_a_path_value_does_not_become_the_executable() {
        assert_eq!(
            exec_first_binary("env LD_LIBRARY_PATH=/opt/app/lib /opt/app/bin/app %U").as_deref(),
            Some("/opt/app/bin/app")
        );
        assert_eq!(
            exec_first_binary("env LD_PRELOAD=/usr/lib/preload.so /opt/app/bin/app").as_deref(),
            Some("/opt/app/bin/app")
        );
        assert_eq!(
            exec_first_binary("QT_QPA_PLATFORM=xcb /opt/app/bin/app").as_deref(),
            Some("/opt/app/bin/app")
        );
    }

    /// With a 32 px target, `16x16` and `48x48` are both 16 px away from it. The
    /// larger asset wins, so the icon is scaled down instead of up.
    #[test]
    fn an_equal_distance_size_tie_prefers_the_larger_asset() {
        let sb = Sandbox::new("size-tie");
        write_desktop(
            &sb.root.join("share/icons/hicolor/index.theme"),
            "[Icon Theme]\nName=Hicolor\nDirectories=16x16/apps,48x48/apps\n\n\
             [16x16/apps]\nSize=16\n\n[48x48/apps]\nSize=48\n",
        );
        write_png(
            &sb.root.join("share/icons/hicolor/16x16/apps/tie.png"),
            16,
            16,
            [1, 1, 1, 255],
        );
        write_png(
            &sb.root.join("share/icons/hicolor/48x48/apps/tie.png"),
            48,
            48,
            [2, 2, 2, 255],
        );
        let r = IconResolver::new(sb.lookup());
        let file = r.find_icon_file("tie").expect("a candidate size dir");
        assert!(file.ends_with("48x48/apps/tie.png"), "{file:?}");
    }

    /// Sibling `applications/` subdirectories are scanned in name order — the
    /// same rule that decides a shared name inside one directory.
    #[test]
    fn sibling_application_subdirectories_are_scanned_in_name_order() {
        let sb = Sandbox::new("apps-subdir-order");
        write_desktop(
            &sb.root.join("share/applications/aaa/first.desktop"),
            "[Desktop Entry]\nExec=/usr/bin/shared\nIcon=first-icon\n",
        );
        write_desktop(
            &sb.root.join("share/applications/zzz/last.desktop"),
            "[Desktop Entry]\nExec=/usr/bin/shared\nIcon=last-icon\n",
        );
        let index = build_desktop_index(&[sb.root.join("share")]);
        assert_eq!(index.get("shared").map(String::as_str), Some("first-icon"));
    }

    /// The `.desktop` index is keyed lowercase, but the themed fallback name
    /// keeps the executable's own case: on a case-sensitive filesystem the
    /// theme file is `MyApp.png`, not `myapp.png`.
    #[test]
    fn the_themed_fallback_name_keeps_the_executables_case() {
        let sb = Sandbox::new("mixed-case");
        let r = IconResolver::new(sb.lookup());
        assert_eq!(r.icon_name_for("/usr/bin/MyApp").as_deref(), Some("MyApp"));
    }
}
