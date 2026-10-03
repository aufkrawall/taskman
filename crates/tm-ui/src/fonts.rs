//! Font setup: prefer OS-native fonts for a native look (Segoe UI Variable
//! on Windows 11, SF Pro via SFNS on macOS, Fontconfig-selected faces on
//! Linux), fall back to known distro paths and then egui's bundled defaults.
//!
//! System font files are read on a background thread so startup stays fast.
//!
//! ## Why the hinting tweak matters more than anything else here
//!
//! epaint rasterizes through `skrifa`, which runs the font's real TrueType
//! instructions — but epaint's DEFAULT `SmoothHinting` sets both
//! `preserve_linear_metrics: true` and `symmetric_rendering: true`, and both
//! of those switch horizontal grid-fitting OFF. Vertical stems then land on
//! fractional pixel columns and get split across two grey columns, which is
//! the "blurry, fat" look. Windows grid-fits horizontally, which is what puts
//! a stem fully inside one pixel column. [`hinting_target`] turns that back on
//! for the sharp profile.

use egui::epaint::text::{FontTweak, HintingTarget, SmoothHinting, VariationCoords};
use egui::{self, FontData, FontDefinitions};
use std::sync::Arc;
use tm_core::settings::TextSmoothing;

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

/// Re-apply the system fonts with the current [`TextSmoothing`] tweak.
///
/// The hinting target lives in each face's [`FontTweak`], not in the global
/// text options, so changing the smoothing profile has to hand egui a fresh
/// [`FontDefinitions`]. The parsed font FILES are reused; only the tweak
/// changes, and this runs on a settings click, not per frame.
pub fn reapply(ctx: &egui::Context) {
    ctx.set_fonts(build_definitions());
}

/// Grid-fitting profile for the active smoothing choice.
///
/// `preserve_linear_metrics: false` + `symmetric_rendering: false` is the
/// combination epaint documents as "lets the (auto)hinter snap horizontally
/// for crisper stems"; egui positions glyphs from the shaper's advances, so
/// this changes sharpness, not layout.
fn hinting_target(smoothing: TextSmoothing) -> HintingTarget {
    match smoothing {
        TextSmoothing::Sharp => HintingTarget::Smooth(SmoothHinting {
            light: false,
            symmetric_rendering: false,
            preserve_linear_metrics: false,
        }),
        TextSmoothing::Standard | TextSmoothing::Smooth => HintingTarget::default(),
    }
}

/// Tweak applied to every face we install.
fn tweak(smoothing: TextSmoothing, coords: VariationCoords) -> FontTweak {
    FontTweak {
        hinting_target: hinting_target(smoothing),
        coords,
        ..Default::default()
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
    let smoothing = crate::theme::text_smoothing();
    let mut fonts = FontDefinitions::default();

    // Variable-font axes for Segoe UI Variable: regular weight at the "Text"
    // optical size, which is the face Windows 11 itself uses for UI chrome.
    // `opsz` 10.5 is the font's own default; naming it keeps us independent of
    // that default and documents the intent.
    let ui_coords = || VariationCoords::new([(b"wght", 400.0), (b"opsz", 10.5)]);

    let mut proportional_candidates: Vec<(&str, Vec<String>, VariationCoords)> = Vec::new();
    #[cfg(target_os = "linux")]
    if let Some(path) = fontconfig_match("sans-serif") {
        proportional_candidates.push(("FontconfigSans", vec![path], VariationCoords::default()));
    }
    proportional_candidates.extend([
        // Windows 11's actual UI face first; `segoeui.ttf` is the Windows 10
        // static fallback.
        (
            "SegoeUIVariable",
            vec![r"C:\Windows\Fonts\SegUIVar.ttf".into()],
            ui_coords(),
        ),
        (
            "SegoeUI",
            vec![r"C:\Windows\Fonts\segoeui.ttf".into()],
            VariationCoords::default(),
        ),
        (
            "SFNS",
            vec!["/System/Library/Fonts/SFNS.ttf".into()],
            VariationCoords::default(),
        ),
        (
            "NotoSans",
            vec![
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf".into(),
                "/usr/share/fonts/TTF/NotoSans-Regular.ttf".into(),
                "/usr/share/noto-cjk/NotoSansCJK-Regular.ttc".into(),
            ],
            VariationCoords::default(),
        ),
        (
            "DejaVuSans",
            vec!["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into()],
            VariationCoords::default(),
        ),
    ]);

    for (name, paths, coords) in &proportional_candidates {
        if let Some(data) = load_first(paths) {
            tracing::info!(font = *name, "using system proportional font");
            let data = data.tweak(tweak(smoothing, coords.clone()));
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
        (
            "SegoeUI-Bold",
            vec![r"C:\Windows\Fonts\segoeuib.ttf".into()],
        ),
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
            let data = data.tweak(tweak(smoothing, VariationCoords::default()));
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
        (
            "CascadiaMono",
            vec![r"C:\Windows\Fonts\CascadiaMono.ttf".into()],
        ),
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
            let data = data.tweak(tweak(smoothing, VariationCoords::default()));
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
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // epaint panics on a face it cannot parse, and Fontconfig can answer
        // with PCF, Type 1 or other non-sfnt files; try the next candidate.
        if is_loadable_font(&bytes) {
            return Some(FontData::from_owned(bytes));
        }
        tracing::warn!(path = %path, "skipping font that is not TrueType/OpenType");
    }
    None
}

/// Whether `bytes` pass the header check epaint's parser (skrifa) starts
/// with: an sfnt table directory (TrueType or CFF-flavoured OpenType), or a
/// TrueType collection whose first face, the index epaint loads, is one.
fn is_loadable_font(bytes: &[u8]) -> bool {
    match bytes.get(..4) {
        Some(b"ttcf") => bytes
            .get(12..16)
            .and_then(|offset| usize::try_from(u32::from_be_bytes(offset.try_into().ok()?)).ok())
            .is_some_and(|offset| is_sfnt_face(bytes, offset)),
        _ => is_sfnt_face(bytes, 0),
    }
}

fn is_sfnt_face(bytes: &[u8], at: usize) -> bool {
    let Some(header) = at.checked_add(12).and_then(|end| bytes.get(at..end)) else {
        return false;
    };
    let version_ok = matches!(&header[..4], b"\0\x01\0\0" | b"true" | b"OTTO");
    let tables = usize::from(u16::from_be_bytes([header[4], header[5]]));
    version_ok
        && tables > 0
        && (at + 12)
            .checked_add(tables * 16)
            .is_some_and(|end| end <= bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal sfnt header with `tables` directory records.
    fn sfnt(version: &[u8; 4], tables: u16) -> Vec<u8> {
        let mut out = version.to_vec();
        out.extend(tables.to_be_bytes());
        out.extend([0; 6]);
        out.extend(vec![0; usize::from(tables) * 16]);
        out
    }

    #[test]
    fn truetype_and_opentype_faces_are_loadable() {
        assert!(is_loadable_font(&sfnt(b"\0\x01\0\0", 3)));
        assert!(is_loadable_font(&sfnt(b"OTTO", 1)));
        assert!(is_loadable_font(&sfnt(b"true", 1)));
    }

    #[test]
    fn non_sfnt_and_truncated_files_are_rejected() {
        // PCF bitmap, Type 1 (PFB and PFA), WOFF2, empty file.
        assert!(!is_loadable_font(b"\x01fcp\x0f\0\0\0"));
        assert!(!is_loadable_font(b"\x80\x01\x10\x07\0\0%!PS-AdobeFont-1.0"));
        assert!(!is_loadable_font(b"%!PS-AdobeFont-1.0: Utopia"));
        assert!(!is_loadable_font(&sfnt(b"wOF2", 1)));
        assert!(!is_loadable_font(b""));
        // Table directory claims more records than the file holds.
        let mut short = sfnt(b"\0\x01\0\0", 2);
        short.truncate(short.len() - 1);
        assert!(!is_loadable_font(&short));
        assert!(!is_loadable_font(&sfnt(b"\0\x01\0\0", 0)));
    }

    #[test]
    fn collection_is_judged_by_its_first_face() {
        let collection = |first_face: &[u8]| {
            let mut out = b"ttcf\0\x01\0\0\0\0\0\x01".to_vec();
            out.extend(16u32.to_be_bytes());
            out.extend(first_face);
            out
        };
        assert!(is_loadable_font(&collection(&sfnt(b"\0\x01\0\0", 1))));
        assert!(!is_loadable_font(&collection(b"\x01fcp")));
        assert!(!is_loadable_font(
            b"ttcf\0\x01\0\0\0\0\0\x01\xff\xff\xff\xff"
        ));
    }
}
