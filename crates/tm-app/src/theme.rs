//! Theme: Windows-11-Task-Manager palette for dark and light mode plus the
//! blue heat-map gradient used by the table value cells.

use eframe::egui::{self, Color32, CornerRadius, Visuals};

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
    pub stroke: Color32,
    pub chart_grid: Color32,
    /// Base navy behind every active row's numeric cells.
    pub heat_base: Color32,
    /// Heat gradient stops (low → high utilization), blue like Win11 TM.
    pub heat_low: Color32,
    pub heat_high: Color32,
    pub ok_green: Color32,
}

pub const DARK: Palette = Palette {
    window_bg: Color32::from_rgb(0x20, 0x20, 0x20),
    panel_bg: Color32::from_rgb(0x27, 0x27, 0x27),
    card_bg: Color32::from_rgb(0x2b, 0x2b, 0x2b),
    card_bg_hover: Color32::from_rgb(0x38, 0x38, 0x38),
    sidebar_bg: Color32::from_rgb(0x1b, 0x1b, 0x1b),
    text: Color32::from_rgb(0xff, 0xff, 0xff),
    text_dim: Color32::from_rgb(0x9d, 0x9d, 0x9d),
    accent: Color32::from_rgb(0x4c, 0xc2, 0xff),
    accent_text: Color32::from_rgb(0x00, 0x1b, 0x2e),
    stroke: Color32::from_rgb(0x38, 0x38, 0x38),
    chart_grid: Color32::from_rgb(0x37, 0x37, 0x37),
    heat_base: Color32::from_rgb(0x1a, 0x2a, 0x4a),
    heat_low: Color32::from_rgb(0x1c, 0x30, 0x58),
    heat_high: Color32::from_rgb(0x3f, 0x76, 0xd0),
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
    stroke: Color32::from_rgb(0xdd, 0xdd, 0xdd),
    chart_grid: Color32::from_rgb(0xe2, 0xe2, 0xe2),
    heat_base: Color32::from_rgb(0xea, 0xf1, 0xfb),
    heat_low: Color32::from_rgb(0xd8, 0xe8, 0xfa),
    heat_high: Color32::from_rgb(0x9f, 0xc8, 0xf0),
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
            style.visuals.window_corner_radius = CornerRadius::same(8);
            style.visuals.menu_corner_radius = CornerRadius::same(8);
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 4.0);
        });
    }
}

pub fn dark_visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = DARK.panel_bg;
    v.extreme_bg_color = DARK.window_bg;
    v.faint_bg_color = Color32::from_rgb(0x2a, 0x2a, 0x2a);
    v.window_fill = Color32::from_rgb(0x2b, 0x2b, 0x2b);
    v.selection.bg_fill = DARK.accent.gamma_multiply(0.35);
    v.selection.stroke = egui::Stroke::new(1.0, DARK.accent);
    v.widgets.inactive.bg_fill = DARK.card_bg;
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
    v.widgets.inactive.bg_fill = LIGHT.card_bg;
    v
}

/// Map normalized intensity [0..=1] to the blue heat gradient
/// (Win11 Task Manager style value-cell background).
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
