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
//! Measure at both **100%** and **150% scaling**: at 100% the LCD filter's outer taps
//! and contrast dominate 1 px stem weight, while at 150% stems are ~2 px and gamma drives
//! perceived thickness.

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

/// Largest font face the comparison will load.
///
/// A UI face is about two MiB (Segoe UI Variable is 1.9 MiB, Segoe UI 0.9 MiB on
/// the reference Windows 11 build), so this leaves headroom for a larger variable
/// font while still bounding the read. The shape is what matters: the path is
/// fixed, so a file replaced or redirected there — a multi-gigabyte one, a device,
/// a FIFO — has to fail honestly instead of being pulled into memory and rendered
/// as a comparison that "found no differences". Same bounded read as the app's own
/// readers (`settings::read_capped`): at most `limit + 1` bytes, and an overrun is
/// an error rather than a truncated face.
const MAX_FONT_BYTES: u64 = 4 * 1024 * 1024;

/// The UI face for the comparison, or why there is none.
enum UiFont {
    /// The face was read and the comparison can run.
    Loaded(Vec<u8>),
    /// No candidate UI face on this machine (a non-Windows host, or an install
    /// without the Segoe UI files). Callers skip: there is nothing to compare
    /// against, which is not the same answer as "the profiles match".
    Absent,
    /// A candidate exists but cannot be used, with the reason. Callers report it
    /// as a failure — a comparison that rendered nothing would otherwise read
    /// exactly like one that found no differences.
    Unusable(String),
}

/// Read at most `limit` bytes of `path`.
///
/// Asks for one byte more than the limit so an overrun is visible without reading
/// the whole file, the way `settings::read_capped` and the Linux text readers do.
fn read_capped(path: &str, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(16 * 1024);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{path} is larger than the {limit}-byte font limit"),
        ));
    }
    Ok(bytes)
}

/// The real UI font, so the comparison reflects what the app actually draws.
fn ui_font() -> UiFont {
    for path in [
        r"C:\Windows\Fonts\SegUIVar.ttf",
        r"C:\Windows\Fonts\segoeui.ttf",
    ] {
        match read_capped(path, MAX_FONT_BYTES) {
            Ok(bytes) => return UiFont::Loaded(bytes),
            // Not installed here: try the next candidate.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return UiFont::Unusable(format!("{path}: {error}")),
        }
    }
    UiFont::Absent
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
    filter: Option<LcdFilter>,
    theme: egui::Theme,
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
            filter: None,
            theme: egui::Theme::Dark,
            ppp: 1.0,
            blend: None,
        }
    }
    fn theme(mut self, theme: egui::Theme) -> Self {
        self.theme = theme;
        self
    }
    fn ppp(mut self, ppp: f32) -> Self {
        self.ppp = ppp;
        self
    }
    fn filter(mut self, f: LcdFilter) -> Self {
        self.filter = Some(f);
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
        theme,
        ppp,
        blend,
    } = cfg;
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(ppp);
    ctx.set_fonts(definitions(bytes, smoothing));
    for t in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(t, |style| {
            let o = &mut style.visuals.text_options;
            o.color_transfer_function = crate::theme::grayscale_transfer(smoothing, t);
            o.subpixel = subpixel;
            o.lcd_filter = filter.unwrap_or_else(|| {
                crate::theme::lcd_filter(smoothing, crate::theme::subpixel_params().lcd_filter)
            });
            // Only `Smooth` asks for quarter-pixel horizontal positioning.
            o.subpixel_binning = smoothing == TextSmoothing::Smooth;
            o.font_hinting = true;
            if !subpixel.is_off() {
                // The atlas ceiling is not settable from a style:
                // `Context::update_fonts_mut` overwrites `max_texture_side` with
                // `RawInput`'s value on every pass, so a write here would be
                // discarded. The harness only renders a short sample anyway.
                // Either an explicit sweep value, or exactly what the app would pick
                // for this profile -- so the profile rows show real behaviour.
                let (gamma, contrast) = blend.unwrap_or_else(|| {
                    crate::theme::cleartype_weight(smoothing, t, &crate::theme::subpixel_params())
                });
                o.text_gamma = gamma;
                o.text_contrast = contrast;
                o.text_background_luma = crate::theme::text_background_luma(t);
            }
        });
    }
    ctx.set_theme(match theme {
        egui::Theme::Dark => egui::ThemePreference::Dark,
        egui::Theme::Light => egui::ThemePreference::Light,
    });
    let (bg_color, fg_color) = match theme {
        egui::Theme::Dark => (
            Color32::from_rgb(0x19, 0x19, 0x19),
            Color32::from_rgb(0xe6, 0xe6, 0xe6),
        ),
        egui::Theme::Light => (
            Color32::from_rgb(0xf3, 0xf3, 0xf3),
            Color32::from_rgb(0x1a, 0x1a, 0x1a),
        ),
    };

    let build = |ui: &mut egui::Ui| {
        ui.painter().text(
            pos2(6.0, 12.0),
            egui::Align2::LEFT_TOP,
            SAMPLE,
            FontId::proportional(13.0),
            fg_color,
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
    Painter::clear(&mut target, bg_color);
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

    /// The face, or `None` when this host simply has no UI font.
    ///
    /// An unusable face is a FAILURE, never a skip: a comparison that rendered
    /// nothing reads exactly like one that found no differences between the
    /// profiles, which is the answer these tests exist to give honestly.
    fn font_or_skip() -> Option<Vec<u8>> {
        match ui_font() {
            UiFont::Loaded(bytes) => Some(bytes),
            UiFont::Absent => None,
            UiFont::Unusable(reason) => panic!("the UI font cannot be used: {reason}"),
        }
    }

    /// A clean directory this test owns, matching the repo's temp-file convention
    /// (PID plus a per-test sequence number, removed afterwards).
    fn temp_dir(name: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tm-text-compare-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp directory this test owns");
        dir
    }

    /// A normal face comes back byte-identical, a bigger one is refused AFTER the
    /// cap and not truncated, and a missing file stays "not installed".
    ///
    /// Regression: the face was read with `std::fs::read`, so whatever sat at the
    /// fixed font path was pulled into memory whole — a replaced multi-gigabyte
    /// file (or a device/FIFO) instead of a comparison. An overrun used to be
    /// indistinguishable from "no font installed", i.e. from a comparison that
    /// rendered nothing.
    #[test]
    fn the_font_read_is_capped_and_reports_an_unusable_face() {
        let dir = temp_dir("capped");
        let path = dir.join("face.ttf");
        std::fs::write(&path, b"a real face").expect("write the fixture");
        let path = path.to_string_lossy().into_owned();

        // A normal file is unchanged, byte for byte.
        let loaded = read_capped(&path, 1024).expect("a small face is readable");
        assert_eq!(loaded.as_slice(), b"a real face".as_slice());

        // Exactly at the limit still loads; one byte over is an error, not a
        // truncated face that the parser would then reject for the wrong reason.
        assert_eq!(
            read_capped(&path, loaded.len() as u64).expect("at the limit"),
            loaded
        );
        let error = read_capped(&path, loaded.len() as u64 - 1).expect_err("over the limit");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{error}");
        assert!(
            error.to_string().contains(path.as_str()),
            "the diagnostic must name the file: {error}"
        );

        // Not installed is its own answer, and the caller treats it as "skip".
        let missing = dir.join("absent.ttf").to_string_lossy().into_owned();
        assert_eq!(
            read_capped(&missing, 1024)
                .expect_err("no such file")
                .kind(),
            std::io::ErrorKind::NotFound
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write one magnified PNG per profile. Off unless `TASKMAN_TEXT_COMPARE` names a
    /// path prefix, so a normal `cargo test` writes nothing.
    #[test]
    fn write_text_smoothing_comparison() {
        let Ok(prefix) = std::env::var("TASKMAN_TEXT_COMPARE") else {
            return;
        };
        let Some(bytes) = font_or_skip() else {
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
        // colour directly. Every shipped preset, for comparison.
        for (name, filter) in [
            ("filter-freetype", LcdFilter::FREETYPE_DEFAULT),
            ("filter-light", LcdFilter::LIGHT),
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

    /// Under sub-pixel rendering, `Sharp` and `Standard` must produce distinct output
    /// (grid-fitting, LCD filter, and blend weight).
    #[test]
    fn the_profiles_differ_under_subpixel() {
        let Some(bytes) = font_or_skip() else { return };
        let sharp = render(&bytes, TextSmoothing::Sharp, SubpixelMode::Rgb);
        let standard = render(&bytes, TextSmoothing::Standard, SubpixelMode::Rgb);
        assert_ne!(
            sharp, standard,
            "Sharp and Standard rendered identically -- profile settings are not reaching the renderer"
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
    /// Measured at both 100% (1080p) and 150% (4K) scaling.
    #[test]
    fn sharp_renders_thinner_than_standard_under_subpixel() {
        let Some(bytes) = font_or_skip() else { return };
        for ppp in [1.0_f32, 1.5] {
            let at = |sm| {
                ink(&render_cfg(
                    &bytes,
                    Cfg::new(sm, SubpixelMode::Rgb).ppp(ppp),
                ))
            };
            let sharp = at(TextSmoothing::Sharp);
            let standard = at(TextSmoothing::Standard);
            assert!(
                sharp < standard * 0.92,
                "at ppp={ppp}, Sharp ({sharp:.0}) is not meaningfully thinner than Standard ({standard:.0}); the weight/filter control is not reaching the renderer"
            );
            // ...but not so thin that stems start dropping out.
            assert!(
                sharp > standard * 0.6,
                "at ppp={ppp}, Sharp ({sharp:.0}) is far lighter than Standard ({standard:.0}) -- text this faint reads as broken rather than crisp"
            );
        }
    }

    /// Total dark ink below the light background: how "fat" dark-on-light text is.
    fn dark_ink(buf: &[u32]) -> f64 {
        let bg: u32 = (299 * 0xf3 + 587 * 0xf3 + 114 * 0xf3) / 1000;
        buf.iter()
            .map(|&px| {
                let (r, g, b) = ((px >> 16) & 0xff, (px >> 8) & 0xff, px & 0xff);
                let l = (299 * r + 587 * g + 114 * b) / 1000;
                f64::from(bg.saturating_sub(l)) / 255.0
            })
            .sum()
    }

    /// Under sub-pixel rendering on a light UI, `Sharp` must also render *thinner*
    /// than `Standard` — lowering gamma blindly would thicken dark-on-light text.
    #[test]
    fn sharp_renders_thinner_than_standard_under_subpixel_in_light_mode() {
        let Some(bytes) = font_or_skip() else { return };
        for ppp in [1.0_f32, 1.5] {
            let at = |sm| {
                dark_ink(&render_cfg(
                    &bytes,
                    Cfg::new(sm, SubpixelMode::Rgb)
                        .theme(egui::Theme::Light)
                        .ppp(ppp),
                ))
            };
            let sharp = at(TextSmoothing::Sharp);
            let standard = at(TextSmoothing::Standard);
            assert!(
                sharp < standard * 0.94,
                "Light Mode at ppp={ppp}, Sharp ({sharp:.0}) is not thinner than Standard ({standard:.0})"
            );
            assert!(
                sharp > standard * 0.6,
                "Light Mode at ppp={ppp}, Sharp ({sharp:.0}) is far lighter than Standard ({standard:.0})"
            );
        }
    }

    /// Under grayscale fallback (`SubpixelMode::Off` — Linux `rgba=none`, RDP, portrait
    /// monitors, or unsupported GPUs), `Sharp` must stay thinner than `Standard` in both
    /// Dark and Light modes without dropping stems.
    #[test]
    fn sharp_renders_thinner_than_standard_under_grayscale() {
        let Some(bytes) = font_or_skip() else { return };
        for ppp in [1.0_f32, 1.5] {
            let dark_sharp = ink(&render_cfg(
                &bytes,
                Cfg::new(TextSmoothing::Sharp, SubpixelMode::Off).ppp(ppp),
            ));
            let dark_std = ink(&render_cfg(
                &bytes,
                Cfg::new(TextSmoothing::Standard, SubpixelMode::Off).ppp(ppp),
            ));
            assert!(
                dark_sharp < dark_std * 0.98 && dark_sharp > dark_std * 0.65,
                "Grayscale Dark ppp={ppp}: sharp={dark_sharp:.0}, standard={dark_std:.0}"
            );

            let light_sharp = dark_ink(&render_cfg(
                &bytes,
                Cfg::new(TextSmoothing::Sharp, SubpixelMode::Off)
                    .theme(egui::Theme::Light)
                    .ppp(ppp),
            ));
            let light_std = dark_ink(&render_cfg(
                &bytes,
                Cfg::new(TextSmoothing::Standard, SubpixelMode::Off)
                    .theme(egui::Theme::Light)
                    .ppp(ppp),
            ));
            assert!(
                light_sharp < light_std * 0.98 && light_sharp > light_std * 0.65,
                "Grayscale Light ppp={ppp}: sharp={light_sharp:.0}, standard={light_std:.0}"
            );
        }
    }
}
