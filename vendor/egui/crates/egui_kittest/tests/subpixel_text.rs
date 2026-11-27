#![cfg(all(feature = "wgpu", not(target_arch = "wasm32")))]

//! Manual hardware coverage for the managed-font dual-source LCD pipeline.
//!
//! These tests are ignored by default. Set `TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1` and provide
//! a D3D12 or Vulkan adapter with `DUAL_SOURCE_BLENDING`; a missing prerequisite is a
//! failure, never a reported pass.

use std::sync::Arc;

use egui::{
    Color32, FontId, Pos2, Theme, Vec2, emath::TSTransform, epaint::TextShape,
    epaint::text::SubpixelMode,
};
use egui_kittest::{Harness, wgpu::WgpuTestRenderer};
use egui_wgpu::{RendererOptions, WgpuSetup, wgpu};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Draw {
    Plain,
    Rotated,
    Scaled,
    FractionalTranslation,
}

#[derive(Clone, Copy)]
struct RenderConfig {
    mode: SubpixelMode,
    draw: Draw,
    contrast: f32,
    text_color: Color32,
    options: RendererOptions,
    srgb_target: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            mode: SubpixelMode::Rgb,
            draw: Draw::Plain,
            contrast: 0.5,
            text_color: Color32::WHITE,
            options: RendererOptions::PREDICTABLE,
            srgb_target: false,
        }
    }
}

fn target_backend(backend: wgpu::Backend) -> bool {
    matches!(backend, wgpu::Backend::Dx12 | wgpu::Backend::Vulkan)
}

fn dual_source_available() -> bool {
    let setup = egui_kittest::wgpu::default_wgpu_setup();
    let instance = pollster::block_on(setup.new_instance());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    adapters.iter().any(|adapter| {
        target_backend(adapter.get_info().backend)
            && adapter.get_info().device_type != wgpu::DeviceType::Cpu
            && adapter
                .features()
                .contains(wgpu::Features::DUAL_SOURCE_BLENDING)
    })
}

fn require_hardware(test: &str) {
    let enabled = std::env::var("TASKMAN_RUN_GPU_SUBPIXEL_TESTS")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes"));
    assert!(
        enabled,
        "{test} is manual: set TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1 to run it"
    );
    assert!(
        dual_source_available(),
        "{test} requires a D3D12 or Vulkan adapter with DUAL_SOURCE_BLENDING"
    );
}

fn setup_for_dual_source() -> WgpuSetup {
    let mut setup = egui_kittest::wgpu::default_wgpu_setup();
    let WgpuSetup::CreateNew(create_new) = &mut setup else {
        unreachable!("default_wgpu_setup always creates a new setup");
    };
    create_new.native_adapter_selector = Some(Arc::new(|adapters, _surface| {
        adapters
            .iter()
            .find(|adapter| {
                target_backend(adapter.get_info().backend)
                    && adapter.get_info().device_type != wgpu::DeviceType::Cpu
                    && adapter
                        .features()
                        .contains(wgpu::Features::DUAL_SOURCE_BLENDING)
            })
            .cloned()
            .ok_or_else(|| "no dual-source-capable wgpu adapter".to_owned())
    }));
    setup
}

fn render(config: RenderConfig) -> image::RgbaImage {
    let renderer = if config.srgb_target {
        WgpuTestRenderer::from_setup_and_format(
            setup_for_dual_source(),
            config.options,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        )
    } else {
        WgpuTestRenderer::from_setup_and_options(setup_for_dual_source(), config.options)
    };
    let mut harness = Harness::builder()
        .with_size(Vec2::new(240.0, 60.0))
        .with_pixels_per_point(1.0)
        .renderer(renderer)
        .build_ui(move |ui| {
            ui.ctx().set_theme(Theme::Dark);
            ui.ctx().style_mut_of(Theme::Dark, |style| {
                style.visuals.text_options.subpixel = config.mode;
                style.visuals.text_options.text_contrast = config.contrast;
            });
            ui.painter()
                .rect_filled(ui.ctx().content_rect(), 0.0, Color32::BLACK);
            let galley = ui.painter().layout_no_wrap(
                "Hamburgefonstiv".to_owned(),
                FontId::proportional(14.0),
                config.text_color,
            );
            let mut shape = TextShape::new(Pos2::new(10.0, 20.0), galley, config.text_color);
            if config.draw == Draw::Rotated {
                shape = shape.with_angle(0.2);
            }
            match config.draw {
                Draw::Scaled => {
                    ui.ctx()
                        .set_transform_layer(ui.layer_id(), TSTransform::from_scaling(1.37));
                }
                Draw::FractionalTranslation => {
                    ui.ctx().set_transform_layer(
                        ui.layer_id(),
                        TSTransform::from_translation(Vec2::new(0.25, 0.0)),
                    );
                }
                Draw::Plain | Draw::Rotated => {}
            }
            ui.painter().add(shape);
        });

    // The harness's construction frame establishes default text options. The next frame
    // applies the style above, resets the atlas, and renders the actual sample.
    harness.run();
    harness.render().expect("headless wgpu render")
}

fn chromatic_pixels(image: &image::RgbaImage) -> usize {
    image
        .pixels()
        .filter(|pixel| {
            let [r, g, b, _] = pixel.0;
            r.max(g).max(b) - r.min(g).min(b) > 24
        })
        .count()
}

fn ink(image: &image::RgbaImage) -> u64 {
    image.pixels().map(|pixel| u64::from(pixel.0[1])).sum()
}

fn tinted_contrast_changes(first: &image::RgbaImage, second: &image::RgbaImage) -> usize {
    first
        .pixels()
        .zip(second.pixels())
        .filter(|(a, b)| {
            let [ar, ag, ab, aa] = a.0;
            let [br, bg, bb, ba] = b.0;
            aa > 0
                && ba > 0
                && ar.max(ag).max(ab) - ar.min(ag).min(ab) > 24
                && [ar, ag, ab] != [br, bg, bb]
        })
        .count()
}

#[test]
#[ignore = "manual hardware GPU test; set TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1"]
fn dual_source_text_is_colored_on_pixels_and_grayscale_off_grid() {
    require_hardware("dual_source_text_is_colored_on_pixels_and_grayscale_off_grid");

    let lcd = render(RenderConfig::default());
    let plain_chromatic = chromatic_pixels(&lcd);
    assert!(
        plain_chromatic > 50,
        "plain LCD text did not produce color fringes"
    );
    assert!(ink(&lcd) > 0, "plain LCD text drew no ink");

    for draw in [Draw::Rotated, Draw::Scaled] {
        let image = render(RenderConfig {
            draw,
            ..Default::default()
        });
        assert_eq!(
            chromatic_pixels(&image),
            0,
            "{draw:?} text must fall back to grayscale"
        );
        assert!(ink(&image) > 0, "{draw:?} fallback drew no ink");
    }

    // `round_text_to_pixels` normalizes text placement before tessellation, so
    // a 0.25-point layer translation must not create additional fringing. The
    // shader still has its texel-center fallback for any non-rounded path.
    let fractional = render(RenderConfig {
        draw: Draw::FractionalTranslation,
        ..Default::default()
    });
    assert_eq!(
        chromatic_pixels(&fractional),
        plain_chromatic,
        "fractional placement must not add chromatic pixels"
    );
}

#[test]
#[ignore = "manual hardware GPU test; set TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1"]
fn bgr_swaps_the_colored_channel_order() {
    require_hardware("bgr_swaps_the_colored_channel_order");

    let rgb = render(RenderConfig::default());
    let bgr = render(RenderConfig {
        mode: SubpixelMode::Bgr,
        ..Default::default()
    });

    // Without these the assertion below also holds for two identical grayscale images, so it
    // would go green in exactly the failure this suite exists to catch: a device that
    // advertises dual-source blending but draws the atlas through the ordinary pipeline.
    assert!(
        chromatic_pixels(&rgb) > 0 && chromatic_pixels(&bgr) > 0,
        "both renders must actually be sub-pixel rendered"
    );
    assert_ne!(
        rgb.as_raw(),
        bgr.as_raw(),
        "BGR must not produce the RGB image"
    );

    let red: u64 = rgb.pixels().map(|pixel| u64::from(pixel.0[0])).sum();
    let blue_of_bgr: u64 = bgr.pixels().map(|pixel| u64::from(pixel.0[2])).sum();
    assert_eq!(red, blue_of_bgr);
}

#[test]
#[ignore = "manual hardware GPU test; set TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1"]
fn tinted_contrast_and_text_alpha_still_blend() {
    require_hardware("tinted_contrast_and_text_alpha_still_blend");

    let no_contrast = render(RenderConfig {
        contrast: 0.0,
        ..Default::default()
    });
    let contrast = render(RenderConfig::default());
    assert!(
        tinted_contrast_changes(&no_contrast, &contrast) > 10,
        "tinted LCD coverage must also receive the contrast curve"
    );

    let translucent = render(RenderConfig {
        text_color: Color32::from_white_alpha(128),
        ..Default::default()
    });
    assert!(ink(&translucent) > 0, "translucent text drew no ink");
    assert!(
        ink(&translucent) < ink(&contrast),
        "translucent text must blend below opaque text"
    );

    let colored = render(RenderConfig {
        text_color: Color32::from_rgb(220, 80, 20),
        ..Default::default()
    });
    assert!(ink(&colored) > 0, "colored text drew no ink");
    assert!(chromatic_pixels(&colored) > 0);
}

#[test]
#[ignore = "manual hardware GPU test; set TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1"]
fn hardware_sampling_dithering_and_srgb_entry_points_still_render_lcd() {
    require_hardware("hardware_sampling_dithering_and_srgb_entry_points_still_render_lcd");

    for dithering in [false, true] {
        let image = render(RenderConfig {
            options: RendererOptions {
                dithering,
                predictable_texture_filtering: false,
                ..RendererOptions::default()
            },
            ..Default::default()
        });
        assert!(ink(&image) > 0);
        assert!(chromatic_pixels(&image) > 0);
    }

    let srgb = render(RenderConfig {
        srgb_target: true,
        ..Default::default()
    });
    assert!(ink(&srgb) > 0, "sRGB-framebuffer LCD path drew no ink");
    assert!(chromatic_pixels(&srgb) > 0);
}

struct SoftwareTestRenderer {
    painter: egui_software::Painter,
}

impl SoftwareTestRenderer {
    fn new() -> Self {
        Self {
            painter: egui_software::Painter::new(),
        }
    }
}

impl egui_kittest::TestRenderer for SoftwareTestRenderer {
    fn handle_delta(&mut self, delta: &mut egui::TexturesDelta) {
        #[expect(clippy::iter_over_hash_type)] // Order doesn't matter here
        for (id, images) in delta.set.drain() {
            for image in images {
                self.painter.set_texture(id, &image);
            }
        }
        #[expect(clippy::iter_over_hash_type)] // Order doesn't matter here
        for id in delta.free.drain() {
            self.painter.free_texture(id);
        }
    }

    fn render(
        &mut self,
        ctx: &egui::Context,
        output: &egui::FullOutput,
    ) -> Result<image::RgbaImage, String> {
        let opts = ctx.fonts(|f| *f.options());
        self.painter.set_text_options(&opts);
        let ppp = ctx.pixels_per_point();
        let size = ctx.content_rect().size() * ppp;
        let width = size.x.round() as u32;
        let height = size.y.round() as u32;
        let mut buf = vec![0_u32; (width * height) as usize];
        let mut target = egui_software::Target::new(&mut buf, width, height)
            .ok_or_else(|| "invalid software target dimensions".to_owned())?;
        let font_tex_size = ctx.fonts(|f| f.font_image_size());
        let shape_ctx = egui_software::ShapeContext {
            pixels_per_point: ppp,
            options: ctx.tessellation_options(|o| o.clone()),
            font_tex_size,
            prepared_discs: Vec::new(),
        };
        self.painter
            .paint_shapes(&mut target, &shape_ctx, output.shapes.clone());
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for px in buf {
            rgba.push(((px >> 16) & 0xff) as u8);
            rgba.push(((px >> 8) & 0xff) as u8);
            rgba.push((px & 0xff) as u8);
            rgba.push(255);
        }
        image::RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| "failed to build RgbaImage".to_owned())
    }
}

fn available_dual_source_backends() -> Vec<wgpu::Backend> {
    let filter = std::env::var("TASKMAN_GPU_BACKEND")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase());
    let setup = egui_kittest::wgpu::default_wgpu_setup();
    let instance = pollster::block_on(setup.new_instance());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    let mut backends = Vec::new();
    for backend in [wgpu::Backend::Dx12, wgpu::Backend::Vulkan] {
        if let Some(ref want) = filter {
            let matches_filter = (want == "dx12" || want == "d3d12")
                && backend == wgpu::Backend::Dx12
                || want == "vulkan" && backend == wgpu::Backend::Vulkan;
            if !matches_filter {
                continue;
            }
        }
        let has_adapter = adapters.iter().any(|adapter| {
            adapter.get_info().backend == backend
                && adapter.get_info().device_type != wgpu::DeviceType::Cpu
                && adapter
                    .features()
                    .contains(wgpu::Features::DUAL_SOURCE_BLENDING)
        });
        if has_adapter {
            backends.push(backend);
        }
    }
    backends
}

fn setup_for_dual_source_backend(backend: wgpu::Backend) -> WgpuSetup {
    let mut setup = egui_kittest::wgpu::default_wgpu_setup();
    let WgpuSetup::CreateNew(create_new) = &mut setup else {
        unreachable!("default_wgpu_setup always creates a new setup");
    };
    create_new.native_adapter_selector = Some(Arc::new(move |adapters, _surface| {
        adapters
            .iter()
            .find(|adapter| {
                adapter.get_info().backend == backend
                    && adapter.get_info().device_type != wgpu::DeviceType::Cpu
                    && adapter
                        .features()
                        .contains(wgpu::Features::DUAL_SOURCE_BLENDING)
            })
            .cloned()
            .ok_or_else(|| format!("no dual-source-capable {backend:?} adapter"))
    }));
    setup
}

/// One CPU-vs-GPU parity scene: text colours, background and the text options under test.
#[derive(Clone, Copy)]
struct ParityCase {
    label: &'static str,
    theme: Theme,
    bg: Color32,
    fg_primary: Color32,
    fg_dim: Color32,
    subpixel: SubpixelMode,
    gamma: f32,
    contrast: f32,
    bg_luma: f32,
    inverse_bg_luma: f32,
}

/// TaskMan's grayscale curve for `Sharp` (`tm_ui::theme::grayscale_transfer`).
const SHARP_GRAY: egui::epaint::FontColorTransferFunction =
    egui::epaint::FontColorTransferFunction::PolarityGamma {
        light_text: 0.80,
        dark_text: 1.15,
    };

/// A face to run the parity scene with, beyond egui's bundled default.
///
/// The product renders the OS UI font (Segoe UI Variable on Windows 11) with TaskMan's
/// `Sharp` hinting tweak, so parity on the bundled face alone would leave the shipped
/// glyph shapes and hinting unchecked.
fn system_ui_font() -> Option<(&'static str, Vec<u8>)> {
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\Windows\Fonts\SegUIVar.ttf",
            r"C:\Windows\Fonts\segoeui.ttf",
        ]
    } else {
        &[
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ]
    };
    candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok().map(|bytes| (*path, bytes)))
}

fn sharp_font_definitions(bytes: &[u8]) -> egui::FontDefinitions {
    use egui::epaint::text::{FontData, FontTweak, HintingTarget, SmoothHinting};
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "system-ui".to_owned(),
        Arc::new(FontData::from_owned(bytes.to_vec()).tweak(FontTweak {
            hinting_target: HintingTarget::Smooth(SmoothHinting {
                light: false,
                symmetric_rendering: false,
                preserve_linear_metrics: false,
            }),
            ..Default::default()
        })),
    );
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, "system-ui".to_owned());
    }
    fonts
}

fn render_parity_scene<R: egui_kittest::TestRenderer + 'static>(
    renderer: R,
    case: ParityCase,
    ppp: f32,
    font: Option<&[u8]>,
) -> image::RgbaImage {
    let mut harness = Harness::builder()
        .with_size(Vec2::new(260.0, 96.0))
        .with_pixels_per_point(ppp)
        .renderer(renderer)
        .build_ui(move |ui| {
            ui.ctx().set_theme(case.theme);
            ui.ctx().style_mut_of(case.theme, |style| {
                let opts = &mut style.visuals.text_options;
                opts.subpixel = case.subpixel;
                opts.text_gamma = case.gamma;
                opts.text_contrast = case.contrast;
                opts.text_background_luma = case.bg_luma;
                opts.text_inverse_background_luma = case.inverse_bg_luma;
                opts.color_transfer_function = SHARP_GRAY;
            });
            ui.painter()
                .rect_filled(ui.ctx().content_rect(), 0.0, case.bg);
            let primary = ui.painter().layout_no_wrap(
                "Hamburgefonstiv 18.4%".to_owned(),
                FontId::proportional(13.0),
                case.fg_primary,
            );
            ui.painter().add(TextShape::new(
                Pos2::new(10.0, 10.0),
                primary,
                case.fg_primary,
            ));
            let dim = ui.painter().layout_no_wrap(
                "Secondary 2410 MB".to_owned(),
                FontId::proportional(13.0),
                case.fg_dim,
            );
            ui.painter()
                .add(TextShape::new(Pos2::new(10.0, 34.0), dim, case.fg_dim));
            let mut italic_job = egui::text::LayoutJob::default();
            italic_job.append(
                "Italic fallback 42%",
                0.0,
                egui::TextFormat {
                    font_id: FontId::proportional(13.0),
                    color: case.fg_primary,
                    italics: true,
                    ..Default::default()
                },
            );
            let italic = ui.painter().layout_job(italic_job);
            ui.painter().add(TextShape::new(
                Pos2::new(10.0, 58.0),
                italic,
                case.fg_primary,
            ));
        });
    if let Some(bytes) = font {
        harness.ctx.set_fonts(sharp_font_definitions(bytes));
    }
    harness.run();
    harness.render().expect("parity render")
}

/// Per-channel difference statistics over pixels that are not plain background.
struct Divergence {
    max: u8,
    mean: f64,
    worst: (u32, u32, [u8; 4], [u8; 4]),
}

fn divergence(
    cpu: &image::RgbaImage,
    gpu: &image::RgbaImage,
    bg: Color32,
    rows: std::ops::Range<u32>,
) -> Divergence {
    let mut max = 0_u8;
    let mut worst = (0_u32, 0_u32, [0_u8; 4], [0_u8; 4]);
    let mut total = 0_u64;
    let mut channels = 0_u64;
    for (idx, (c, g)) in cpu.pixels().zip(gpu.pixels()).enumerate() {
        if !rows.contains(&(idx as u32 / cpu.width())) {
            continue;
        }
        for ch in 0..3 {
            let diff = c.0[ch].abs_diff(g.0[ch]);
            if diff > max {
                max = diff;
                worst = (idx as u32 % cpu.width(), idx as u32 / cpu.width(), c.0, g.0);
            }
            if c.0[ch] != bg[ch] || g.0[ch] != bg[ch] {
                total += u64::from(diff);
                channels += 1;
            }
        }
    }
    Divergence {
        max,
        mean: total as f64 / channels.max(1) as f64,
        worst,
    }
}

/// `(max, mean)` per-channel LSB bounds for upright and italic rows.
struct ParityBounds {
    upright: (u8, f64),
    italic: (u8, f64),
}

/// Bounds from the D3D12 and Vulkan measurements of 2026-11-27; see
/// `llm-wiki/render-pipeline.md`.
///
/// The LCD maxima are isolated pixels with known causes, not drift: where two glyph quads
/// overlap (adjacent glyphs' one-pixel LCD margins) the GPU blends the second glyph as if
/// it landed on the calibrated surface, but the pixel already holds the first glyph's ink
/// (measured up to 7 LSB; 10 under an earlier, steeper 1.98 Light gamma); italic rows add
/// the 1/16 px vs 1/256 px vertex-snapping difference on sheared edges (up to 8, was 13).
/// The maxima keep headroom for the steeper curve a user's ClearType tuning can select;
/// the means are what pins the calibration.
fn parity_bounds(grayscale: bool) -> ParityBounds {
    if grayscale {
        ParityBounds {
            upright: (3, 0.6),
            italic: (4, 0.75),
        }
    } else {
        ParityBounds {
            upright: (11, 1.15),
            italic: (14, 1.7),
        }
    }
}

/// The real shaders against the CPU renderer, on the surfaces and scales TaskMan ships.
///
/// Tolerances are the measured worst case plus a small margin, not a loose bound: a
/// regression in `egui_lcd.wgsl`'s calibration or `egui.wgsl`'s grayscale curve shows up as
/// a broken bound here. Every case prints its measured numbers, so the documentation can
/// quote real results.
#[test]
#[ignore = "manual hardware GPU test; set TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1"]
fn cpu_and_gpu_match_on_dark_and_light_surfaces() {
    require_hardware("cpu_and_gpu_match_on_dark_and_light_surfaces");
    let backends = available_dual_source_backends();
    assert!(
        !backends.is_empty(),
        "no D3D12 or Vulkan dual-source adapter matched"
    );

    let dark = |label, bg, fg_primary, fg_dim, subpixel, gamma, contrast| ParityCase {
        label,
        theme: Theme::Dark,
        bg,
        fg_primary,
        fg_dim,
        subpixel,
        gamma,
        contrast,
        bg_luma: 0.13,
        inverse_bg_luma: -1.0,
    };
    let light = |label, bg, fg_primary, fg_dim, subpixel| ParityCase {
        label,
        theme: Theme::Light,
        bg,
        fg_primary,
        fg_dim,
        subpixel,
        gamma: 1.584,
        contrast: 0.0,
        bg_luma: 0.96,
        inverse_bg_luma: 0.13,
    };
    let rgb = Color32::from_rgb;
    let cases = [
        dark(
            "lcd_dark_sharp_#191919",
            rgb(0x19, 0x19, 0x19),
            Color32::WHITE,
            rgb(0x9d, 0x9d, 0x9d),
            SubpixelMode::Rgb,
            1.404,
            0.0,
        ),
        dark(
            "lcd_dark_sharp_#2b2b2b",
            rgb(0x2b, 0x2b, 0x2b),
            Color32::WHITE,
            rgb(0x9d, 0x9d, 0x9d),
            SubpixelMode::Rgb,
            1.404,
            0.0,
        ),
        dark(
            "lcd_dark_standard_translucent",
            rgb(0x21, 0x21, 0x21),
            Color32::from_white_alpha(128),
            rgb(0x9d, 0x9d, 0x9d),
            SubpixelMode::Rgb,
            1.8,
            0.5,
        ),
        light(
            "lcd_light_sharp_#f3f3f3",
            rgb(0xf3, 0xf3, 0xf3),
            rgb(0x1a, 0x1a, 0x1a),
            rgb(0x5f, 0x5f, 0x5f),
            SubpixelMode::Rgb,
        ),
        // White toast text in the Light theme: opposite polarity, inverse surface.
        light(
            "lcd_light_sharp_toast_#212121",
            rgb(0x21, 0x21, 0x21),
            Color32::WHITE,
            rgb(0xc8, 0xc8, 0xc8),
            SubpixelMode::Rgb,
        ),
        dark(
            "gray_dark_sharp_#191919",
            rgb(0x19, 0x19, 0x19),
            Color32::WHITE,
            rgb(0x9d, 0x9d, 0x9d),
            SubpixelMode::Off,
            1.404,
            0.0,
        ),
        light(
            "gray_light_sharp_#f3f3f3",
            rgb(0xf3, 0xf3, 0xf3),
            rgb(0x1a, 0x1a, 0x1a),
            rgb(0x5f, 0x5f, 0x5f),
            SubpixelMode::Off,
        ),
        light(
            "gray_light_sharp_toast_#212121",
            rgb(0x21, 0x21, 0x21),
            Color32::WHITE,
            rgb(0xc8, 0xc8, 0xc8),
            SubpixelMode::Off,
        ),
    ];

    let renderer_configs = [
        // eframe's default options and an `Unorm` surface: what TaskMan ships.
        (
            "hardware_dithered",
            RendererOptions {
                dithering: true,
                predictable_texture_filtering: false,
                ..RendererOptions::default()
            },
            false,
        ),
        ("predictable", RendererOptions::PREDICTABLE, false),
        (
            "hardware_dithered_srgb",
            RendererOptions {
                dithering: true,
                predictable_texture_filtering: false,
                ..RendererOptions::default()
            },
            true,
        ),
        ("predictable_srgb", RendererOptions::PREDICTABLE, true),
    ];

    let system = system_ui_font();
    match &system {
        Some((path, _)) => eprintln!("parity: system UI font {path}"),
        None => eprintln!("parity: no system UI font found; bundled face only"),
    }
    let mut fonts: Vec<(&str, Option<&[u8]>)> = vec![("bundled", None)];
    if let Some((_, bytes)) = &system {
        fonts.push(("system", Some(bytes.as_slice())));
    }

    let mut failures = Vec::new();
    for backend in backends {
        // One device per backend; every render compiles a fresh renderer on it.
        let base = egui_kittest::wgpu::create_render_state(
            setup_for_dual_source_backend(backend),
            RendererOptions::PREDICTABLE,
        );
        let existing = || {
            WgpuSetup::Existing(egui_wgpu::WgpuSetupExisting {
                instance: base.instance.clone(),
                adapter: base.adapter.clone(),
                device: base.device.clone(),
                queue: base.queue.clone(),
            })
        };
        for &(font_label, font) in &fonts {
            for ppp in [1.0_f32, 1.25, 1.5, 2.0] {
                // Every renderer configuration on the bundled face at the two shipped
                // scales; the shipped configuration everywhere else.
                let configs = if font.is_none() && (ppp == 1.0 || ppp == 1.5) {
                    &renderer_configs[..]
                } else {
                    &renderer_configs[..1]
                };
                for case in cases {
                    let cpu = render_parity_scene(SoftwareTestRenderer::new(), case, ppp, font);
                    for &(opt_label, options, srgb_target) in configs {
                        let gpu_renderer = if srgb_target {
                            WgpuTestRenderer::from_setup_and_format(
                                existing(),
                                options,
                                wgpu::TextureFormat::Rgba8UnormSrgb,
                            )
                        } else {
                            WgpuTestRenderer::from_setup_and_options(existing(), options)
                        };
                        let gpu = render_parity_scene(gpu_renderer, case, ppp, font);
                        // Upright rows (1:1 glyph blits) and the italic row (triangle path)
                        // are bounded separately: the CPU rasterizer snaps vertices to 1/16 px,
                        // the GPU to 1/256 px, which moves sheared glyph edges by a fraction of
                        // a texel -- a coverage difference that steep blend curves amplify.
                        let italic_top = (52.0 * ppp) as u32;
                        let upright = divergence(&cpu, &gpu, case.bg, 0..italic_top);
                        let italic = divergence(&cpu, &gpu, case.bg, italic_top..u32::MAX);
                        let what = format!(
                            "{backend:?} {font_label} {} ppp={ppp} {opt_label}",
                            case.label
                        );
                        eprintln!(
                            "parity: {what}: upright max={} mean={:.3} italic max={} mean={:.3}",
                            upright.max, upright.mean, italic.max, italic.mean
                        );
                        // The ordinary pipeline blends in linear space on sRGB targets (stock
                        // egui behaviour, every fill included), so grayscale text there cannot
                        // match the gamma-space CPU blend; TaskMan always selects an `Unorm`
                        // surface. Reported above, not bounded.
                        if case.subpixel.is_off() && srgb_target {
                            continue;
                        }
                        let bounds = parity_bounds(case.subpixel.is_off());
                        for (row_label, d, (max_allowed, mean_allowed)) in [
                            ("upright", &upright, bounds.upright),
                            ("italic", &italic, bounds.italic),
                        ] {
                            if d.max > max_allowed || d.mean > mean_allowed {
                                // `TASKMAN_PARITY_DUMP=<dir>` keeps both frames of a failing
                                // case for inspection.
                                if let Some(dir) = std::env::var_os("TASKMAN_PARITY_DUMP") {
                                    let dir = std::path::PathBuf::from(dir);
                                    std::fs::create_dir_all(&dir).ok();
                                    let stem = what.replace([' ', '#', '='], "_");
                                    cpu.save(dir.join(format!("{stem}-cpu.png"))).ok();
                                    gpu.save(dir.join(format!("{stem}-gpu.png"))).ok();
                                }
                                failures.push(format!(
                                    "{what} {row_label}: max_diff={} at ({},{}) cpu={:?}                                      gpu={:?}, mean_diff={:.3}",
                                    d.max, d.worst.0, d.worst.1, d.worst.2, d.worst.3, d.mean
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "GPU diverged from CPU:\n{}",
        failures.join("\n")
    );
}
