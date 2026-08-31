//! Theme: Windows-11-Task-Manager palette for dark and light mode plus the
//! blue heat-map gradient used by the table value cells.

use eframe::egui::{self, Color32, CornerRadius, FontId, Visuals};

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
    /// Base navy behind every active row's numeric cells.
    pub heat_base: Color32,
    /// Heat gradient stops (low → high utilization), blue like Win11 TM.
    pub heat_low: Color32,
    pub heat_high: Color32,
    /// Highlight fill for the top-consumer cell of each resource column.
    pub heat_top: Color32,
    /// Thin separators drawn between heat cells inside the blue band.
    pub heat_sep: Color32,
    pub ok_green: Color32,
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
    heat_base: Color32::from_rgb(0x11, 0x24, 0x3e),
    heat_low: Color32::from_rgb(0x1c, 0x30, 0x58),
    heat_high: Color32::from_rgb(0x3f, 0x76, 0xd0),
    heat_top: Color32::from_rgb(0x08, 0x33, 0x6e),
    heat_sep: Color32::from_rgb(0x29, 0x32, 0x3f),
    ok_green: Color32::from_rgb(0x6c, 0xcb, 0x6f),
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
    heat_base: Color32::from_rgb(0xea, 0xf1, 0xfb),
    heat_low: Color32::from_rgb(0xd8, 0xe8, 0xfa),
    heat_high: Color32::from_rgb(0x9f, 0xc8, 0xf0),
    heat_top: Color32::from_rgb(0xc5, 0xdd, 0xf7),
    heat_sep: Color32::from_rgb(0xc8, 0xc8, 0xc8),
    ok_green: Color32::from_rgb(0x0f, 0x7b, 0x0f),
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

/// Re-install custom visuals when the active dark/light mode changed.
/// Cheap enough to call every frame.
pub fn ensure_visuals(ctx: &egui::Context) {
    // Track the dark flag per theme; reinstall custom visuals when it changed.
    let dark_dark = ctx.style_of(egui::Theme::Dark).visuals.dark_mode;
    let applied = ctx.data(|d| d.get_temp::<bool>(egui::Id::new("tm-visuals-dark")));
    if applied != Some(dark_dark) {
        install_visuals(ctx);
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("tm-visuals-dark"), dark_dark));
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
            apply_text_aa(&mut style.visuals.text_options);
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
            // Win11-style overlay scroll bars: always visible as a thin
            // handle, expanding on hover, floating ABOVE the content without
            // reserving layout space — this keeps the table header and body
            // column layouts pixel-aligned whether or not a bar is shown.
            style.spacing.scroll = egui::style::ScrollStyle {
                floating: true,
                bar_width: 12.0,
                floating_width: 5.0,
                floating_allocated_width: 0.0,
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

/// Text antialiasing tuned for Windows' ClearType look.
///
/// What "ClearType" maps to inside egui (which does grayscale AA only —
/// true RGB-subpixel rendering is upstream issue emilk/egui#2639):
/// * `font_hinting` stays on (default): stems snap toward the pixel grid,
///   like ClearType's hinted rendering.
/// * Glyph positions snap to physical pixels (`TessellationOptions`
///   `round_text_to_pixels`, also default-on).
/// * `subpixel_binning` goes OFF: it renders every glyph at four fractional
///   x-offsets which then get bilinearly sampled — softer edges, blurrier
///   small text. Off = crisper stems at native scale, at the cost of
///   slightly less even kerning (the classic crisp-Windows tradeoff).
///   Restore binning with `TASKMAN_TEXT_BINNING=1` on fractional-DPI
///   displays where evenness matters more than sharpness.
/// * The glyph-coverage→alpha curve keeps egui's per-mode defaults
///   (light: linear; dark: 2·c−c², which keeps bright-on-dark text sharp).
fn apply_text_aa(opts: &mut egui::epaint::TextOptions) {
    static BINNING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    opts.subpixel_binning = *BINNING.get_or_init(|| {
        std::env::var("TASKMAN_TEXT_BINNING")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    });
    // Hinting is already the default; assert the intent so a future default
    // flip cannot silently soften our text.
    opts.font_hinting = true;
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

/// Map normalized intensity [0..=1] to the blue heat gradient. Kept for
/// future per-value heat rendering (tables currently use the flat TM style:
/// base fill + brighter top-consumer cell only).
#[allow(dead_code)]
pub fn heat_blue(pal: &Palette, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = t * t; // ease-in: small values stay near the base navy
    let lerp = |a: Color32, b: Color32, f: f32| -> Color32 {
        Color32::from_rgba_premultiplied(
            (a.r() as f32 + (b.r() as f32 - a.r() as f32) * f) as u8,
            (a.g() as f32 + (b.g() as f32 - a.g() as f32) * f) as u8,
            (a.b() as f32 + (b.b() as f32 - a.b() as f32) * f) as u8,
            255,
        )
    };
    lerp(pal.heat_low, pal.heat_high, f)
}

/// Palette for a context (outside of any Ui).
pub fn palette_ctx(ctx: &egui::Context) -> Palette {
    match ctx.theme() {
        egui::Theme::Dark => DARK,
        egui::Theme::Light => LIGHT,
    }
}
