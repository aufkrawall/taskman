//! Font setup: prefer OS-native fonts for a native look (Segoe UI on Windows,
//! SF Pro via SFNS on macOS, Noto/DejaVu on Linux), fall back to egui's
//! bundled defaults. Glyphs rasterize at device pixels → sharp at any DPI.

use eframe::egui::{self, FontData, FontDefinitions};
use std::sync::Arc;

pub fn install(ctx: egui::Context) {
    let mut fonts = FontDefinitions::default();

    let mut inserted = 0usize;

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
            inserted += 1;
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
            inserted += 1;
            break;
        }
    }

    tracing::info!(fonts_installed = inserted, "font setup complete");
    ctx.set_fonts(fonts);
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
