//! Theme: Win11-Task-Manager-inspired palette for dark and light mode,
//! heatmap gradient helpers and accent colors.

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
    /// Heat gradient stops (low → high utilization).
    pub heat_low: Color32,
    pub heat_mid: Color32,
    pub heat_high: Color32,
    pub ok_green: Color32,
}

pub const DARK: Palette = Palette {
    window_bg: Color32::from_rgb(0x20, 0x20, 0x20),
    panel_bg: Color32::from_rgb(0x27, 0x27, 0x27),
    card_bg: Color32::from_rgb(0x2d, 0x2d, 0x2d),
    card_bg_hover: Color32::from_rgb(0x35, 0x35, 0x35),
    sidebar_bg: Color32::from_rgb(0x1c, 0x1c, 0x1c),
    text: Color32::from_rgb(0xf0, 0xf0, 0xf0),
    text_dim: Color32::from_rgb(0x9a, 0x9a, 0x9a),
    accent: Color32::from_rgb(0x4c, 0xc2, 0xff),
    accent_text: Color32::from_rgb(0x00, 0x1b, 0x2e),
    stroke: Color32::from_rgb(0x3a, 0x3a, 0x3a),
    chart_grid: Color32::from_rgb(0x3c, 0x3c, 0x3c),
    heat_low: Color32::from_rgba_premultiplied(60, 120, 70, 90),
    heat_mid: Color32::from_rgba_premultiplied(140, 130, 40, 100),
    heat_high: Color32::from_rgba_premultiplied(160, 55, 45, 120),
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
    heat_low: Color32::from_rgba_premultiplied(190, 230, 195, 110),
    heat_mid: Color32::from_rgba_premultiplied(245, 220, 150, 130),
    heat_high: Color32::from_rgba_premultiplied(240, 170, 155, 150),
    ok_green: Color32::from_rgb(0x0f, 0x7b, 0x0f),
};

/// Active palette derived from egui's dark-mode flag.
pub fn palette(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode {
        DARK
    } else {
        LIGHT
    }
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

/// Map normalized intensity [0..=1] to the heat gradient.
pub fn heat_color(pal: &Palette, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: Color32, b: Color32, f: f32| -> Color32 {
        let f = f.clamp(0.0, 1.0);
        Color32::from_rgba_premultiplied(
            (a.r() as f32 + (b.r() as f32 - a.r() as f32) * f) as u8,
            (a.g() as f32 + (b.g() as f32 - a.g() as f32) * f) as u8,
            (a.b() as f32 + (b.b() as f32 - a.b() as f32) * f) as u8,
            (a.a() as f32 + (b.a() as f32 - a.a() as f32) * f) as u8,
        )
    };
    if t < 0.5 {
        lerp(pal.heat_low, pal.heat_mid, t * 2.0)
    } else {
        lerp(pal.heat_mid, pal.heat_high, (t - 0.5) * 2.0)
    }
}

/// Palette for a context (outside of any Ui).
pub fn palette_ctx(ctx: &egui::Context) -> Palette {
    match ctx.theme() {
        egui::Theme::Dark => DARK,
        egui::Theme::Light => LIGHT,
    }
}
