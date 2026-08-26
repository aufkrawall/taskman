//! Font setup: prefer OS-native fonts for a native look (Segoe UI on Windows,
//! SF Pro via SFNS on macOS, Fontconfig-selected faces on Linux), fall back
//! to known distro paths and then egui's bundled defaults.
//!
//! System font files are read on a background thread so startup stays fast.

use eframe::egui::{self, FontData, FontDefinitions};
use std::sync::Arc;

pub fn install_async(ctx: egui::Context) {
    let ctx2 = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("tm-fonts".into())
        .spawn(move || {
            let defs = build_definitions();
            *apply_result().lock().unwrap_or_else(|e| e.into_inner()) = Some(defs);
            ctx2.request_repaint();
        });
    if spawned.is_err() {
        let defs = build_definitions();
        ctx.set_fonts(defs);
    }
}

fn apply_result() -> &'static std::sync::Mutex<Option<FontDefinitions>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<FontDefinitions>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

pub fn poll_async_apply(ctx: &egui::Context) {
    let ready = apply_result()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(defs) = ready {
        tracing::info!("system fonts applied after first frame");
        ctx.set_fonts(defs);
    }
}

fn build_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();

    let mut proportional_candidates: Vec<(&str, Vec<String>)> = Vec::new();
    #[cfg(target_os = "linux")]
    if let Some(path) = fontconfig_match("sans-serif") {
        proportional_candidates.push(("FontconfigSans", vec![path]));
    }
    proportional_candidates.extend([
        (
            "SegoeUI",
            vec![
                r"C:\Windows\Fonts\segoeui.ttf".into(),
                r"C:\Windows\Fonts\SegoeUIVar.ttf".into(),
            ],
        ),
        ("SFNS", vec!["/System/Library/Fonts/SFNS.ttf".into()]),
        (
            "NotoSans",
            vec![
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf".into(),
                "/usr/share/fonts/TTF/NotoSans-Regular.ttf".into(),
                "/usr/share/noto-cjk/NotoSansCJK-Regular.ttc".into(),
            ],
        ),
        (
            "DejaVuSans",
            vec!["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into()],
        ),
    ]);

    for (name, paths) in &proportional_candidates {
        if let Some(data) = load_first(paths) {
            tracing::info!(font = *name, "using system proportional font");
            fonts.font_data.insert((*name).to_string(), Arc::new(data));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, (*name).to_string());
            break;
        }
    }

    // Bold remains an optional fallback. Egui's font system does not select
    // a separate face purely from RichText::strong, so do not pretend this is
    // native DirectWrite/Fontconfig shaping; it is kept for glyph fallback.
    let bold_candidates: &[(&str, Vec<String>)] = &[
        ("SegoeUI-Bold", vec![r"C:\Windows\Fonts\segoeuib.ttf".into()]),
        (
            "NotoSans-Bold",
            vec!["/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf".into()],
        ),
        (
            "DejaVuSans-Bold",
            vec!["/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf".into()],
        ),
    ];
    for (name, paths) in bold_candidates {
        if let Some(data) = load_first(paths) {
            fonts.font_data.insert(name.to_string(), Arc::new(data));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push(name.to_string());
            break;
        }
    }

    let mut mono_candidates: Vec<(&str, Vec<String>)> = Vec::new();
    #[cfg(target_os = "linux")]
    if let Some(path) = fontconfig_match("monospace") {
        mono_candidates.push(("FontconfigMono", vec![path]));
    }
    mono_candidates.extend([
        ("CascadiaMono", vec![r"C:\Windows\Fonts\CascadiaMono.ttf".into()]),
        ("Consolas", vec![r"C:\Windows\Fonts\consola.ttf".into()]),
        (
            "JetBrainsMono",
            vec!["/usr/share/fonts/truetype/JetBrainsMono/JetBrainsMono-Regular.ttf".into()],
        ),
        (
            "DejaVuSansMono",
            vec!["/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf".into()],
        ),
        (
            "Menlo",
            vec![
                "/System/Library/Fonts/Menlo.ttc".into(),
                "/System/Library/Fonts/Monaco.ttf".into(),
            ],
        ),
    ]);
    for (name, paths) in &mono_candidates {
        if let Some(data) = load_first(paths) {
            tracing::info!(font = *name, "using system monospace font");
            fonts.font_data.insert((*name).to_string(), Arc::new(data));
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, (*name).to_string());
            break;
        }
    }

    fonts
}

/// Ask Fontconfig for the user's configured generic family. This respects
/// KDE Plasma's font settings, per-user ~/.config/fontconfig rules and distro
/// aliases instead of assuming a particular Noto/DejaVu installation path.
#[cfg(target_os = "linux")]
fn fontconfig_match(pattern: &str) -> Option<String> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\n", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!path.is_empty() && std::path::Path::new(&path).is_file()).then_some(path)
}

fn load_first(paths: &[String]) -> Option<FontData> {
    for path in paths {
        if let Ok(bytes) = std::fs::read(path) {
            return Some(FontData::from_owned(bytes));
        }
    }
    None
}
