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
        let opts = ctx.fonts(|f| f.options().clone());
        self.painter
            .set_subpixel(opts.subpixel, opts.text_gamma, opts.text_contrast);
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

fn render_parity_scene<R: egui_kittest::TestRenderer + 'static>(
    renderer: R,
    theme: Theme,
    bg: Color32,
    fg_primary: Color32,
    fg_dim: Color32,
    gamma: f32,
    contrast: f32,
    bg_luma: f32,
) -> image::RgbaImage {
    let mut harness = Harness::builder()
        .with_size(Vec2::new(260.0, 72.0))
        .with_pixels_per_point(1.0)
        .renderer(renderer)
        .build_ui(move |ui| {
            ui.ctx().set_theme(theme);
            ui.ctx().style_mut_of(theme, |style| {
                let opts = &mut style.visuals.text_options;
                opts.subpixel = SubpixelMode::Rgb;
                opts.text_gamma = gamma;
                opts.text_contrast = contrast;
                opts.text_background_luma = bg_luma;
            });
            ui.painter().rect_filled(ui.ctx().content_rect(), 0.0, bg);
            let primary = ui.painter().layout_no_wrap(
                "Hamburgefonstiv 18.4%".to_owned(),
                FontId::proportional(13.0),
                fg_primary,
            );
            ui.painter()
                .add(TextShape::new(Pos2::new(10.0, 12.0), primary, fg_primary));
            let dim = ui.painter().layout_no_wrap(
                "Secondary 2410 MB".to_owned(),
                FontId::proportional(13.0),
                fg_dim,
            );
            ui.painter()
                .add(TextShape::new(Pos2::new(10.0, 38.0), dim, fg_dim));
        });
    harness.run();
    harness.render().expect("parity render")
}

#[test]
#[ignore = "manual hardware GPU test; set TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1"]
fn cpu_and_gpu_match_on_dark_and_light_surfaces() {
    require_hardware("cpu_and_gpu_match_on_dark_and_light_surfaces");

    let cases = [
        (
            "dark_surface_#191919",
            Theme::Dark,
            Color32::from_rgb(0x19, 0x19, 0x19),
            Color32::WHITE,
            Color32::from_rgb(0x9d, 0x9d, 0x9d),
            1.35,
            0.0,
            0.13,
        ),
        (
            "dark_elevated_#2b2b2b",
            Theme::Dark,
            Color32::from_rgb(0x2b, 0x2b, 0x2b),
            Color32::WHITE,
            Color32::from_rgb(0x9d, 0x9d, 0x9d),
            1.35,
            0.0,
            0.13,
        ),
        (
            "light_surface_#f3f3f3",
            Theme::Light,
            Color32::from_rgb(0xf3, 0xf3, 0xf3),
            Color32::from_rgb(0x1a, 0x1a, 0x1a),
            Color32::from_rgb(0x5f, 0x5f, 0x5f),
            1.9,
            0.15,
            0.96,
        ),
    ];

    for (label, theme, bg, fg_primary, fg_dim, gamma, contrast, bg_luma) in cases {
        let cpu = render_parity_scene(
            SoftwareTestRenderer::new(),
            theme,
            bg,
            fg_primary,
            fg_dim,
            gamma,
            contrast,
            bg_luma,
        );
        for srgb_target in [false, true] {
            let gpu_renderer = if srgb_target {
                WgpuTestRenderer::from_setup_and_format(
                    setup_for_dual_source(),
                    RendererOptions::PREDICTABLE,
                    wgpu::TextureFormat::Rgba8UnormSrgb,
                )
            } else {
                WgpuTestRenderer::from_setup_and_options(
                    setup_for_dual_source(),
                    RendererOptions::PREDICTABLE,
                )
            };
            let gpu = render_parity_scene(
                gpu_renderer,
                theme,
                bg,
                fg_primary,
                fg_dim,
                gamma,
                contrast,
                bg_luma,
            );
            let mut max_diff = 0_u8;
            let mut total_diff = 0_u64;
            let mut non_bg_channels = 0_u64;
            for (c, g) in cpu.pixels().zip(gpu.pixels()) {
                for ch in 0..3 {
                    let diff = c.0[ch].abs_diff(g.0[ch]);
                    max_diff = max_diff.max(diff);
                    if c.0[ch] != bg[ch] || g.0[ch] != bg[ch] {
                        total_diff += u64::from(diff);
                        non_bg_channels += 1;
                    }
                }
            }
            let mean_diff = total_diff as f64 / non_bg_channels.max(1) as f64;
            assert!(
                max_diff <= 6 && mean_diff <= 1.25,
                "{label} (srgb={srgb_target}) diverged from CPU: max_diff={max_diff}, mean_diff={mean_diff:.3}"
            );
        }
    }
}
