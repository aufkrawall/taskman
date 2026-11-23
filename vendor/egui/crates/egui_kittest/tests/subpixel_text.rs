#![cfg(all(feature = "wgpu", not(target_arch = "wasm32")))]

//! Headless WGPU coverage for the managed-font dual-source LCD pipeline.
//!
//! This test is intentionally explicit about skipping: if the host has no adapter that
//! supports dual-source blending, it says so and returns instead of claiming a pass.

use std::sync::Arc;

use egui::{
    Color32, FontId, Pos2, Theme, Vec2, emath::TSTransform, epaint::TextShape,
    epaint::text::SubpixelMode,
};
use egui_kittest::{Harness, wgpu::WgpuTestRenderer};
use egui_wgpu::{WgpuSetup, wgpu};

#[derive(Clone, Copy, PartialEq)]
enum Draw {
    Plain,
    Rotated,
    Scaled,
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
            && adapter
                .features()
                .contains(wgpu::Features::DUAL_SOURCE_BLENDING)
    })
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
                    && adapter
                        .features()
                        .contains(wgpu::Features::DUAL_SOURCE_BLENDING)
            })
            .cloned()
            .ok_or_else(|| "no dual-source-capable wgpu adapter".to_owned())
    }));
    setup
}

fn render(mode: SubpixelMode, draw: Draw) -> image::RgbaImage {
    let renderer = WgpuTestRenderer::from_setup(setup_for_dual_source());
    let mut harness = Harness::builder()
        .with_size(Vec2::new(240.0, 60.0))
        .with_pixels_per_point(1.0)
        .renderer(renderer)
        .build_ui(move |ui| {
            ui.ctx().set_theme(Theme::Dark);
            ui.ctx().style_mut_of(Theme::Dark, |style| {
                style.visuals.text_options.subpixel = mode;
            });
            ui.painter()
                .rect_filled(ui.ctx().content_rect(), 0.0, Color32::BLACK);
            let galley = ui.painter().layout_no_wrap(
                "Hamburgefonstiv".to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
            );
            let mut shape = TextShape::new(Pos2::new(10.0, 20.0), galley, Color32::WHITE);
            if draw == Draw::Rotated {
                shape = shape.with_angle(0.2);
            }
            if draw == Draw::Scaled {
                ui.ctx()
                    .set_transform_layer(ui.layer_id(), TSTransform::from_scaling(1.37));
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

#[test]
fn dual_source_text_is_colored_on_pixels_and_grayscale_off_grid() {
    if !dual_source_available() {
        eprintln!(
            "SKIP dual_source_text_is_colored_on_pixels_and_grayscale_off_grid: \
             no wgpu adapter with DUAL_SOURCE_BLENDING"
        );
        return;
    }

    let lcd = render(SubpixelMode::Rgb, Draw::Plain);
    assert!(
        chromatic_pixels(&lcd) > 50,
        "plain LCD text did not produce color fringes"
    );
    assert!(ink(&lcd) > 0, "plain LCD text drew no ink");

    for draw in [Draw::Rotated, Draw::Scaled] {
        let image = render(SubpixelMode::Rgb, draw);
        assert_eq!(
            chromatic_pixels(&image),
            0,
            "off-grid text must fall back to grayscale"
        );
        assert!(ink(&image) > 0, "off-grid fallback drew no ink");
    }
}

#[test]
fn bgr_swaps_the_colored_channel_order() {
    if !dual_source_available() {
        eprintln!(
            "SKIP bgr_swaps_the_colored_channel_order: \
             no wgpu adapter with DUAL_SOURCE_BLENDING"
        );
        return;
    }

    let rgb = render(SubpixelMode::Rgb, Draw::Plain);
    let bgr = render(SubpixelMode::Bgr, Draw::Plain);
    let red: u64 = rgb.pixels().map(|pixel| u64::from(pixel.0[0])).sum();
    let blue_of_bgr: u64 = bgr.pixels().map(|pixel| u64::from(pixel.0[2])).sum();
    assert_eq!(red, blue_of_bgr);
}
