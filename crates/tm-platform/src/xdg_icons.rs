//! Freedesktop (XDG) icon-theme resolution: map an executable to the icon
//! the OS's current icon theme would show for it, and decode that icon to
//! straight-alpha RGBA for the GUI.
//!
//! The lookup follows the icon theme spec (https://specifications.freedesktop.org/icon-theme-spec):
//! an executable basename is matched against the `Exec=` line (or the desktop
//! file's own id) of the `.desktop` files in the XDG data dirs; the resulting
//! `Icon=` value is then looked up in the icon theme chain (current theme,
//! its `Inherits`, then `hicolor`) under `$XDG_DATA_HOME/icons`,
//! `$XDG_DATA_DIRS/icons` and finally the legacy pixmaps dir. An absolute
//! `Icon=` path is used directly.
//!
//! Only PNG icons are decoded (the overwhelmingly common case in hicolor and
//! shipped themes); SVG/XPM entries are skipped, so an app whose icon exists
//! only as SVG falls back to the generic glyph. The module is platform-neutral
//! on purpose: only the Linux backend calls it, but compiling it everywhere
//! lets its unit tests run on the Windows dev box and CI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Maximum edge of a decoded icon; larger PNGs are box-downsampled so the
/// GUI-side texture cache stays small (64×64×4 ≈ 16 KiB per icon).
const MAX_ICON_EDGE: u32 = 64;
/// The GUI paints icons at ~18 px; 32 is the size desktop shells hand out.
const TARGET_SIZE: u32 = 32;

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
    /// Legacy fallback dir, usually `/usr/share/pixmaps`.
    pub pixmaps: Option<PathBuf>,
    /// Name of the current icon theme (e.g. from the GTK settings), if known.
    pub theme: Option<String>,
}

impl IconLookup {
    /// Build the lookup from the process environment (XDG variables, HOME,
    /// GTK settings.ini for the current icon theme).
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
        Self {
            data_dirs,
            pixmaps: Some("/usr/share/pixmaps".into()),
            theme,
        }
    }
}

/// `gtk-icon-theme-name` from the GTK 3/4 settings.ini, if present.
fn current_icon_theme(home: Option<&Path>) -> Option<String> {
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
    None
}

/// Resolver with a lazily built desktop-file index (exe basename → icon
/// name), so repeated lookups do not rescan `applications/`.
pub struct IconResolver {
    lookup: IconLookup,
    /// exe basename (lowercase) → `Icon=` value, highest-precedence desktop
    /// file wins.
    desktop_index: HashMap<String, String>,
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
        Self {
            lookup,
            desktop_index,
        }
    }

    /// Resolve `exe_path` (an executable path, a `.desktop` file path, or a
    /// bare command name) to a decoded RGBA icon.
    pub fn icon_rgba_for_exe(&self, exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
        let name = self.icon_name_for(exe_path)?;
        let file = self.find_icon_file(&name)?;
        decode_png_rgba(&file)
    }

    /// The icon the theme shows for an executable that has no icon of its own
    /// (a kernel thread, an app without a `.desktop` entry): the standard
    /// "this is an application" icon, then the spec's missing-image icon.
    pub fn default_app_icon_rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
        DEFAULT_APP_ICON_NAMES
            .iter()
            .find_map(|name| self.find_icon_file(name).and_then(|f| decode_png_rgba(&f)))
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
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())?;
        if let Some(icon) = self.desktop_index.get(&base) {
            return Some(icon.clone());
        }
        // No desktop entry: the icon theme spec's fallback is the executable
        // basename as a themed icon name (hicolor ships one for most apps).
        Some(base)
    }

    /// Locate the file for an icon `name` following the theme spec: absolute
    /// path used as-is; otherwise the current theme, its `Inherits` chain and
    /// `hicolor` across all icon dirs, then the pixmaps dir. PNG files only.
    pub fn find_icon_file(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() {
            return None;
        }
        // Per the spec an icon name that is an absolute path is used directly.
        if Path::new(name).is_absolute() {
            let p = PathBuf::from(name);
            return p.is_file().then_some(p);
        }
        let icon_dirs: Vec<PathBuf> = self
            .lookup
            .data_dirs
            .iter()
            .map(|d| d.join("icons"))
            .collect();
        // Theme chain: current theme first, then hicolor as the mandatory
        // fallback, de-duplicated.
        let mut themes: Vec<String> = Vec::new();
        if let Some(t) = &self.lookup.theme {
            themes.push(t.clone());
        }
        themes.push("hicolor".to_string());
        let mut seen = std::collections::HashSet::new();
        for theme in &themes {
            if !seen.insert(theme.clone()) {
                continue;
            }
            for dir in &icon_dirs {
                if let Some(f) = find_in_theme(dir, theme, name) {
                    return Some(f);
                }
            }
            // Follow Inherits of this theme (one level is what the spec's
            // chain needs in practice; hicolor is always tried anyway).
            for parent in theme_inherits(&icon_dirs, theme) {
                if seen.contains(&parent) {
                    continue;
                }
                seen.insert(parent.clone());
                for dir in &icon_dirs {
                    if let Some(f) = find_in_theme(dir, &parent, name) {
                        return Some(f);
                    }
                }
            }
        }
        // Legacy pixmaps dir: `<pixmaps>/<name>.png`.
        if let Some(px) = &self.lookup.pixmaps {
            let f = px.join(format!("{name}.png"));
            if f.is_file() {
                return Some(f);
            }
        }
        None
    }
}

/// Scan `<data_dir>/applications` (recursively, one level of subdirs is
/// enough for KDE's legacy layout) for `.desktop` files and index their
/// `Icon=` by exe basename. Matching rules: the first meaningful token of
/// `Exec=` (skipping `env VAR=..` prefixes) by basename, or the desktop
/// file's own id. Higher-precedence data dirs are scanned first and win.
fn build_desktop_index(data_dirs: &[PathBuf]) -> HashMap<String, String> {
    let mut index = HashMap::new();
    for data_dir in data_dirs {
        let apps = data_dir.join("applications");
        let mut stack = vec![apps.clone()];
        // Two levels cover applications/ and e.g. applications/kde4/.
        let mut depth_left = 2;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    if depth_left > 0 {
                        depth_left -= 1;
                        stack.push(p);
                    }
                    continue;
                }
                let Some(file_name) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !file_name.ends_with(".desktop") {
                    continue;
                }
                let Some((exec, icon)) = desktop_file_exec_icon(&p) else {
                    continue;
                };
                let Some(icon) = icon else { continue };
                let stem = file_name.trim_end_matches(".desktop").to_ascii_lowercase();
                index.entry(stem).or_insert_with(|| icon.clone());
                if let Some(exe_base) = exec
                    .as_deref()
                    .and_then(exec_first_binary)
                    .and_then(|b| b.rsplit('/').next().map(str::to_ascii_lowercase))
                    .filter(|b| !b.is_empty())
                {
                    index.entry(exe_base).or_insert_with(|| icon.clone());
                }
            }
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

/// Parse a `.desktop` file for its `Exec=` command and `Icon=` value.
fn desktop_file_exec_icon(path: &Path) -> Option<(Option<String>, Option<String>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let mut exec: Option<String> = None;
    let mut icon: Option<String> = None;
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
            exec.get_or_insert_with(|| v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Icon=") {
            icon.get_or_insert_with(|| v.trim().to_string());
        }
    }
    Some((exec, icon))
}

/// First meaningful token of a desktop `Exec=` line: strip field codes
/// (`%u`, `%F`, ...), then skip `env`/`sudo` and `VAR=value` prefixes.
fn exec_first_binary(exec: &str) -> Option<String> {
    let mut tokens = exec.split_whitespace();
    let mut first = tokens.next()?;
    loop {
        let lower = first.to_ascii_lowercase();
        if lower == "env" || lower == "sudo" {
            first = tokens.next()?;
            continue;
        }
        if first.contains('=') && !first.contains('/') {
            // env-style assignment prefix (only before the binary)
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

/// Find `<icon_dir>/<theme>/<size>/<context>/<name>.png`, choosing the size
/// closest to [`TARGET_SIZE`]. Uses the theme's `index.theme` directory list
/// when present; otherwise walks the two-level `<size>/<context>` layout.
fn find_in_theme(icon_dir: &Path, theme: &str, name: &str) -> Option<PathBuf> {
    let theme_dir = icon_dir.join(theme);
    if !theme_dir.is_dir() {
        return None;
    }
    let target = format!("{name}.png");
    // Candidate subdirs with their nominal size, best (closest to 32) first.
    let mut candidates: Vec<(u32, PathBuf)> = Vec::new();
    if let Some(dirs) = theme_directories(&theme_dir) {
        for (size, sub) in dirs {
            candidates.push((size, theme_dir.join(sub)));
        }
    } else {
        // No index.theme: scan one level of `<size>` dirs, then `<context>`.
        if let Ok(sizes) = std::fs::read_dir(&theme_dir) {
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
                let size = size_name
                    .split('x')
                    .next()
                    .and_then(|n| n.parse::<u32>().ok())
                    .unwrap_or(0);
                candidates.push((size, size_entry.path()));
            }
        }
    }
    candidates.sort_by_key(|(size, _)| size.abs_diff(TARGET_SIZE));
    for (_, dir) in candidates {
        // The icon may sit directly in the size dir (rare) or one context
        // level deeper (the standard layout).
        let direct = dir.join(&target);
        if direct.is_file() {
            return Some(direct);
        }
        if let Ok(contexts) = std::fs::read_dir(&dir) {
            for ctx in contexts.flatten() {
                let f = ctx.path().join(&target);
                if f.is_file() {
                    return Some(f);
                }
            }
        }
    }
    None
}

/// `(size, subdir)` pairs from a theme's `index.theme`: the `Directories=`
/// list of `[Icon Theme]`, each with the `Size=` of its own section.
fn theme_directories(theme_dir: &Path) -> Option<Vec<(u32, String)>> {
    let text = std::fs::read_to_string(theme_dir.join("index.theme")).ok()?;
    let mut dirs: Vec<String> = Vec::new();
    let mut sizes: HashMap<String, u32> = HashMap::new();
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
            dirs.extend(
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
        if !in_theme
            && let (Some(sec), Some(v)) = (&section, line.strip_prefix("Size="))
            && let Ok(n) = v.trim().parse::<u32>()
        {
            sizes.insert(sec.clone(), n);
        }
    }
    if dirs.is_empty() {
        return None;
    }
    Some(
        dirs.into_iter()
            .map(|d| (sizes.get(&d).copied().unwrap_or(0), d))
            .collect(),
    )
}

/// Decode a PNG file to straight-alpha RGBA, box-downsampling anything larger
/// than [`MAX_ICON_EDGE`] on a side.
pub fn decode_png_rgba(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    // EXPAND: palette -> RGB, grayscale <8bit -> 8bit, tRNS -> alpha channel.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
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
    Some(if w.max(h) > MAX_ICON_EDGE {
        downscale_rgba(&src, w, h)
    } else {
        (w, h, src)
    })
}

/// Box-filter downscale so the longest edge is at most [`MAX_ICON_EDGE`].
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
            let mut acc = [0u64; 4];
            let mut count = 0u64;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * w + sx) * 4) as usize;
                    for (a, c) in acc.iter_mut().zip(rgba[i..i + 4].iter()) {
                        *a += *c as u64;
                    }
                    count += 1;
                }
            }
            let o = ((y * nw + x) * 4) as usize;
            for (a, c) in acc.iter().zip(out[o..o + 4].iter_mut()) {
                *c = (*a / count.max(1)) as u8;
            }
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

    fn write_png(path: &Path, w: u32, h: u32, rgba: [u8; 4]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let file = std::fs::File::create(path).unwrap();
        let mut enc = png::Encoder::new(file, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            data.extend_from_slice(&rgba);
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
                pixmaps: Some(self.root.join("pixmaps")),
                theme: Some("custom".into()),
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
}
