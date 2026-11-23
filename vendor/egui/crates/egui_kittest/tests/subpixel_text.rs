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
    let red: u64 = rgb.pixels().map(|pixel| u64::from(pixel.0[0])).sum();
    let blue_of_bgr: u64 = bgr.pixels().map(|pixel| u64::from(pixel.0[2])).sum();
    assert_eq!(red, blue_of_bgr);
}

#[test]
#[ignore = "manual hardware GPU test; set TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1"]
fn neutral_coverage_receives_contrast_and_text_alpha_still_blends() {
    require_hardware("neutral_coverage_receives_contrast_and_text_alpha_still_blends");

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
