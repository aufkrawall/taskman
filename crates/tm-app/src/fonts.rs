//! Font setup: prefer OS-native fonts for a native look (Segoe UI on Windows,
//! SF Pro via SFNS on macOS, Noto/DejaVu on Linux), fall back to egui's
//! bundled defaults. Glyphs rasterize at device pixels → sharp at any DPI.
//!
//! Startup architecture (implement.md §5.3): system font files (megabyte-scale
//! on some systems) are read on a background thread; the first frame renders
//! with egui's embedded defaults, and the system fonts are swapped in once
//! loaded — one controlled relayout with a repaint request, no synchronous
//! disk I/O before first paint.

use eframe::egui::{self, FontData, FontDefinitions};
use std::sync::Arc;

/// Kick off the background load and swap fonts in when ready.
pub fn install_async(ctx: egui::Context) {
    let ctx2 = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("tm-fonts".into())
        .spawn(move || {
            let defs = build_definitions();
            // Hand the result back through the UI thread: egui types are not
            // Send across in a useful way here, so signal + apply next frame.
            *apply_result().lock().unwrap_or_else(|e| e.into_inner()) = Some(defs);
            ctx2.request_repaint();
        });
    if spawned.is_err() {
        // No worker available: fall back to synchronous install so the app
        // still gets its native fonts (rare; startup cost acceptable then).
        let defs = build_definitions();
        ctx.set_fonts(defs);
    }
}

/// One-slot handoff from the loader thread to the UI pass.
fn apply_result() -> &'static std::sync::Mutex<Option<FontDefinitions>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<FontDefinitions>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Called once per frame from the UI: applies loaded fonts exactly once.
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

    // --- proportional text -------------------------------------------------
    let candidates: &[(&str, Vec<String>)] = &[
        (
            "SegoeUI",
            vec![
                r"C:\Windows\Fonts\segoeui.ttf".into(),
                // Segoe UI Variable lives in a different file family.
                r"C:\Windows\Fonts\SegoeUIVar.ttf".into(),
            ],
        ),
        ("SFNS", vec!["/System/Library/Fonts/SFNS.ttf".into()]),
        (
            "NotoSans",
            vec![
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf".into(),
                "/usr/share/fonts/TTF/NotoSans-Regular.ttf".into(),
                "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc".into(),
            ],
        ),
        (
            "DejaVuSans",
            vec!["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into()],
        ),
    ];

    for (name, paths) in candidates {
        if let Some(data) = load_first(paths) {
            tracing::info!(font = name, "using system proportional font");
            fonts.font_data.insert(name.to_string(), Arc::new(data));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, name.to_string());
            break;
        }
    }

    // Bold variant where we can find one.
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
            fonts.font_data.insert(name.to_string(), Arc::new(data));
            // Register as fallback after the primary so headings pick it up.
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push(name.to_string());
            break;
        }
    }

    // --- monospace numbers ---------------------------------------------------
    let mono_candidates: &[(&str, Vec<String>)] = &[
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
    ];
    for (name, paths) in mono_candidates {
        if let Some(data) = load_first(paths) {
            tracing::info!(font = name, "using system monospace font");
            fonts.font_data.insert(name.to_string(), Arc::new(data));
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, name.to_string());
            break;
        }
    }

    fonts
}

fn load_first(paths: &[String]) -> Option<FontData> {
    for path in paths {
        match std::fs::read(path) {
            Ok(bytes) => {
                return Some(FontData::from_owned(bytes));
            }
            Err(_) => continue,
        }
    }
    None
}
