//! Side-by-side comparison of the text-smoothing profiles.
//!
//! Renders the same string through each [`TextSmoothing`] profile with sub-pixel
//! (`ClearType`) coverage active, using the real UI font, and writes magnified PNGs. The
//! question "which profile is right for `ClearType`" is a judgement about how text looks,
//! so it deserves pictures rather than an argument.
//!
//! ```text
//! TASKMAN_TEXT_COMPARE=target/text cargo test -p tm-app text_compare
//! ```
//!
//! # What the profile means once sub-pixel is on
//!
//! Not what it used to. The coverage ramp (`FontColorTransferFunction`) is consulted only
//! by the grayscale rasterizer; the sub-pixel path writes per-channel coverage and the
//! renderer applies gamma and enhanced contrast instead. So the profile was left almost
//! inert -- measured at 100% scaling the three were indistinguishable.
//!
//! It now selects the **weight** of sub-pixel text, which is the useful thing left for it
//! to control:
//!
//! | | grid-fit horizontally | binning | blend |
//! | --- | --- | --- | --- |
//! | `Sharp` | yes | no | thinner (gamma pulled back, no contrast boost) |
//! | `Standard` | no | no | the display's own DirectWrite parameters |
//! | `Smooth` | no | yes | the display's parameters, softest |
//!
//! Measure at **150% scaling**, not 100%: at 100% the profiles look alike, and the
//! weight question only becomes visible where stems are ~2 px.

use eframe::egui::epaint::text::{
    FontData, FontDefinitions, FontFamily, FontTweak, HintingTarget, LcdFilter, SmoothHinting,
    SubpixelMode, VariationCoords,
};
use eframe::egui::{self, Color32, FontId, Pos2, Rect, pos2, vec2};
use egui_software::{Painter, ShapeContext, Target};

use tm_core::settings::TextSmoothing;

const W: u32 = 460;
const H: u32 = 44;
const ZOOM: u32 = 5;
const SAMPLE: &str = "Brave Browser (43)  18.4%  2410 MB";

/// The grid-fitting profile, mirroring `fonts::hinting_target`.
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

/// The real UI font, so the comparison reflects what the app actually draws.
fn ui_font() -> Option<Vec<u8>> {
    for path in [
        r"C:\Windows\Fonts\SegUIVar.ttf",
        r"C:\Windows\Fonts\segoeui.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            return Some(bytes);
        }
    }
    None
}

fn definitions(bytes: &[u8], smoothing: TextSmoothing) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let data = FontData::from_owned(bytes.to_vec()).tweak(FontTweak {
        hinting_target: hinting_target(smoothing),
        coords: VariationCoords::new([(b"wght", 400.0), (b"opsz", 10.5)]),
        ..Default::default()
    });
    fonts
        .font_data
        .insert("UI".into(), std::sync::Arc::new(data));
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "UI".into());
    fonts
}

/// One rendering configuration.
#[derive(Clone, Copy)]
struct Cfg {
    smoothing: TextSmoothing,
    subpixel: SubpixelMode,
    filter: LcdFilter,
    /// Physical pixels per point. 1.5 is Windows' 150% scaling.
    ppp: f32,
    /// `None` uses the display's own DirectWrite parameters.
    blend: Option<(f32, f32)>,
}

impl Cfg {
    fn new(smoothing: TextSmoothing, subpixel: SubpixelMode) -> Self {
        Self {
            smoothing,
            subpixel,
            filter: LcdFilter::default(),
            ppp: 1.0,
            blend: None,
        }
    }
    fn ppp(mut self, ppp: f32) -> Self {
        self.ppp = ppp;
        self
    }
    fn filter(mut self, f: LcdFilter) -> Self {
        self.filter = f;
        self
    }
    fn blend(mut self, gamma: f32, contrast: f32) -> Self {
        self.blend = Some((gamma, contrast));
        self
    }
}

fn render(bytes: &[u8], smoothing: TextSmoothing, subpixel: SubpixelMode) -> Vec<u32> {
    render_cfg(bytes, Cfg::new(smoothing, subpixel))
}

fn render_with_filter(
    bytes: &[u8],
    smoothing: TextSmoothing,
    subpixel: SubpixelMode,
    filter: LcdFilter,
) -> Vec<u32> {
    render_cfg(bytes, Cfg::new(smoothing, subpixel).filter(filter))
}

fn render_cfg(bytes: &[u8], cfg: Cfg) -> Vec<u32> {
    let Cfg {
        smoothing,
        subpixel,
        filter,
        ppp,
        blend,
    } = cfg;
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(ppp);
    ctx.set_fonts(definitions(bytes, smoothing));
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            let o = &mut style.visuals.text_options;
            o.subpixel = subpixel;
            o.lcd_filter = filter;
            // Only `Smooth` asks for quarter-pixel horizontal positioning.
            o.subpixel_binning = smoothing == TextSmoothing::Smooth;
            o.font_hinting = true;
            if !subpixel.is_off() {
                o.max_texture_side = o.max_texture_side.max(4096);
                // Either an explicit sweep value, or exactly what the app would pick
                // for this profile -- so the profile rows show real behaviour.
                let (gamma, contrast) = blend.unwrap_or_else(|| {
                    crate::theme::cleartype_weight(smoothing, &crate::theme::subpixel_params())
                });
                o.text_gamma = gamma;
                o.text_contrast = contrast;
            }
        });
    }
    ctx.set_theme(egui::ThemePreference::Dark);

    let build = |ui: &mut egui::Ui| {
        ui.painter().text(
            pos2(6.0, 12.0),
            egui::Align2::LEFT_TOP,
            SAMPLE,
            FontId::proportional(13.0),
            Color32::from_rgb(0xe6, 0xe6, 0xe6),
        );
    };
    let input = || egui::RawInput {
        screen_rect: Some(Rect::from_min_size(
            Pos2::ZERO,
            vec2(W as f32 / ppp, H as f32 / ppp),
        )),
        ..Default::default()
    };

    let mut painter = Painter::new();

    let apply = |painter: &mut Painter, out: &mut egui::FullOutput| {
        for (id, deltas) in &out.textures_delta.set {
            for d in deltas {
                painter.set_texture(*id, d);
            }
        }
        out.textures_delta.clear();
    };

    let mut build = build;
    let mut warm = ctx.run_ui(input(), &mut build);
    apply(&mut painter, &mut warm);
    let mut out = ctx.run_ui(input(), &mut build);
    apply(&mut painter, &mut out);

    if !subpixel.is_off() {
        // Mirror what `software_integration` does: take the blend from the atlas options
        // (only readable once a pass has run), so rasterization and blending cannot
        // disagree.
        let (gamma, contrast) = ctx.fonts(|f| (f.options().text_gamma, f.options().text_contrast));
        painter.set_subpixel(subpixel, gamma, contrast);
    }

    let shape_ctx = ShapeContext {
        pixels_per_point: out.pixels_per_point,
        options: ctx.tessellation_options(|o| *o),
        font_tex_size: ctx.fonts(|f| f.font_image_size()),
        prepared_discs: ctx.fonts(|f| f.fonts.texture_atlas().prepared_discs()),
    };

    let mut buf = vec![0u32; (W * H) as usize];
    let mut target = Target::new(&mut buf, W, H).expect("target");
    Painter::clear(&mut target, Color32::from_rgb(0x19, 0x19, 0x19));
    painter.paint_shapes(&mut target, &shape_ctx, out.shapes);
    buf
}

/// Nearest-neighbour magnification, so individual sub-pixel fringes stay visible.
fn write_zoomed(path: &std::path::Path, buf: &[u32]) {
    let (ow, oh) = (W * ZOOM, H * ZOOM);
    let mut rgba = Vec::with_capacity((ow * oh * 4) as usize);
    for y in 0..oh {
        for x in 0..ow {
            let px = buf[((y / ZOOM) * W + (x / ZOOM)) as usize];
            rgba.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, px as u8, 0xff]);
        }
    }
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).expect("create dir");
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), ow, oh);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("header")
        .write_image_data(&rgba)
        .expect("data");
    eprintln!("wrote {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write one magnified PNG per profile. Off unless `TASKMAN_TEXT_COMPARE` names a
    /// path prefix, so a normal `cargo test` writes nothing.
    #[test]
    fn write_text_smoothing_comparison() {
        let Ok(prefix) = std::env::var("TASKMAN_TEXT_COMPARE") else {
            return;
        };
        let Some(bytes) = ui_font() else {
            eprintln!("no system UI font found; skipping");
            return;
        };
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

        for (name, smoothing) in [
            ("sharp", TextSmoothing::Sharp),
            ("standard", TextSmoothing::Standard),
            ("smooth", TextSmoothing::Smooth),
        ] {
            let buf = render(&bytes, smoothing, SubpixelMode::Rgb);
            write_zoomed(&base.join(format!("{prefix}-{name}-lcd.png")), &buf);
        }
        // The LCD filter matters more than the profile does: it trades sharpness against
        // colour directly. Both shipped presets, for comparison.
        for (name, filter) in [
            ("filter-freetype", LcdFilter::FREETYPE_DEFAULT),
            ("filter-classic", LcdFilter::CLASSIC),
            ("filter-none", LcdFilter::NONE),
        ] {
            let buf =
                render_with_filter(&bytes, TextSmoothing::Standard, SubpixelMode::Rgb, filter);
            write_zoomed(&base.join(format!("{prefix}-{name}.png")), &buf);
        }

        // At 150% scaling, which is where the weight question actually bites: a 13 pt
        // string is ~20 px tall and stems are ~2 px, a different regime from 100%.
        for (name, gamma, contrast) in [
            ("g18-c05", 1.8, 0.5),
            ("g18-c00", 1.8, 0.0),
            ("g14-c00", 1.4, 0.0),
            ("g10-c00", 1.0, 0.0),
        ] {
            let cfg = Cfg::new(TextSmoothing::Standard, SubpixelMode::Rgb)
                .ppp(1.5)
                .blend(gamma, contrast);
            write_zoomed(
                &base.join(format!("{prefix}-150-{name}.png")),
                &render_cfg(&bytes, cfg),
            );
        }
        // ...and Sharp vs Standard at 150%, since the profile question was only ever
        // measured at 100%.
        for (name, sm) in [
            ("sharp", TextSmoothing::Sharp),
            ("standard", TextSmoothing::Standard),
        ] {
            let cfg = Cfg::new(sm, SubpixelMode::Rgb).ppp(1.5);
            write_zoomed(
                &base.join(format!("{prefix}-150-{name}.png")),
                &render_cfg(&bytes, cfg),
            );
        }

        // The proposal: Sharp's grid-fitting plus a thinner blend, which is what native
        // Windows text looks like on a dark UI at this scale.
        write_zoomed(
            &base.join(format!("{prefix}-150-proposed.png")),
            &render_cfg(
                &bytes,
                Cfg::new(TextSmoothing::Sharp, SubpixelMode::Rgb)
                    .ppp(1.5)
                    .blend(1.3, 0.0),
            ),
        );

        // Grayscale `Sharp` for reference: what the app drew before this work.
        let buf = render(&bytes, TextSmoothing::Sharp, SubpixelMode::Off);
        write_zoomed(&base.join(format!("{prefix}-sharp-gray.png")), &buf);
    }

    /// The coverage ramp is not consulted on the sub-pixel path, so `Sharp` and
    /// `Standard` can only differ through grid-fitting. If a future change starts
    /// applying the ramp there too, the gamma model would be applied twice and text would
    /// come out visibly wrong -- this pins the current behaviour down.
    #[test]
    fn the_profiles_differ_only_by_hinting_and_binning_under_subpixel() {
        let Some(bytes) = ui_font() else { return };
        let sharp = render(&bytes, TextSmoothing::Sharp, SubpixelMode::Rgb);
        let standard = render(&bytes, TextSmoothing::Standard, SubpixelMode::Rgb);
        assert_ne!(
            sharp, standard,
            "Sharp and Standard rendered identically -- the hinting profile is not \
             reaching the rasterizer"
        );
    }

    /// Total light above the background: how "fat" the text is.
    fn ink(buf: &[u32]) -> f64 {
        let bg: u32 = (299 * 0x19 + 587 * 0x19 + 114 * 0x19) / 1000;
        buf.iter()
            .map(|&px| {
                let (r, g, b) = ((px >> 16) & 0xff, (px >> 8) & 0xff, px & 0xff);
                let l = (299 * r + 587 * g + 114 * b) / 1000;
                f64::from(l.saturating_sub(bg)) / 255.0
            })
            .sum()
    }

    /// Under sub-pixel rendering, `Sharp` must render *thinner* than `Standard`.
    ///
    /// This is the whole point of the profile now: DirectWrite's gamma and contrast
    /// describe DirectWrite's curve, and fed unchanged into this one they lift a
    /// half-covered pixel from 128 to 178 on a dark UI -- fat and glowing next to native
    /// Windows text, which errs thin. If a future change makes the profiles equivalent
    /// again, the setting silently stops doing anything and this catches it.
    ///
    /// Measured at 150% scaling, where taskman is actually used on a 4K display.
    #[test]
    fn sharp_renders_thinner_than_standard_under_subpixel() {
        let Some(bytes) = ui_font() else { return };
        let at = |sm| {
            ink(&render_cfg(
                &bytes,
                Cfg::new(sm, SubpixelMode::Rgb).ppp(1.5),
            ))
        };
        let sharp = at(TextSmoothing::Sharp);
        let standard = at(TextSmoothing::Standard);
        assert!(
            sharp < standard * 0.95,
            "Sharp ({sharp:.0}) is not meaningfully thinner than Standard ({standard:.0});              the weight control is not reaching the renderer"
        );
        // ...but not so thin that stems start dropping out.
        assert!(
            sharp > standard * 0.6,
            "Sharp ({sharp:.0}) is far lighter than Standard ({standard:.0}) -- text this              faint reads as broken rather than crisp"
        );
    }
}
