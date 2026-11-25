use std::sync::Arc;
use std::{iter::once, time::Duration};

use egui::TexturesDelta;
use egui_wgpu::{RenderState, ScreenDescriptor, WgpuSetup, wgpu};
use image::RgbaImage;

use crate::texture_to_image::texture_to_image;

/// Timeout for waiting on the GPU to finish rendering.
///
/// Windows will reset native drivers after 2 seconds of being stuck (known was TDR - timeout detection & recovery).
/// However, software rasterizers like lavapipe may not do that and take longer if there's a lot of work in flight.
/// In the end, what we really want to protect here against is undetected errors that lead to device loss
/// and therefore infinite waits it happens occasionally on MacOS/Metal as of writing.
pub(crate) const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default wgpu setup used for the wgpu renderer.
pub fn default_wgpu_setup() -> egui_wgpu::WgpuSetup {
    // No display handle needed for headless testing — we don't present to a window.
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();

    // WebGPU not supported yet since we rely on blocking screenshots.
    setup
        .instance_descriptor
        .backends
        .remove(wgpu::Backends::BROWSER_WEBGPU);

    // Prefer software rasterizers.
    setup.native_adapter_selector = Some(Arc::new(|adapters, _surface| {
        let mut adapters = adapters.iter().collect::<Vec<_>>();

        // Adapters are already sorted by preferred backend by wgpu, but let's be explicit.
        adapters.sort_by_key(|a| match a.get_info().backend {
            wgpu::Backend::Metal => 0,
            wgpu::Backend::Vulkan => 1,
            wgpu::Backend::Dx12 => 2,
            wgpu::Backend::Gl => 4,
            wgpu::Backend::BrowserWebGpu => 6,
            wgpu::Backend::Noop => 7,
        });

        // Prefer CPU adapters, otherwise if we can't, prefer discrete GPU over integrated GPU.
        adapters.sort_by_key(|a| match a.get_info().device_type {
            wgpu::DeviceType::Cpu => 0, // CPU is the best for our purposes!
            wgpu::DeviceType::DiscreteGpu => 1,
            wgpu::DeviceType::Other
            | wgpu::DeviceType::IntegratedGpu
            | wgpu::DeviceType::VirtualGpu => 2,
        });

        adapters
            .first()
            .map(|a| (*a).clone())
            .ok_or_else(|| "No adapter found".to_owned())
    }));

    egui_wgpu::WgpuSetup::CreateNew(setup)
}

pub fn create_render_state(
    setup: WgpuSetup,
    options: egui_wgpu::RendererOptions,
) -> egui_wgpu::RenderState {
    // No display handle needed for headless testing — we don't present to a window.
    let instance = pollster::block_on(setup.new_instance());

    pollster::block_on(egui_wgpu::RenderState::create(
        &egui_wgpu::WgpuConfiguration {
            wgpu_setup: setup,
            ..Default::default()
        },
        &instance,
        None,
        options,
    ))
    .expect("Failed to create render state")
}

/// Create a headless render state whose pipelines are still to be compiled.
///
/// `create_render_state_with_format` needs the target format settled before the pipelines are
/// baked into a renderer, which eager creation does not allow.
fn create_render_state_without_renderer(setup: WgpuSetup) -> egui_wgpu::RenderState {
    // No display handle needed for headless testing — we don't present to a window.
    let instance = pollster::block_on(setup.new_instance());

    pollster::block_on(egui_wgpu::RenderState::create_without_renderer(
        &egui_wgpu::WgpuConfiguration {
            wgpu_setup: setup,
            ..Default::default()
        },
        &instance,
        None,
    ))
    .expect("Failed to create render state")
}

/// Create a headless renderer with an explicit target format.
///
/// The default headless setup selects a gamma-unaware target. This variant exists so the
/// manual LCD hardware suite can exercise the sRGB-framebuffer shader entry point.
pub fn create_render_state_with_format(
    setup: WgpuSetup,
    options: egui_wgpu::RendererOptions,
    target_format: wgpu::TextureFormat,
) -> egui_wgpu::RenderState {
    let mut state = create_render_state_without_renderer(setup);
    // The pipelines bake the target format in, so it has to be selected before the renderer
    // is compiled. The slot behind `ensure_renderer` is shared with every clone of the state,
    // so the renderer stays reachable through `state` even though the returned lock is
    // discarded here.
    state.target_format = target_format;
    let _renderer = state.ensure_renderer(options);
    state
}

/// Utility to render snapshots from a [`crate::Harness`] using [`egui_wgpu`].
pub struct WgpuTestRenderer {
    render_state: RenderState,
}

impl Default for WgpuTestRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl WgpuTestRenderer {
    /// Create a new [`WgpuTestRenderer`] with the default setup.
    pub fn new() -> Self {
        Self {
            render_state: create_render_state(
                default_wgpu_setup(),
                egui_wgpu::RendererOptions::PREDICTABLE,
            ),
        }
    }

    /// Create a new [`WgpuTestRenderer`] with the given setup.
    pub fn from_setup(setup: WgpuSetup) -> Self {
        Self::from_setup_and_options(setup, egui_wgpu::RendererOptions::PREDICTABLE)
    }

    /// Create a new [`WgpuTestRenderer`] from a setup and explicit render options.
    pub fn from_setup_and_options(setup: WgpuSetup, options: egui_wgpu::RendererOptions) -> Self {
        Self {
            render_state: create_render_state(setup, options),
        }
    }

    /// Create a new [`WgpuTestRenderer`] with an explicit target framebuffer format.
    pub fn from_setup_and_format(
        setup: WgpuSetup,
        options: egui_wgpu::RendererOptions,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        Self {
            render_state: create_render_state_with_format(setup, options, target_format),
        }
    }

    /// Create a new [`WgpuTestRenderer`] from an existing [`RenderState`].
    ///
    /// # Panics
    /// Panics if the [`RenderState`] has been used before.
    pub fn from_render_state(render_state: RenderState) -> Self {
        assert!(
            render_state
                .renderer()
                .expect("a created RenderState has a renderer")
                .read()
                .texture(&egui::epaint::TextureId::Managed(0))
                .is_none(),
            "The RenderState passed in has been used before, pass in a fresh RenderState instead."
        );
        Self { render_state }
    }

    /// Create a new [`WgpuTestRenderer`] with custom render options.
    pub fn with_render_options(options: egui_wgpu::RendererOptions) -> Self {
        Self {
            render_state: create_render_state(default_wgpu_setup(), options),
        }
    }
}

impl crate::TestRenderer for WgpuTestRenderer {
    #[cfg(feature = "eframe")]
    fn setup_eframe(&self, cc: &mut eframe::CreationContext<'_>, frame: &mut eframe::Frame) {
        cc.wgpu_render_state = Some(self.render_state.clone());
        frame.wgpu_render_state = Some(self.render_state.clone());
    }

    fn handle_delta(&mut self, delta: &mut TexturesDelta) {
        let mut renderer = self
            .render_state
            .renderer()
            .expect("a created RenderState has a renderer")
            .write();
        #[expect(clippy::iter_over_hash_type)] // Order doesn't matter here
        for (id, images) in delta.set.drain() {
            for image in images {
                renderer.update_texture(
                    &self.render_state.device,
                    &self.render_state.queue,
                    id,
                    &image,
                );
            }
        }

        #[expect(clippy::iter_over_hash_type)] // Order doesn't matter here
        for id in delta.free.drain() {
            renderer.free_texture(&id);
        }
    }

    /// Render the [`crate::Harness`] and return the resulting image.
    fn render(
        &mut self,
        ctx: &egui::Context,
        output: &egui::FullOutput,
    ) -> Result<RgbaImage, String> {
        let mut renderer = self
            .render_state
            .renderer()
            .expect("a created RenderState has a renderer")
            .write();
        let (subpixel, gamma, contrast) = ctx.fonts(|fonts| {
            let options = fonts.options();
            (options.subpixel, options.text_gamma, options.text_contrast)
        });
        renderer.set_subpixel_text_options(subpixel, gamma, contrast);

        let mut encoder =
            self.render_state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Egui Command Encoder"),
                });

        let size = ctx.content_rect().size() * ctx.pixels_per_point();
        let screen = ScreenDescriptor {
            pixels_per_point: ctx.pixels_per_point(),
            size_in_pixels: [size.x.round() as u32, size.y.round() as u32],
        };

        let tessellated = ctx.tessellate(output.shapes.clone(), ctx.pixels_per_point());

        let user_buffers = renderer.update_buffers(
            &self.render_state.device,
            &self.render_state.queue,
            &mut encoder,
            &tessellated,
            &screen,
        );

        let texture = self
            .render_state
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("Egui Texture"),
                size: wgpu::Extent3d {
                    width: screen.size_in_pixels[0],
                    height: screen.size_in_pixels[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.render_state.target_format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("Egui Render Pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &texture_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    ..Default::default()
                })
                .forget_lifetime();

            renderer.render(&mut pass, &tessellated, &screen);
        }

        self.render_state
            .queue
            .submit(std::iter::chain(user_buffers, once(encoder.finish())));

        self.render_state
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(WAIT_TIMEOUT),
            })
            .map_err(|err| format!("PollError: {err}"))?;

        Ok(texture_to_image(
            &self.render_state.device,
            &self.render_state.queue,
            &texture,
        ))
    }
}
