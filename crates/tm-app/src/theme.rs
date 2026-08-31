//! Theme: Windows-11-Task-Manager palette for dark and light mode plus the
//! blue heat-map gradient used by the table value cells.

use std::sync::atomic::{AtomicU8, Ordering};

use eframe::egui::{self, Color32, CornerRadius, FontId, Visuals};
use tm_core::settings::TextSmoothing;

/// Active glyph-weight choice. Process-global like the UI language, because
/// [`install_visuals`] is called from contexts that do not carry settings
/// (startup, per-frame theme re-check).
static SMOOTHING: AtomicU8 = AtomicU8::new(0);

fn smoothing_code(v: TextSmoothing) -> u8 {
    match v {
        TextSmoothing::Sharp => 0,
        TextSmoothing::Standard => 1,
        TextSmoothing::Smooth => 2,
    }
}

/// Publish the user's glyph-weight choice. Returns true when it changed, so
/// the caller can reinstall visuals; egui rebuilds the glyph atlas by itself
/// once the new text options reach it, so no restart is needed.
pub fn set_text_smoothing(v: TextSmoothing) -> bool {
    SMOOTHING.swap(smoothing_code(v), Ordering::Relaxed) != smoothing_code(v)
}

pub fn text_smoothing() -> TextSmoothing {
    match SMOOTHING.load(Ordering::Relaxed) {
        1 => TextSmoothing::Standard,
        2 => TextSmoothing::Smooth,
        _ => TextSmoothing::Sharp,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub window_bg: Color32,
    pub panel_bg: Color32,
    pub card_bg: Color32,
    pub card_bg_hover: Color32,
    pub sidebar_bg: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub cpu_graph: Color32,
    pub memory_graph: Color32,
    pub disk_graph: Color32,
    pub network_graph: Color32,
    pub gpu_graph: Color32,
    pub stroke: Color32,
    pub chart_grid: Color32,
    /// Floor of the heat gradient: the tint EVERY numeric cell carries, even
    /// at zero. Win11 TM has no uncolored value cells, so neither do we.
    pub heat_base: Color32,
    /// Top of the heat gradient (the column's busiest process).
    pub heat_high: Color32,
    /// Thin separators drawn between heat cells inside the blue band.
    pub heat_sep: Color32,
    /// Efficiency-mode leaf.
    pub ok_green: Color32,
    /// Suspended/paused status glyph.
    pub warn_orange: Color32,
}

pub const DARK: Palette = Palette {
    // Win11 TM dark: content area 0x191919, sidebar/chrome slightly lighter.
    window_bg: Color32::from_rgb(0x19, 0x19, 0x19),
    panel_bg: Color32::from_rgb(0x20, 0x20, 0x20),
    card_bg: Color32::from_rgb(0x2b, 0x2b, 0x2b),
    card_bg_hover: Color32::from_rgb(0x38, 0x38, 0x38),
    sidebar_bg: Color32::from_rgb(0x20, 0x20, 0x20),
    text: Color32::from_rgb(0xff, 0xff, 0xff),
    text_dim: Color32::from_rgb(0x9d, 0x9d, 0x9d),
    accent: Color32::from_rgb(0x4c, 0xc2, 0xff),
    accent_text: Color32::from_rgb(0x00, 0x1b, 0x2e),
    cpu_graph: Color32::from_rgb(0x4c, 0xc2, 0xff),
    memory_graph: Color32::from_rgb(0xa9, 0x72, 0xe8),
    disk_graph: Color32::from_rgb(0x6f, 0xc2, 0x68),
    network_graph: Color32::from_rgb(0xdc, 0x72, 0xb8),
    gpu_graph: Color32::from_rgb(0xe0, 0x91, 0x4f),
    stroke: Color32::from_rgb(0x2d, 0x2d, 0x2d),
    chart_grid: Color32::from_rgb(0x37, 0x37, 0x37),
    heat_base: Color32::from_rgb(0x14, 0x27, 0x40),
    heat_high: Color32::from_rgb(0x2e, 0x6f, 0xc4),
    heat_sep: Color32::from_rgb(0x29, 0x32, 0x3f),
    ok_green: Color32::from_rgb(0x6c, 0xcb, 0x6f),
    warn_orange: Color32::from_rgb(0xf2, 0xa2, 0x3c),
};

pub const LIGHT: Palette = Palette {
    window_bg: Color32::from_rgb(0xf3, 0xf3, 0xf3),
    panel_bg: Color32::from_rgb(0xfb, 0xfb, 0xfb),
    card_bg: Color32::from_rgb(0xff, 0xff, 0xff),
    card_bg_hover: Color32::from_rgb(0xf0, 0xf6, 0xfc),
    sidebar_bg: Color32::from_rgb(0xe9, 0xe9, 0xe9),
    text: Color32::from_rgb(0x1a, 0x1a, 0x1a),
    text_dim: Color32::from_rgb(0x5f, 0x5f, 0x5f),
    accent: Color32::from_rgb(0x00, 0x78, 0xd4),
    accent_text: Color32::WHITE,
    cpu_graph: Color32::from_rgb(0x00, 0x78, 0xd4),
    memory_graph: Color32::from_rgb(0x74, 0x4d, 0xa9),
    disk_graph: Color32::from_rgb(0x2f, 0x82, 0x2f),
    network_graph: Color32::from_rgb(0x9b, 0x3f, 0x82),
    gpu_graph: Color32::from_rgb(0xa8, 0x5a, 0x20),
    stroke: Color32::from_rgb(0xdd, 0xdd, 0xdd),
    chart_grid: Color32::from_rgb(0xe2, 0xe2, 0xe2),
    heat_base: Color32::from_rgb(0xef, 0xf5, 0xfd),
    heat_high: Color32::from_rgb(0x8a, 0xba, 0xea),
    heat_sep: Color32::from_rgb(0xc8, 0xc8, 0xc8),
    ok_green: Color32::from_rgb(0x0f, 0x7b, 0x0f),
    warn_orange: Color32::from_rgb(0xb4, 0x6b, 0x00),
};

/// Active palette derived from egui's dark-mode flag.
pub fn palette(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode { DARK } else { LIGHT }
}

pub fn apply_startup(ctx: &egui::Context) {
    // Follow the OS theme preference by default; user override handled in settings.
    ctx.set_theme(egui::ThemePreference::System);
    install_visuals(ctx);
}

/// Force the glyph atlas to be rebuilt with the current text options.
/// `install_visuals` alone is enough — egui compares the incoming
/// `TextOptions` against the atlas's and recreates it on a difference — but
/// the repaint request makes the change visible immediately instead of at the
/// next unrelated event.
pub fn refresh_text_rendering(ctx: &egui::Context) {
    install_visuals(ctx);
    ctx.request_repaint();
}

/// Re-install custom visuals when the active dark/light mode or the glyph
/// weight changed. Cheap enough to call every frame.
pub fn ensure_visuals(ctx: &egui::Context) {
    let state = (
        ctx.style_of(egui::Theme::Dark).visuals.dark_mode,
        SMOOTHING.load(Ordering::Relaxed),
    );
    let applied = ctx.data(|d| d.get_temp::<(bool, u8)>(egui::Id::new("tm-visuals-dark")));
    if applied != Some(state) {
        install_visuals(ctx);
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("tm-visuals-dark"), state));
    }
}

pub fn install_visuals(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            if theme == egui::Theme::Dark {
                style.visuals = dark_visuals();
            } else {
                style.visuals = light_visuals();
            }
            apply_text_aa(&mut style.visuals.text_options, theme);
            // Centralized text-style ladder so egui widgets and the
            // hand-painted chrome share one scale, matched to Win11 TM's
            // measured sizes: body/button/table text 13, dialog section
            // headings = tab titles 15.5, monospace 12.0 (debug overlay).
            // Without this, egui's defaults (13.0/18.0) leak into every
            // dialog, menu and label next to 13 px chrome.
            style.text_styles = [
                (egui::TextStyle::Small, FontId::proportional(11.0)),
                (egui::TextStyle::Body, FontId::proportional(13.0)),
                (egui::TextStyle::Button, FontId::proportional(13.0)),
                (egui::TextStyle::Heading, FontId::proportional(15.5)),
                (egui::TextStyle::Monospace, FontId::proportional(12.0)),
            ]
            .into();
            style.visuals.window_corner_radius = CornerRadius::same(8);
            style.visuals.menu_corner_radius = CornerRadius::same(8);
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 4.0);
            // Win11-style scroll bars: a thin idle handle that expands on
            // hover — but with their full lane RESERVED, never painted over
            // the content.
            //
            // `floating` here means "draw like an overlay bar" (thin handle,
            // hover expansion, foreground color). `floating_allocated_width`
            // is what decides whether the bar costs layout space: at 0 the
            // bar sits INSIDE the viewport and covers the last ~14 px of
            // every row, card and dialog it belongs to. Reserving the full
            // bar width (`bar_width` + `bar_outer_margin`) moves it just
            // outside the content rect, so nothing is ever occluded and the
            // handle still animates within its own lane.
            //
            // Do NOT "simplify" this to `floating: false`. egui decides
            // whether a bar is needed against the OUTER rect for floating
            // bars and against the shrunken INNER rect for solid ones — with
            // solid bars, content whose height depends on its width (wrapped
            // labels) can flip the bar on and off every frame.
            style.spacing.scroll = egui::style::ScrollStyle {
                floating: true,
                bar_width: 12.0,
                floating_width: 5.0,
                floating_allocated_width: 14.0,
                bar_inner_margin: 2.0,
                bar_outer_margin: 2.0,
                foreground_color: true,
                // Visible while idle — a scrollbar you can't see is useless.
                dormant_handle_opacity: 0.5,
                dormant_background_opacity: 0.0,
                active_handle_opacity: 0.65,
                active_background_opacity: 0.30,
                interact_handle_opacity: 1.0,
                interact_background_opacity: 0.45,
                ..Default::default()
            };
        });
    }
}

/// Glyph anti-aliasing, as close to the Windows UI look as egui allows.
///
/// IMPORTANT — what is NOT possible here: egui stores every glyph in a
/// single-channel coverage atlas and tints it in the shader, so there is no
/// per-channel (RGB sub-pixel) coverage anywhere in the pipeline. True
/// ClearType would need a 3-channel atlas plus dual-source blending in both
/// the glow and wgpu backends; upstream tracks this as emilk/egui#2639.
/// Everything below therefore tunes GRAYSCALE anti-aliasing.
///
/// What we do control:
/// * `font_hinting` stays on: stems snap toward the pixel grid, like
///   ClearType's hinted rendering.
/// * Glyph positions snap to physical pixels (`TessellationOptions`
///   `round_text_to_pixels`, default-on).
/// * `subpixel_binning` caches each glyph at four fractional x-offsets which
///   are then bilinearly sampled — more even spacing, softer stems. Only
///   [`TextSmoothing::Smooth`] asks for it.
/// * The coverage→alpha ramp decides stroke WEIGHT. egui's dark-mode default
///   `2c − c²` lifts every partially covered pixel (0.5 → 0.75), which reads
///   as fat and blurry next to native Windows text;
///   [`TextSmoothing::Sharp`] uses raw coverage instead.
fn apply_text_aa(opts: &mut egui::epaint::TextOptions, theme: egui::Theme) {
    use egui::epaint::FontColorTransferFunction as Ramp;
    let smoothing = env_smoothing().unwrap_or_else(text_smoothing);
    let per_theme = match theme {
        egui::Theme::Dark => Ramp::DARK_MODE_DEFAULT,
        egui::Theme::Light => Ramp::LIGHT_MODE_DEFAULT,
    };
    let (ramp, binning) = match smoothing {
        TextSmoothing::Sharp => (Ramp::Off, false),
        TextSmoothing::Standard => (per_theme, false),
        TextSmoothing::Smooth => (per_theme, true),
    };
    opts.color_transfer_function = ramp;
    opts.subpixel_binning = binning;
    // Hinting is already the default; assert the intent so a future default
    // flip cannot silently soften our text.
    opts.font_hinting = true;
}

/// `TASKMAN_TEXT_SMOOTHING=sharp|standard|smooth` overrides the setting for
/// A/B comparisons without touching the user's config.
fn env_smoothing() -> Option<TextSmoothing> {
    static OVERRIDE: std::sync::OnceLock<Option<TextSmoothing>> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        let raw = std::env::var("TASKMAN_TEXT_SMOOTHING").ok()?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "sharp" => Some(TextSmoothing::Sharp),
            "standard" => Some(TextSmoothing::Standard),
            "smooth" => Some(TextSmoothing::Smooth),
            _ => None,
        }
    })
}

pub fn dark_visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = DARK.panel_bg;
    v.extreme_bg_color = DARK.window_bg;
    v.faint_bg_color = Color32::from_rgb(0x24, 0x24, 0x24);
    v.window_fill = Color32::from_rgb(0x2b, 0x2b, 0x2b);
    v.selection.bg_fill = DARK.accent.gamma_multiply(0.35);
    v.selection.stroke = egui::Stroke::new(1.0, DARK.accent);
    // Checkbox/radio/slider backgrounds MUST contrast with the window fill
    // (0x2b2b2b): a same-color fill with no stroke renders the control box
    // invisible until the first hover switches to the `hovered` visuals.
    // Keep `weak_bg_fill` for buttons; give must-fill widgets their own look.
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x37, 0x37, 0x37);
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(0x76, 0x76, 0x76));
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.6, Color32::from_rgb(0xd4, 0xd4, 0xd4));
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x3a);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(0x9a, 0x9a, 0x9a));
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.6, Color32::WHITE);
    v.widgets.active.bg_fill = Color32::from_rgb(0x40, 0x40, 0x40);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, DARK.accent);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.6, Color32::WHITE);
    v.override_text_color = Some(DARK.text);
    v
}

pub fn light_visuals() -> Visuals {
    let mut v = Visuals::light();
    v.panel_fill = LIGHT.panel_bg;
    v.extreme_bg_color = LIGHT.window_bg;
    v.faint_bg_color = Color32::from_rgb(0xf6, 0xf6, 0xf6);
    v.window_fill = Color32::WHITE;
    v.selection.bg_fill = LIGHT.accent.gamma_multiply(0.30);
    v.selection.stroke = egui::Stroke::new(1.0, LIGHT.accent);
    // Same treatment as dark mode: visible border + fill for must-fill
    // widgets so checkboxes never blend into the dialog background.
    v.widgets.inactive.bg_fill = Color32::WHITE;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(0x8a, 0x8a, 0x8a));
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.6, Color32::from_rgb(0x33, 0x33, 0x33));
    v.widgets.hovered.bg_fill = Color32::from_rgb(0xf0, 0xf6, 0xfc);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, LIGHT.accent);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.6, Color32::from_rgb(0x11, 0x11, 0x11));
    v.widgets.active.bg_fill = Color32::from_rgb(0xe2, 0xee, 0xfa);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, LIGHT.accent);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.6, Color32::BLACK);
    v
}

/// Map a normalized per-column intensity `[0..=1]` to the blue heat gradient
/// used by every numeric table cell.
///
/// Two deliberate properties, both taken from Win11 TM:
/// * `t == 0` still returns `heat_base` — an idle process shows a pale blue
///   cell, never an unpainted hole in the band.
/// * The ramp is ease-OUT (`sqrt`), not ease-in. Intensities are normalized
///   against the column MAXIMUM, so with one busy process the entire rest of
///   the column sits near zero; an ease-in curve collapsed all of them onto
///   the base tint and made the heat map look binary.
pub fn heat_blue(pal: &Palette, t: f32) -> Color32 {
    let f = t.clamp(0.0, 1.0).sqrt();
    lerp_rgb(pal.heat_base, pal.heat_high, f)
}

fn lerp_rgb(a: Color32, b: Color32, f: f32) -> Color32 {
    let ch = |a: u8, b: u8| {
        (a as f32 + (b as f32 - a as f32) * f)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

/// Palette for a context (outside of any Ui).
pub fn palette_ctx(ctx: &egui::Context) -> Palette {
    match ctx.theme() {
        egui::Theme::Dark => DARK,
        egui::Theme::Light => LIGHT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The heat map must have no unpainted floor and must rise smoothly:
    /// an idle process still shows the base tint, and every step up in
    /// intensity is at least as blue as the one below it.
    #[test]
    fn heat_gradient_starts_at_the_base_tint_and_rises_monotonically() {
        for pal in [DARK, LIGHT] {
            assert_eq!(heat_blue(&pal, 0.0), pal.heat_base);
            assert_eq!(heat_blue(&pal, 1.0), pal.heat_high);
            assert_eq!(heat_blue(&pal, -3.0), pal.heat_base, "clamped");
            assert_eq!(heat_blue(&pal, 7.0), pal.heat_high, "clamped");

            let mut previous = heat_blue(&pal, 0.0);
            for step in 1..=20 {
                let next = heat_blue(&pal, step as f32 / 20.0);
                let toward_high = |a: Color32, b: Color32| {
                    // distance to heat_high must never grow
                    let d = |c: Color32| {
                        (c.r() as i32 - pal.heat_high.r() as i32).abs()
                            + (c.g() as i32 - pal.heat_high.g() as i32).abs()
                            + (c.b() as i32 - pal.heat_high.b() as i32).abs()
                    };
                    d(b) <= d(a)
                };
                assert!(toward_high(previous, next), "step {step} regressed");
                previous = next;
            }
        }
    }

    /// A low but non-zero share of the column maximum must be visibly
    /// bluer than an idle cell — the old ease-in curve crushed it back onto
    /// the base tint and made the heat map look binary.
    #[test]
    fn small_intensities_are_visible_above_the_base() {
        for pal in [DARK, LIGHT] {
            let idle = heat_blue(&pal, 0.0);
            let small = heat_blue(&pal, 0.05);
            let delta = (small.r() as i32 - idle.r() as i32).abs()
                + (small.g() as i32 - idle.g() as i32).abs()
                + (small.b() as i32 - idle.b() as i32).abs();
            assert!(delta >= 8, "5% of the column max is invisible: {delta}");
        }
    }
}
