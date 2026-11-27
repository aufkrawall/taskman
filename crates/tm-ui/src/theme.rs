//! Theme: Windows-11-Task-Manager palette for dark and light mode plus the
//! blue heat-map gradient used by the table value cells.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use egui::{self, Color32, CornerRadius, FontFamily, FontId, Visuals};
use tm_core::settings::TextSmoothing;

/// Semibold UI face (`Segoe UI Variable` `wght: 600` on Win11, bold fallback elsewhere).
pub fn font_semibold(size: f32) -> FontId {
    FontId::new(
        size,
        FontFamily::Name(Arc::from(crate::fonts::FAMILY_SEMIBOLD)),
    )
}

/// Display optical-size face for large KPI readouts (`Segoe UI Variable` `opsz: 36` on Win11).
pub fn font_display(size: f32) -> FontId {
    FontId::new(
        size,
        FontFamily::Name(Arc::from(crate::fonts::FAMILY_DISPLAY)),
    )
}

/// Small optical-size face for captions, axis ticks, and sublabels (`opsz: 8` on Win11).
pub fn font_small(size: f32) -> FontId {
    FontId::new(
        size,
        FontFamily::Name(Arc::from(crate::fonts::FAMILY_SMALL)),
    )
}

fn named_family_in(ui: &egui::Ui, name: &'static str, size: f32) -> FontId {
    if matches!(
        ui.style().text_styles.get(&egui::TextStyle::Heading),
        Some(id) if matches!(id.family, FontFamily::Name(_))
    ) {
        FontId::new(size, FontFamily::Name(Arc::from(name)))
    } else {
        FontId::proportional(size)
    }
}

/// Like [`font_semibold`], falling back to `Proportional` in isolated widget
/// tests that run on a bare `egui::Context::default()` without `install_visuals`.
pub fn font_semibold_in(ui: &egui::Ui, size: f32) -> FontId {
    named_family_in(ui, crate::fonts::FAMILY_SEMIBOLD, size)
}

/// Like [`font_display`], with the same bare-context fallback as [`font_semibold_in`].
pub fn font_display_in(ui: &egui::Ui, size: f32) -> FontId {
    named_family_in(ui, crate::fonts::FAMILY_DISPLAY, size)
}

/// Like [`font_small`], with the same bare-context fallback as [`font_semibold_in`].
pub fn font_small_in(ui: &egui::Ui, size: f32) -> FontId {
    named_family_in(ui, crate::fonts::FAMILY_SMALL, size)
}

/// Whether the active renderer can display sub-pixel (`ClearType`) text.
///
/// This is an interlock, not a preference. An atlas rasterized with per-channel coverage
/// is only meaningful to a renderer that blends per channel. The software painter always
/// can; WGPU can only after its device enabled dual-source blending; Glow and unsupported
/// WGPU draw rainbow-tinted text. `main.rs` sets this once the renderer is known.
static SUBPIXEL_CAPABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The platform's text-rendering parameters, refreshed when Windows reports a monitor
/// placement/DPI change.
static SUBPIXEL_PARAMS: std::sync::Mutex<Option<tm_platform::text_rendering::Params>> =
    std::sync::Mutex::new(None);

/// Record that the active renderer blends per channel, and the display's parameters.
pub fn set_subpixel_capable(capable: bool, params: tm_platform::text_rendering::Params) {
    SUBPIXEL_CAPABLE.store(capable, Ordering::Relaxed);
    let _ = set_subpixel_params(params);
}

/// Update the display policy without changing the renderer capability.
///
/// This is used when the window moves to another monitor. The renderer is not recreated:
/// the next frame reads the new parameters from the atlas options and uploads them to the
/// renderer uniform. Returns whether the atlas/options actually need reinstalling.
pub fn set_subpixel_params(params: tm_platform::text_rendering::Params) -> bool {
    let mut current = SUBPIXEL_PARAMS.lock().unwrap_or_else(|e| e.into_inner());
    if *current == Some(params) {
        return false;
    }
    *current = Some(params);
    true
}

/// The sub-pixel mode to rasterize glyphs with, or `Off`.
pub fn subpixel_mode() -> egui::epaint::text::SubpixelMode {
    let params = *SUBPIXEL_PARAMS.lock().unwrap_or_else(|e| e.into_inner());
    resolve_subpixel_mode(
        SUBPIXEL_CAPABLE.load(Ordering::Relaxed),
        env_subpixel(),
        params,
    )
}

fn resolve_subpixel_mode(
    capable: bool,
    forced: Option<bool>,
    params: Option<tm_platform::text_rendering::Params>,
) -> egui::epaint::text::SubpixelMode {
    use egui::epaint::text::SubpixelMode;
    // The override can force off, but can never energize an unsupported renderer or a
    // platform policy whose channel order is unknown.
    if forced == Some(false) || !capable {
        return SubpixelMode::Off;
    }
    match params {
        Some(p) if p.enabled => {
            if p.bgr {
                SubpixelMode::Bgr
            } else {
                SubpixelMode::Rgb
            }
        }
        _ => SubpixelMode::Off,
    }
}

/// The display's text blend parameters, for the software renderer.
pub fn subpixel_params() -> tm_platform::text_rendering::Params {
    (*SUBPIXEL_PARAMS.lock().unwrap_or_else(|e| e.into_inner())).unwrap_or_default()
}

/// `TASKMAN_SUBPIXEL=0` forces sub-pixel text off. `=1` requests the supported policy for
/// A/B comparison, but never overrides a disabled platform policy or renderer capability.
fn env_subpixel() -> Option<bool> {
    static OVERRIDE: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| match std::env::var("TASKMAN_SUBPIXEL").ok()?.trim() {
        "0" | "off" | "false" => Some(false),
        "1" | "on" | "true" => Some(true),
        _ => None,
    })
}

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
    /// Inset surface for a graph cell sitting ON a highlighted card. A
    /// sparkline that keeps `card_bg` while its card is selected or hovered
    /// paints the same colour as the card behind it, so the cell dissolves
    /// into the row exactly when the user is pointing at it.
    pub card_bg_sunken: Color32,
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
    /// Partner colours for the three charts that overlay two series at once.
    ///
    /// These used to be `gamma_multiply(0.6)` of the resource colour, which is
    /// not a shade at all: it scales the ALPHA along with the components, so
    /// both halves of a pair were the same hue and told apart only by how much
    /// background showed through — and the chart's own area fill then thinned
    /// that further. Each partner is now an opaque colour of its own, chosen
    /// to differ in HUE from its primary, which is what still reads at a
    /// glance on a 160 px tall graph.
    pub cpu_kernel_graph: Color32,
    pub disk_write_graph: Color32,
    pub network_send_graph: Color32,
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
    card_bg_sunken: Color32::from_rgb(0x1b, 0x1b, 0x1b),
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
    // Indigo under the cyan total, gold against the green read, teal against
    // the magenta receive.
    cpu_kernel_graph: Color32::from_rgb(0x3a, 0x63, 0xd9),
    disk_write_graph: Color32::from_rgb(0xe8, 0xa0, 0x20),
    network_send_graph: Color32::from_rgb(0x45, 0xc4, 0xb0),
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
    card_bg_sunken: Color32::from_rgb(0xed, 0xed, 0xed),
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
    // Same three hue relationships, re-tuned against a white card: the dark
    // theme's tones would all read as pastel smears on it.
    cpu_kernel_graph: Color32::from_rgb(0x5a, 0x3e, 0xa8),
    disk_write_graph: Color32::from_rgb(0xb0, 0x6a, 0x00),
    network_send_graph: Color32::from_rgb(0x0e, 0x7a, 0x6b),
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

/// The text-rendering half of the visuals: exactly what
/// [`apply_text_aa`] derives from the smoothing setting and the display's
/// ClearType parameters.
///
/// Two different values mean the glyph atlas has to be rasterized again, so
/// this — not just the weight setting — is what [`ensure_visuals`] compares.
/// The parameters can change without the dark/light mode or the weight
/// moving: a monitor with its own ClearType calibration, or the startup
/// query landing after the first frame on a slow display stack.
fn text_aa_state(theme: egui::Theme) -> egui::epaint::TextOptions {
    let mut opts = egui::epaint::TextOptions::default();
    apply_text_aa(&mut opts, theme);
    opts
}

/// Re-install custom visuals when the active dark/light mode or the glyph
/// rendering changed. Cheap enough to call every frame.
pub fn ensure_visuals(ctx: &egui::Context) {
    let state = (
        ctx.style_of(egui::Theme::Dark).visuals.dark_mode,
        text_aa_state(egui::Theme::Dark),
        text_aa_state(egui::Theme::Light),
    );
    let applied = ctx.data(|d| {
        d.get_temp::<(bool, egui::epaint::TextOptions, egui::epaint::TextOptions)>(egui::Id::new(
            "tm-visuals-dark",
        ))
    });
    if applied != Some(state) {
        install_visuals(ctx);
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("tm-visuals-dark"), state));
    }
}

pub fn install_visuals(ctx: &egui::Context) {
    crate::fonts::ensure_named_families(ctx);
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
            // headings = tab titles 15.5 (semibold), small 11.0 (small optical
            // size), monospace 12.0 (debug overlay & diagnostic hex values).
            // Without this, egui's defaults (13.0/18.0) leak into every
            // dialog, menu and label next to 13 px chrome.
            style.text_styles = [
                (egui::TextStyle::Small, font_small(11.0)),
                (egui::TextStyle::Body, FontId::proportional(13.0)),
                (egui::TextStyle::Button, FontId::proportional(13.0)),
                (egui::TextStyle::Heading, font_semibold(15.5)),
                (egui::TextStyle::Monospace, FontId::monospace(12.0)),
            ]
            .into();
            style.visuals.window_corner_radius = CornerRadius::same(8);
            style.visuals.menu_corner_radius = CornerRadius::same(8);
            // Allow tooltip delay to rely on a single scheduled repaint after
            // `tooltip_delay` rather than calling `request_repaint()` every
            // frame while the pointer moves over a tooltipped widget.
            style.interaction.show_tooltips_only_when_still = false;
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
                // No fade-out gradients at the edges of a scroll area. egui
                // paints a 20 px background-coloured ramp at the top and
                // bottom of anything scrollable, which over a dense table of
                // rows reads as a shadow lying on the list — a second,
                // moving edge next to the header and the window frame. Task
                // Manager's lists have hard edges; so do these.
                fade: egui::style::ScrollFadeStyle {
                    strength: 0.0,
                    ..Default::default()
                },
                ..Default::default()
            };
        });
    }
}

/// Glyph anti-aliasing, as close to the Windows UI look as egui allows.
///
/// The fork stores per-channel LCD coverage in the atlas's RGB channels. The software
/// painter always consumes it; WGPU does so through a dual-source font-atlas pipeline
/// when the device enabled that feature. Glow and unsupported WGPU adapters remain on the
/// byte-identical grayscale path, so the renderer interlock above still decides whether
/// LCD rasterization is valid at all.
///
/// For grayscale text (`SubpixelMode::Off`, e.g. RDP, portrait monitors, mixed-monitor
/// straddle, non-dual-source GPUs, or Linux desktops with `rgba=none`):
/// * `font_hinting` stays on unless Linux Fontconfig explicitly disables hinting.
/// * Glyph positions snap to physical pixels (`TessellationOptions`
///   `round_text_to_pixels`, default-on).
/// * `subpixel_binning` caches each glyph at four fractional x-offsets rasterized from
///   shifted vector outlines and blitted 1:1 — more even spacing, softer stems. Only
///   [`TextSmoothing::Smooth`] asks for it.
/// * The coverage→alpha curve (`grayscale_transfer`) is applied by the renderer per draw,
///   by the TEXT colour's polarity (`epaint::polarity_split`): light-on-dark text gets
///   `0.80` Sharp / `0.65` Standard, dark-on-light text `1.15` Sharp / `0.95` Standard --
///   measured against native DirectWrite text (`text_compare`), Sharp keeps ~0.9 of
///   native ink in both themes and Standard ~1.0. A curve chosen by theme would also hit
///   opposite-polarity text -- the Light theme's white toast labels -- with the wrong
///   direction.
fn apply_text_aa(opts: &mut egui::epaint::TextOptions, theme: egui::Theme) {
    let smoothing = env_smoothing().unwrap_or_else(text_smoothing);
    configure_text_options(opts, smoothing, theme, subpixel_mode(), &subpixel_params());
}

/// Every text option the app derives from a smoothing profile, theme, renderer-gated
/// sub-pixel mode and platform parameters.
///
/// The one definition, shared with `text_compare` so the comparison harness renders
/// exactly what the app renders (hinting, filter, blend and grayscale curve alike).
pub fn configure_text_options(
    opts: &mut egui::epaint::TextOptions,
    smoothing: TextSmoothing,
    theme: egui::Theme,
    subpixel: egui::epaint::text::SubpixelMode,
    params: &tm_platform::text_rendering::Params,
) {
    let binning = matches!(smoothing, TextSmoothing::Smooth);
    opts.color_transfer_function = grayscale_transfer(smoothing);
    opts.subpixel_binning = binning;
    // The polarity reference for the grayscale curve, and the GPU LCD blend calibration.
    opts.text_background_luma = text_background_luma(theme);
    opts.text_inverse_background_luma = text_inverse_background_luma(theme);

    // Sub-pixel (ClearType) coverage. Orthogonal to the grayscale weight above: it
    // changes how glyphs are RASTERIZED, and is gated on the active renderer being able
    // to blend per channel at all.
    opts.subpixel = subpixel;
    if !subpixel.is_off() {
        opts.cleartype_level = params.cleartype_level;
        opts.lcd_filter = lcd_filter(smoothing, params.lcd_filter);

        // Under sub-pixel rendering the smoothing profile becomes a WEIGHT and filter
        // control: `Sharp` pairs `LcdFilter::LIGHT` (omitting `FREETYPE_DEFAULT`'s `±2`
        // outer taps when the platform did not specify a filter) with `contrast = 0.0`
        // and polarity-appropriate gamma tuning so 13 px rows stay crisp and lean.
        let (gamma, contrast) = cleartype_weight(smoothing, theme, params);
        opts.text_gamma = gamma;
        opts.text_contrast = contrast;
    }
    // The atlas ceiling is NOT set here. `Context::update_fonts_mut` overwrites
    // `TextOptions::max_texture_side` with `RawInput::max_texture_side` on every
    // pass before the `Fonts` object is built, so a value written into this
    // style would be discarded; the renderer's own integration supplies it (16 K
    // for the software renderer this app defaults to).
    // Respect an explicit Linux Fontconfig `hinting: false` / `hintstyle: hintnone`;
    // otherwise keep hinting on so stems stay crisp.
    opts.font_hinting = params.hint_style != tm_platform::text_rendering::HintStyleChoice::None;
}

/// Calibrated grayscale coverage-to-alpha curve for `SubpixelMode::Off` glyphs.
///
/// Shared with `text_compare` so grayscale comparisons mirror app rendering. The renderer
/// picks the exponent per draw from the text colour's polarity
/// ([`egui::epaint::FontColorTransferFunction::PolarityGamma`]).
///
/// Calibrated against native DirectWrite grayscale text (Windows-default parameters,
/// `text_compare::the_weight_matches_native_directwrite_text`): Sharp keeps ~0.9 of native
/// ink in both themes, Standard ~1.0, at 100/125/150%. Native dark-on-light text is heavier
/// than raw coverage blended in sRGB (DirectWrite's gamma and contrast add weight there),
/// so the dark-text exponents sit near 1, not above it.
pub const fn grayscale_transfer(
    smoothing: TextSmoothing,
) -> egui::epaint::FontColorTransferFunction {
    use egui::epaint::FontColorTransferFunction::PolarityGamma;
    match smoothing {
        TextSmoothing::Sharp => PolarityGamma {
            light_text: 0.80,
            dark_text: 1.15,
        },
        TextSmoothing::Standard | TextSmoothing::Smooth => PolarityGamma {
            light_text: 0.65,
            dark_text: 0.95,
        },
    }
}

/// Map platform LCD filter preference and smoothing profile to epaint's 5-tap filter.
///
/// Shared with `text_compare` so the comparison harness mirrors app rendering.
pub fn lcd_filter(
    smoothing: TextSmoothing,
    choice: tm_platform::text_rendering::LcdFilterChoice,
) -> egui::epaint::LcdFilter {
    use tm_platform::text_rendering::LcdFilterChoice;
    match (smoothing, choice) {
        // When the platform did not specify an explicit filter (e.g. Windows), `Sharp`
        // uses `LIGHT` to omit `FREETYPE_DEFAULT`'s `±2` sub-pixel taps that otherwise
        // fatten 1px stems at 100% scaling. An explicit Linux Fontconfig `lcdfilter: default`
        // is respected as `FREETYPE_DEFAULT`.
        (TextSmoothing::Sharp, LcdFilterChoice::PlatformDefault) => egui::epaint::LcdFilter::LIGHT,
        (_, LcdFilterChoice::PlatformDefault | LcdFilterChoice::Default) => {
            egui::epaint::LcdFilter::FREETYPE_DEFAULT
        }
        (_, LcdFilterChoice::Light) => egui::epaint::LcdFilter::LIGHT,
        // fontconfig's legacy filter and epaint's classic ClearType filter have the same
        // role; use the normalized five-tap approximation rather than inventing an
        // unverified FreeType coefficient table.
        (_, LcdFilterChoice::Legacy) => egui::epaint::LcdFilter::CLASSIC,
        (_, LcdFilterChoice::None) => egui::epaint::LcdFilter::NONE,
    }
}

/// Expected sRGB background luminance: the polarity reference for grayscale text, and the
/// background GPU fixed-function LCD blending calibrates against to match CPU `TextGamma`.
pub const fn text_background_luma(theme: egui::Theme) -> f32 {
    match theme {
        egui::Theme::Dark => 0.13,
        egui::Theme::Light => 0.96,
    }
}

/// Expected luminance under text whose polarity opposes the theme, or `-1.0` for none.
///
/// The Light theme's light text is the toast UI, drawn on `from_black_alpha(220)` over the
/// page (`#212121`, luma ~0.13); without this the GPU LCD blend assumed pure black there and
/// drew that text up to ~16 LSB heavier than the CPU renderer, and polarity would split at
/// the 0.96 page, misreading light-gray toast text as dark-on-light. The Dark theme draws no
/// dark text, and an inverse surface there would move the polarity split above mid-luma
/// accent text (`#3a63d9`, luma ~0.39) that sits on the dark page.
pub const fn text_inverse_background_luma(theme: egui::Theme) -> f32 {
    match theme {
        egui::Theme::Dark => -1.0,
        egui::Theme::Light => text_background_luma(egui::Theme::Dark),
    }
}

/// Weight tuning for [`TextSmoothing::Sharp`] under sub-pixel rendering.
///
/// Dropping `contrast` to `0.0` avoids lifting 1px stem edges at 100% scaling. Measured
/// against native DirectWrite ClearType (Windows-default parameters,
/// `text_compare::the_weight_matches_native_directwrite_text`), Dark gamma `~1.40` and
/// Light gamma `~1.58` (on Windows' `1.8`) both keep ~0.9 of native ink at 100-150%: thin
/// and crisp in both themes. (Light used to be `~1.98`, which left only ~0.83.)
const SHARP_DARK_GAMMA_SCALE: f32 = 0.78;
const SHARP_LIGHT_GAMMA_SCALE: f32 = 0.88;
const SHARP_CONTRAST: f32 = 0.0;

/// Blend gamma and contrast for a smoothing profile under sub-pixel rendering.
///
/// Shared with `text_compare` so the comparison harness renders what the app renders.
pub fn cleartype_weight(
    smoothing: TextSmoothing,
    theme: egui::Theme,
    params: &tm_platform::text_rendering::Params,
) -> (f32, f32) {
    match (smoothing, theme) {
        // Thin and crisp: closest to native Windows on a dark UI, trimming 100% ink by
        // ~10.4% and 150% ink by ~8.0% relative to the prior 0.88/0.25 + FREETYPE_DEFAULT.
        (TextSmoothing::Sharp, egui::Theme::Dark) => (
            (params.gamma * SHARP_DARK_GAMMA_SCALE).clamp(1.0, 3.0),
            params.contrast.min(SHARP_CONTRAST),
        ),
        // Same crispness goal on a light UI without inverting gamma polarity.
        (TextSmoothing::Sharp, egui::Theme::Light) => (
            (params.gamma * SHARP_LIGHT_GAMMA_SCALE).clamp(1.0, 3.0),
            params.contrast.min(SHARP_CONTRAST),
        ),
        // The display's own DirectWrite / Fontconfig parameters, unmodified.
        (TextSmoothing::Standard | TextSmoothing::Smooth, _) => (params.gamma, params.contrast),
    }
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
    v.override_text_color = Some(LIGHT.text);
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

/// A subordinate tone of `color`: mixed `factor` of the way from the card
/// surface towards the full colour, and OPAQUE.
///
/// `Color32::gamma_multiply` looks like it does this but scales the alpha too,
/// so "a quieter shade" came out as "the same colour at 62 % opacity". That
/// darkens on the dark theme and washes out on the light one — and since
/// `widgets::chart` already paints series fills translucent, the two alphas
/// compounded until a subordinate series barely existed. Mixing towards the
/// card keeps the intended quietness in both themes without spending alpha.
pub fn toned(pal: &Palette, color: Color32, factor: f32) -> Color32 {
    lerp_rgb(pal.card_bg, color, factor.clamp(0.0, 1.0))
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

    #[test]
    fn subpixel_override_can_force_off_but_never_bypasses_platform_or_renderer_capability() {
        use egui::epaint::text::SubpixelMode;

        let rgb = tm_platform::text_rendering::Params {
            enabled: true,
            ..Default::default()
        };
        let bgr = tm_platform::text_rendering::Params { bgr: true, ..rgb };
        let disabled = tm_platform::text_rendering::Params {
            enabled: false,
            ..rgb
        };

        assert_eq!(
            resolve_subpixel_mode(true, None, Some(bgr)),
            SubpixelMode::Bgr
        );
        assert_eq!(
            resolve_subpixel_mode(true, Some(true), Some(rgb)),
            SubpixelMode::Rgb
        );
        assert_eq!(
            resolve_subpixel_mode(true, Some(false), Some(rgb)),
            SubpixelMode::Off
        );
        assert_eq!(
            resolve_subpixel_mode(false, Some(true), Some(rgb)),
            SubpixelMode::Off
        );
        assert_eq!(
            resolve_subpixel_mode(true, Some(true), Some(disabled)),
            SubpixelMode::Off
        );
    }

    #[test]
    fn fontconfig_lcdfilter_choices_map_to_filter_tables() {
        use tm_platform::text_rendering::LcdFilterChoice;
        // Platform-unspecified (`PlatformDefault`, e.g. Windows): `Sharp` uses `LIGHT`.
        assert_eq!(
            lcd_filter(TextSmoothing::Sharp, LcdFilterChoice::PlatformDefault),
            egui::epaint::LcdFilter::LIGHT
        );
        assert_eq!(
            lcd_filter(TextSmoothing::Standard, LcdFilterChoice::PlatformDefault),
            egui::epaint::LcdFilter::FREETYPE_DEFAULT
        );
        assert_eq!(
            lcd_filter(TextSmoothing::Smooth, LcdFilterChoice::PlatformDefault),
            egui::epaint::LcdFilter::FREETYPE_DEFAULT
        );
        // Explicit Linux Fontconfig `lcdfilter: default` is respected even under `Sharp`.
        assert_eq!(
            lcd_filter(TextSmoothing::Sharp, LcdFilterChoice::Default),
            egui::epaint::LcdFilter::FREETYPE_DEFAULT
        );
        assert_eq!(
            lcd_filter(TextSmoothing::Standard, LcdFilterChoice::Default),
            egui::epaint::LcdFilter::FREETYPE_DEFAULT
        );
        assert_eq!(
            lcd_filter(TextSmoothing::Smooth, LcdFilterChoice::Default),
            egui::epaint::LcdFilter::FREETYPE_DEFAULT
        );
        assert_eq!(
            lcd_filter(TextSmoothing::Standard, LcdFilterChoice::Light),
            egui::epaint::LcdFilter::LIGHT
        );
        assert_eq!(
            lcd_filter(TextSmoothing::Sharp, LcdFilterChoice::Legacy),
            egui::epaint::LcdFilter::CLASSIC
        );
        assert_eq!(
            lcd_filter(TextSmoothing::Sharp, LcdFilterChoice::None),
            egui::epaint::LcdFilter::NONE
        );
    }

    #[test]
    fn grayscale_transfer_follows_text_polarity_not_theme() {
        let half = |smoothing, rgb: [f32; 3], theme| {
            let exponent = grayscale_transfer(smoothing)
                .polarity_exponent(
                    rgb,
                    text_background_luma(theme),
                    text_inverse_background_luma(theme),
                )
                .expect("the grayscale curve is applied at render time");
            0.5_f32.powf(exponent)
        };
        let white = [1.0; 3];
        let ink = [0x1a as f32 / 255.0; 3];
        let accent_text_dark = [0.0, 0x1b as f32 / 255.0, 0x2e as f32 / 255.0];

        // Light-on-dark lifts partial coverage above raw linear (0.5) without `2c - c^2`
        // bloat (0.75), with Sharp leaner than Standard.
        let ds = half(TextSmoothing::Sharp, white, egui::Theme::Dark);
        let dt = half(TextSmoothing::Standard, white, egui::Theme::Dark);
        assert!(
            ds > 0.5 && ds < dt && dt < 0.70,
            "light text: sharp={ds} std={dt}"
        );

        // Dark-on-light stays near raw coverage (native DirectWrite is not thin there),
        // Sharp leaner than Standard.
        let ls = half(TextSmoothing::Sharp, ink, egui::Theme::Light);
        let lt = half(TextSmoothing::Standard, ink, egui::Theme::Light);
        assert!(
            ls < lt && ls > 0.4 && lt < 0.6,
            "dark text: sharp={ls} std={lt}"
        );

        // Opposite-polarity text keeps its own direction: the Light theme's white and
        // light-gray toast text is lifted like Dark-theme text, dark text thinned anywhere.
        let light_gray = [0xc8 as f32 / 255.0; 3];
        assert_eq!(half(TextSmoothing::Sharp, white, egui::Theme::Light), ds);
        assert_eq!(
            half(TextSmoothing::Sharp, light_gray, egui::Theme::Light),
            ds
        );
        assert_eq!(
            half(TextSmoothing::Sharp, accent_text_dark, egui::Theme::Dark),
            ls
        );
        // Mid-luma accent text on the dark page stays light-on-dark.
        let kernel_blue = [
            0x3a as f32 / 255.0,
            0x63 as f32 / 255.0,
            0xd9 as f32 / 255.0,
        ];
        assert_eq!(
            half(TextSmoothing::Sharp, kernel_blue, egui::Theme::Dark),
            ds
        );
    }

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

    /// Weighted channel distance, the crude stand-in for "tells apart at a
    /// glance" used by the pairing test below.
    fn distance(a: Color32, b: Color32) -> i32 {
        let d = |x: u8, y: u8| (x as i32 - y as i32).abs();
        (2 * d(a.r(), b.r()) + 5 * d(a.g(), b.g()) + d(a.b(), b.b())) / 8
    }

    /// Both halves of an overlaid pair must be OPAQUE and far enough apart to
    /// read as two series.
    ///
    /// Opacity is the part that regressed: the partners were
    /// `gamma_multiply(0.6)` of the primary, and that scales alpha along with
    /// the components — so "the darker one" was really "the same colour, more
    /// background showing through", which `widgets::chart` then painted behind
    /// its own translucent area fill. Disk read/write, network send/receive
    /// and CPU user/kernel all came out as one smear.
    #[test]
    fn paired_series_are_opaque_and_far_apart() {
        for pal in [DARK, LIGHT] {
            for (name, primary, partner) in [
                ("cpu", pal.cpu_graph, pal.cpu_kernel_graph),
                ("disk", pal.disk_graph, pal.disk_write_graph),
                ("network", pal.network_graph, pal.network_send_graph),
            ] {
                assert_eq!(primary.a(), 255, "{name} primary is translucent");
                assert_eq!(partner.a(), 255, "{name} partner is translucent");
                let d = distance(primary, partner);
                assert!(d >= 50, "{name}: {primary:?} vs {partner:?} is only {d}");
                // Each half must also stand clear of the card it is drawn on.
                for c in [primary, partner] {
                    let d = distance(c, pal.card_bg);
                    assert!(d >= 40, "{name}: {c:?} is only {d} off the card");
                }
            }
        }
    }

    /// A subordinate tone stays opaque and moves towards the CARD, not towards
    /// black — the light theme would otherwise make the quiet series the
    /// loudest thing on the page.
    #[test]
    fn a_subordinate_tone_is_opaque_and_sits_between_card_and_colour() {
        for pal in [DARK, LIGHT] {
            let quiet = toned(&pal, pal.memory_graph, 0.62);
            assert_eq!(quiet.a(), 255);
            assert!(distance(quiet, pal.card_bg) < distance(pal.memory_graph, pal.card_bg));
            assert!(distance(quiet, pal.card_bg) >= 30, "faded into the card");
            assert_eq!(toned(&pal, pal.memory_graph, 1.0), pal.memory_graph);
            assert_eq!(toned(&pal, pal.memory_graph, 0.0), pal.card_bg);
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

    #[test]
    fn installed_text_styles_and_light_visuals_match_contract() {
        let ctx = egui::Context::default();
        install_visuals(&ctx);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let style = ctx.style_of(theme);
            assert_eq!(
                style.text_styles[&egui::TextStyle::Monospace],
                FontId::monospace(12.0),
                "TextStyle::Monospace must use the Monospace family"
            );
            assert_eq!(
                style.text_styles[&egui::TextStyle::Heading],
                font_semibold(15.5)
            );
            assert_eq!(style.text_styles[&egui::TextStyle::Small], font_small(11.0));
        }
        assert_eq!(light_visuals().override_text_color, Some(LIGHT.text));
        assert_eq!(dark_visuals().override_text_color, Some(DARK.text));
    }

    #[test]
    fn sharp_cleartype_weight_respects_polarity_in_dark_and_light_modes() {
        let win = tm_platform::text_rendering::Params {
            enabled: true,
            gamma: 1.8,
            contrast: 0.5,
            ..Default::default()
        };
        let (dark_g, dark_c) = cleartype_weight(TextSmoothing::Sharp, egui::Theme::Dark, &win);
        let (light_g, light_c) = cleartype_weight(TextSmoothing::Sharp, egui::Theme::Light, &win);
        assert!(
            dark_g < win.gamma,
            "dark Sharp should lower gamma to thin white ink"
        );
        // Light used to RAISE gamma to thin dark ink; measured against native DirectWrite
        // that left Light ~0.83 of native ink. Both themes now sit below Windows' gamma and
        // the weight itself is pinned by `text_compare`'s DirectWrite comparison.
        assert!(
            (1.0..win.gamma).contains(&light_g) && light_g > dark_g,
            "light Sharp gamma {light_g} must stay below Windows' and above dark's {dark_g}"
        );
        assert!(dark_c < win.contrast && light_c < win.contrast);

        // Zero-contrast (Linux) must not be boosted by Sharp.
        let linux = tm_platform::text_rendering::Params {
            enabled: true,
            gamma: 1.8,
            contrast: 0.0,
            ..Default::default()
        };
        assert_eq!(
            cleartype_weight(TextSmoothing::Sharp, egui::Theme::Dark, &linux).1,
            0.0
        );
        assert_eq!(
            cleartype_weight(TextSmoothing::Sharp, egui::Theme::Light, &linux).1,
            0.0
        );
        assert_eq!(text_background_luma(egui::Theme::Dark), 0.13);
        assert_eq!(text_background_luma(egui::Theme::Light), 0.96);
        assert_eq!(text_inverse_background_luma(egui::Theme::Light), 0.13);
        assert_eq!(text_inverse_background_luma(egui::Theme::Dark), -1.0);
    }
}
