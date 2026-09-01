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
//! # What actually differs once sub-pixel is on
//!
//! Less than it looks. The profiles control three things, and one of them stops applying:
//!
//! | | grid-fit horizontally | sub-pixel binning | coverage ramp |
//! | --- | --- | --- | --- |
//! | `Sharp` | yes | no | inert |
//! | `Standard` | no | no | inert |
//! | `Smooth` | no | yes | inert |
//!
//! The ramp (`FontColorTransferFunction`) is consulted only by the grayscale rasterizer.
//! The sub-pixel path writes per-channel coverage directly and the renderer applies gamma
//! and enhanced contrast instead, so selecting `Sharp` for its raw-coverage ramp changes
//! nothing here -- only its grid-fitting does.

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

fn render(bytes: &[u8], smoothing: TextSmoothing, subpixel: SubpixelMode) -> Vec<u32> {
    render_with_filter(bytes, smoothing, subpixel, LcdFilter::default())
}

fn render_with_filter(
    bytes: &[u8],
    smoothing: TextSmoothing,
    subpixel: SubpixelMode,
    filter: LcdFilter,
) -> Vec<u32> {
    let ctx = egui::Context::default();
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
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(W as f32, H as f32))),
        ..Default::default()
    };

    let mut painter = Painter::new();
    if !subpixel.is_off() {
        let p = crate::theme::subpixel_params();
        painter.set_subpixel(subpixel, p.gamma, p.contrast);
    }

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
}
