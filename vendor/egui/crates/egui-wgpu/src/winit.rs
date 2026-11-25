#![expect(clippy::missing_errors_doc)]
#![expect(clippy::undocumented_unsafe_blocks)]
#![expect(clippy::unwrap_used)] // TODO(emilk): avoid unwraps
#![expect(unsafe_code)]

use crate::{RenderState, SurfaceConfig, SurfaceErrorAction, WgpuConfiguration, renderer};
use crate::{
    RendererOptions,
    capture::{CaptureReceiver, CaptureSender, CaptureState, capture_channel},
};
use egui::{Context, Event, UserData, ViewportId, ViewportIdMap, ViewportIdSet};
use std::{collections::HashMap, num::NonZeroU32, sync::Arc};

struct SurfaceState {
    surface: wgpu::Surface<'static>,
    alpha_mode: wgpu::CompositeAlphaMode,
    width: u32,
    height: u32,
    resizing: bool,
    needs_reconfigure: bool,
    needs_recreate: bool,
}

#[derive(Clone)]
struct TextureSnapshot {
    image: epaint::ImageData,
    options: epaint::textures::TextureOptions,
}

impl TextureSnapshot {
    fn new(delta: &epaint::ImageDelta) -> Self {
        Self {
            image: delta.image.clone(),
            options: delta.options,
        }
    }

    /// Apply a texture delta to the CPU-side image that will rebuild the renderer.
    fn update(&mut self, delta: &epaint::ImageDelta) -> bool {
        let Some([x, y]) = delta.pos else {
            *self = Self::new(delta);
            return true;
        };

        let (epaint::ImageData::Color(dst), epaint::ImageData::Color(src)) =
            (&mut self.image, &delta.image);
        let dst = Arc::make_mut(dst);
        let src = src.as_ref();

        if x + src.size[0] > dst.size[0] || y + src.size[1] > dst.size[1] {
            log::error!(
                "Ignoring out-of-bounds texture update at ({x}, {y}) with size {:?} for {:?}",
                src.size,
                dst.size
            );
            return false;
        }

        let src_width = src.size[0];
        let dst_width = dst.size[0];
        for row in 0..src.size[1] {
            let src_start = row * src_width;
            let dst_start = (y + row) * dst_width + x;
            dst.pixels[dst_start..dst_start + src_width]
                .copy_from_slice(&src.pixels[src_start..src_start + src_width]);
        }
        self.options = delta.options;
        true
    }

    fn full_delta(&self) -> epaint::ImageDelta {
        epaint::ImageDelta::full(self.image.clone(), self.options)
    }
}

/// CPU-side copies of managed user textures uploaded to the renderer, plus the sampler
/// options for egui's built-in font atlas.
///
/// `epaint::TextureManager` keeps metadata only, and egui emits texture data just once.
/// Keeping the latest full image lets a suspended painter recreate its renderer without
/// losing any user images that are still in use. The font atlas (`TextureId::default()`)
/// already lives in `egui::Context`'s `FontsImpl`, so we only record its sampler options
/// and pull the live atlas image on replay instead of keeping a permanent second copy in RAM.
#[derive(Default)]
struct TextureSnapshots {
    font_atlas_options: Option<epaint::textures::TextureOptions>,
    textures: HashMap<epaint::TextureId, TextureSnapshot>,
}

impl TextureSnapshots {
    #[expect(clippy::iter_over_hash_type)]
    fn record(&mut self, delta: &epaint::textures::TexturesDelta) {
        let font_id = epaint::TextureId::default();
        for (id, image_deltas) in &delta.set {
            if *id == font_id {
                if let Some(last) = image_deltas.last() {
                    self.font_atlas_options = Some(last.options);
                }
                continue;
            }
            for image_delta in image_deltas {
                match self.textures.entry(*id) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().update(image_delta);
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        if image_delta.is_whole() {
                            entry.insert(TextureSnapshot::new(image_delta));
                        } else {
                            log::warn!(
                                "Cannot replay partial texture update for unallocated {id:?}"
                            );
                        }
                    }
                }
            }
        }

        for id in &delta.free {
            if *id == font_id {
                self.font_atlas_options = None;
            } else {
                self.textures.remove(id);
            }
        }
    }

    #[expect(clippy::iter_over_hash_type)]
    fn replay_into(&self, context: &Context, render_state: &RenderState) {
        if self.textures.is_empty() && self.font_atlas_options.is_none() {
            return;
        }

        // Callers replay right after creating the renderer, so it is always present here.
        let Some(renderer) = render_state.renderer() else {
            log::debug!("No renderer to replay managed textures into");
            return;
        };

        let font_delta = self.font_atlas_options.and_then(|options| {
            let image = context.fonts(|fonts| fonts.image());
            (image.width() > 0 && image.height() > 0)
                .then(|| epaint::ImageDelta::full(image, options))
        });

        let mut renderer = renderer.write();
        if let Some(font_delta) = font_delta {
            renderer.update_texture(
                &render_state.device,
                &render_state.queue,
                epaint::TextureId::default(),
                &font_delta,
            );
        }
        for (id, texture) in &self.textures {
            renderer.update_texture(
                &render_state.device,
                &render_state.queue,
                *id,
                &texture.full_delta(),
            );
        }
    }

    fn ids(&self) -> impl Iterator<Item = epaint::TextureId> + '_ {
        self.font_atlas_options
            .map(|_| epaint::TextureId::default())
            .into_iter()
            .chain(self.textures.keys().copied())
    }
}

/// Which viewports still owe their surface a first presented frame.
///
/// A surface holds whatever the swapchain last contained — nothing, on a fresh window — until
/// something is presented. Clearing it deliberately (`Painter::present_clear`) is what keeps a
/// just-shown or just-restored window from showing stale or uninitialised pixels while the
/// first egui pass is still being prepared, so the painter tracks which surfaces did that.
#[derive(Clone, Debug, Default)]
struct PrimerState {
    unprimed: ViewportIdSet,
}

impl PrimerState {
    /// A surface was installed or re-created for `viewport_id`: it needs a primer again.
    fn mark_new_surface(&mut self, viewport_id: ViewportId) {
        self.unprimed.insert(viewport_id);
    }

    /// `viewport_id` presented a frame, so its contents are defined.
    fn mark_presented(&mut self, viewport_id: ViewportId) {
        self.unprimed.remove(&viewport_id);
    }

    /// Every surface was dropped, so there is nothing left to prime.
    fn clear(&mut self) {
        self.unprimed.clear();
    }

    fn retain(&mut self, active_viewports: &ViewportIdSet) {
        self.unprimed.retain(|id| active_viewports.contains(id));
    }

    /// Whether `viewport_id`'s surface still owes its first presented frame.
    ///
    /// Per viewport, not for the painter as a whole: one window that has presented says
    /// nothing about another window that has not.
    fn needs_primer(&self, viewport_id: ViewportId) -> bool {
        self.unprimed.contains(&viewport_id)
    }
}

/// Where the time before the first presented frame went.
///
/// Reported as one line on the first present, so a slow start-up can be attributed to device
/// creation, surface configuration or pipeline compilation instead of guessed at.
struct StartupTimings {
    since: web_time::Instant,
    device_ms: Option<f32>,
    surface_ms: Option<f32>,
    renderer_ms: Option<f32>,
    logged: bool,
}

impl StartupTimings {
    fn new(since: web_time::Instant) -> Self {
        Self {
            since,
            device_ms: None,
            surface_ms: None,
            renderer_ms: None,
            logged: false,
        }
    }

    fn ms(start: web_time::Instant) -> f32 {
        start.elapsed().as_secs_f32() * 1000.0
    }

    /// Only the first device creation is on the start-up path; re-creations after
    /// `Painter::suspend` are not, so they never overwrite the measurement.
    fn record_device(&mut self, start: web_time::Instant) {
        if self.device_ms.is_none() {
            self.device_ms = Some(Self::ms(start));
        }
    }

    fn record_surface(&mut self, start: web_time::Instant) {
        if self.surface_ms.is_none() {
            self.surface_ms = Some(Self::ms(start));
        }
    }

    fn record_renderer(&mut self, start: web_time::Instant) {
        if self.renderer_ms.is_none() {
            self.renderer_ms = Some(Self::ms(start));
        }
    }

    fn message(&self, total_ms: f32) -> String {
        fn phase(ms: Option<f32>) -> String {
            ms.map_or_else(|| "n/a".to_owned(), |ms| format!("{ms:.1}"))
        }
        format!(
            "wgpu first frame presented {total_ms:.1} ms after painter creation: \
             device {} ms, surface {} ms, pipelines {} ms",
            phase(self.device_ms),
            phase(self.surface_ms),
            phase(self.renderer_ms),
        )
    }

    /// The start-up line, the first time a frame reaches the screen.
    fn first_present_message(&mut self) -> Option<String> {
        if self.logged {
            return None;
        }
        self.logged = true;
        let total_ms = self.since.elapsed().as_secs_f32() * 1000.0;
        Some(self.message(total_ms))
    }

    /// The pipeline phase on its own, when it lands after the first presented frame.
    ///
    /// The start-up line is emitted at the first present, which on the deferred path happens
    /// before the pipelines exist; without this the pipeline cost would never be reported, and
    /// that cost is exactly what this deferral took off the window's reveal path.
    fn late_renderer_message(&self) -> Option<String> {
        if !self.logged {
            return None;
        }
        let renderer_ms = self.renderer_ms?;
        let total_ms = self.since.elapsed().as_secs_f32() * 1000.0;
        Some(format!(
            "wgpu pipelines compiled {renderer_ms:.1} ms, {total_ms:.1} ms after painter creation"
        ))
    }
}

/// Report the one case where the capability answer and what was built disagree.
///
/// A device that advertises dual-source blending lets a font atlas rasterize LCD coverage; if
/// no pipeline came out of it, those texels reach the ordinary pipeline as additive
/// per-channel coverage while their alpha is zero, so text is corrupted instead of degraded.
/// That must never be silent.
fn report_missing_lcd_pipeline(render_state: &RenderState, has_lcd_pipeline: bool) {
    if has_lcd_pipeline || !crate::Renderer::advertises_subpixel_text(&render_state.device) {
        return;
    }
    log::error!(
        "The device advertises dual-source blending but the LCD text pipeline was not created; \
         sub-pixel text cannot be drawn (see the warnings above for the shader or pipeline error)"
    );
}

/// Everything you need to paint egui with [`wgpu`] on [`winit`].
///
/// Alternatively you can use [`crate::Renderer`] directly.
///
/// NOTE: all egui viewports share the same painter.
pub struct Painter {
    context: Context,
    config: WgpuConfiguration,
    options: RendererOptions,
    support_transparent_backbuffer: bool,
    screen_capture_state: Option<CaptureState>,

    instance: wgpu::Instance,
    render_state: Option<RenderState>,
    texture_snapshots: TextureSnapshots,
    max_texture_side: Option<usize>,
    subpixel_text: Option<bool>,
    primer: PrimerState,
    startup: StartupTimings,

    // Per viewport/window:
    depth_texture_view: ViewportIdMap<wgpu::TextureView>,
    msaa_texture_view: ViewportIdMap<wgpu::TextureView>,
    surfaces: ViewportIdMap<SurfaceState>,
    capture_tx: CaptureSender,
    capture_rx: CaptureReceiver,
}

impl Painter {
    /// Manages [`wgpu`] state, including surface state, required to render egui.
    ///
    /// Only the [`wgpu::Instance`] is initialized here. Device selection and the initialization
    /// of render + surface state is deferred until the painter is given its first window target
    /// via [`set_window()`](Self::set_window). (Ensuring that a device that's compatible with the
    /// native window is chosen)
    ///
    /// Before calling [`paint_and_update_textures()`](Self::paint_and_update_textures) a
    /// [`wgpu::Surface`] must be initialized (and corresponding render state) by calling
    /// [`set_window()`](Self::set_window) once you have
    /// a [`winit::window::Window`] with a valid `.raw_window_handle()`
    /// associated.
    pub async fn new(
        context: Context,
        config: WgpuConfiguration,
        support_transparent_backbuffer: bool,
        options: RendererOptions,
    ) -> Self {
        let created_at = web_time::Instant::now();
        let (capture_tx, capture_rx) = capture_channel();
        let instance = config.wgpu_setup.new_instance().await;

        Self {
            context,
            config,
            options,
            support_transparent_backbuffer,
            screen_capture_state: None,

            instance,
            render_state: None,
            texture_snapshots: Default::default(),
            max_texture_side: None,
            subpixel_text: None,
            primer: PrimerState::default(),
            startup: StartupTimings::new(created_at),

            depth_texture_view: Default::default(),
            surfaces: Default::default(),
            msaa_texture_view: Default::default(),

            capture_tx,
            capture_rx,
        }
    }

    /// Get the [`RenderState`].
    ///
    /// Will return [`None`] if the render state has not been initialized yet.
    pub fn render_state(&self) -> Option<RenderState> {
        self.render_state.clone()
    }

    fn configure_surface(
        surface_state: &SurfaceState,
        render_state: &RenderState,
        config: &SurfaceConfig,
    ) {
        profiling::function_scope!();

        let SurfaceConfig {
            present_mode,
            desired_maximum_frame_latency,
        } = *config;

        // Transaction presentation can hold a drawable during AppKit live resize. Keep the
        // configured low-latency path normally, but use three Metal drawables while resizing.
        #[cfg(all(target_os = "macos", feature = "macos-window-resize-jitter-fix"))]
        let desired_maximum_frame_latency = if surface_state.resizing {
            Some(desired_maximum_frame_latency.unwrap_or(2).max(2))
        } else {
            desired_maximum_frame_latency
        };

        let width = surface_state.width;
        let height = surface_state.height;
        if width == 0 || height == 0 {
            return;
        }

        let Some(default_config) =
            surface_state
                .surface
                .get_default_config(&render_state.adapter, width, height)
        else {
            log::warn!(
                "The surface isn't supported by this adapter or has invalid size ({width}x{height})"
            );
            return;
        };

        let mut surf_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: render_state.target_format,
            present_mode,
            alpha_mode: surface_state.alpha_mode,
            view_formats: vec![render_state.target_format],
            ..default_config
        };

        if let Some(desired_maximum_frame_latency) = desired_maximum_frame_latency {
            surf_config.desired_maximum_frame_latency = desired_maximum_frame_latency;
        }

        surface_state
            .surface
            .configure(&render_state.device, &surf_config);
    }

    /// Replace the [`wgpu::Surface`] for `viewport_id` with a fresh one for the given window
    /// via [`wgpu::Instance::create_surface`], then configure it.
    ///
    /// Used to recover from [`wgpu::CurrentSurfaceTexture::Lost`], where reconfiguring the
    /// existing surface object cannot recover.
    ///
    /// The replacement is created *before* the old surface is dropped: if surface creation
    /// itself fails (device removed, adapter gone, output destroyed), the viewport keeps its
    /// previous entry, so the retry the caller schedules still has a state to replace and the
    /// window is not left without one.
    fn recreate_surface(
        &mut self,
        viewport_id: ViewportId,
        window: &Arc<winit::window::Window>,
    ) -> Result<(), crate::WgpuError> {
        profiling::function_scope!();

        let Some(old_state) = self.surfaces.get(&viewport_id) else {
            return Ok(());
        };
        let (width, height, resizing) = (old_state.width, old_state.height, old_state.resizing);

        let surface = self.instance.create_surface(Arc::clone(window))?;
        self.install_surface(surface, viewport_id, width, height, resizing);
        Ok(())
    }

    /// Updates (or clears) the [`winit::window::Window`] associated with the [`Painter`]
    ///
    /// This creates a [`wgpu::Surface`] for the given Window (as well as initializing render
    /// state if needed) that is used for egui rendering.
    ///
    /// This must be called before trying to render via
    /// [`paint_and_update_textures`](Self::paint_and_update_textures)
    ///
    /// # Portability
    ///
    /// _In particular it's important to note that on Android a it's only possible to create
    /// a window surface between `Resumed` and `Paused` lifecycle events, and Winit will panic on
    /// attempts to query the raw window handle while paused._
    ///
    /// On Android [`set_window`](Self::set_window) should be called with `Some(window)` for each
    /// `Resumed` event and `None` for each `Paused` event. Currently, on all other platforms
    /// [`set_window`](Self::set_window) may be called with `Some(window)` as soon as you have a
    /// valid [`winit::window::Window`].
    ///
    /// # Errors
    /// If the provided wgpu configuration does not match an available device.
    pub async fn set_window(
        &mut self,
        viewport_id: ViewportId,
        window: Option<Arc<winit::window::Window>>,
    ) -> Result<(), crate::WgpuError> {
        profiling::scope!("Painter::set_window"); // profile_function gives bad names for async functions

        if let Some(window) = window {
            let size = window.inner_size();
            if !self.surfaces.contains_key(&viewport_id) {
                let surface = self.instance.create_surface(window)?;
                self.add_surface(surface, viewport_id, size).await?;
            }
        } else {
            log::warn!("No window - clearing all surfaces");
            self.surfaces.clear();
        }
        Ok(())
    }

    /// Updates (or clears) the [`winit::window::Window`] associated with the [`Painter`] without taking ownership of the window.
    ///
    /// Like [`set_window`](Self::set_window) except:
    ///
    /// # Safety
    /// The user is responsible for ensuring that the window is alive for as long as it is set.
    pub async unsafe fn set_window_unsafe(
        &mut self,
        viewport_id: ViewportId,
        window: Option<&winit::window::Window>,
    ) -> Result<(), crate::WgpuError> {
        profiling::scope!("Painter::set_window_unsafe"); // profile_function gives bad names for async functions

        if let Some(window) = window {
            let size = window.inner_size();
            if !self.surfaces.contains_key(&viewport_id) {
                let surface = unsafe {
                    self.instance
                        .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::from_window(&window)?)?
                };
                self.add_surface(surface, viewport_id, size).await?;
            }
        } else {
            log::warn!("No window - clearing all surfaces");
            self.surfaces.clear();
        }
        Ok(())
    }

    async fn add_surface(
        &mut self,
        surface: wgpu::Surface<'static>,
        viewport_id: ViewportId,
        size: winit::dpi::PhysicalSize<u32>,
    ) -> Result<(), crate::WgpuError> {
        if self.render_state.is_none() {
            // Pipelines are compiled by `ensure_renderer` on the first drawn frame instead of
            // here, so this window can present a clear colour before they exist.
            let device_start = web_time::Instant::now();
            let render_state =
                RenderState::create_without_renderer(&self.config, &self.instance, Some(&surface))
                    .await?;
            self.startup.record_device(device_start);
            self.max_texture_side =
                Some(render_state.device.limits().max_texture_dimension_2d as usize);
            self.render_state = Some(render_state);
        }
        self.install_surface(surface, viewport_id, size.width, size.height, false);
        Ok(())
    }

    /// Compile the renderer's pipelines if that has not happened yet.
    ///
    /// Renderer creation is deferred off the window's critical path: [`Self::set_window`] only
    /// decides the device and the surface format, so a window can be shown and cleared before
    /// any pipeline is built. Every paint path calls this, so an unprimed painter still draws,
    /// and managed textures captured while the renderer was absent are replayed once it exists.
    ///
    /// Idempotent; the second and later calls cost a dead check.
    pub fn ensure_renderer(&mut self) {
        profiling::function_scope!();

        let Some(render_state) = self.render_state.as_ref() else {
            return;
        };
        if let Some(renderer) = render_state.renderer() {
            // The renderer slot is shared with every clone of the render state, so an
            // application that compiled the pipelines through a clone would otherwise leave
            // this painter without a capability answer and without its snapshots uploaded.
            if self.subpixel_text.is_none() {
                let has_lcd_pipeline = renderer.read().has_subpixel_pipeline();
                self.subpixel_text = Some(has_lcd_pipeline);
                report_missing_lcd_pipeline(render_state, has_lcd_pipeline);
                self.texture_snapshots.replay_into(&self.context, render_state);
            }
            return;
        }

        let renderer_start = web_time::Instant::now();
        let renderer = render_state.ensure_renderer(self.options);
        self.startup.record_renderer(renderer_start);
        if let Some(message) = self.startup.late_renderer_message() {
            log::info!("{message}");
        }

        let has_lcd_pipeline = renderer.read().has_subpixel_pipeline();
        self.subpixel_text = Some(has_lcd_pipeline);
        report_missing_lcd_pipeline(render_state, has_lcd_pipeline);
        self.texture_snapshots.replay_into(&self.context, render_state);
    }

    /// Inserts a freshly created surface into [`Self::surfaces`] and configures it.
    ///
    /// Render state must already be initialised before calling this.
    // NOTE: The same assumption is already required by `resize_and_generate_depth_texture_view_and_msaa_view`.
    fn install_surface(
        &mut self,
        surface: wgpu::Surface<'static>,
        viewport_id: ViewportId,
        width: u32,
        height: u32,
        resizing: bool,
    ) {
        let alpha_mode = {
            // Panic: We use the same failure mode as `resize_and_generate_depth_texture_view_and_msaa_view`
            let render_state = self
                .render_state
                .as_ref()
                .expect("install_surface called before render_state initialization");
            if self.support_transparent_backbuffer {
                let supported_alpha_modes =
                    surface.get_capabilities(&render_state.adapter).alpha_modes;
                // Prefer pre multiplied over post multiplied!
                if supported_alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
                    wgpu::CompositeAlphaMode::PreMultiplied
                } else if supported_alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied)
                {
                    wgpu::CompositeAlphaMode::PostMultiplied
                } else {
                    log::warn!(
                        "Transparent window was requested, but the active wgpu surface does not support a `CompositeAlphaMode` with transparency."
                    );
                    wgpu::CompositeAlphaMode::Auto
                }
            } else {
                wgpu::CompositeAlphaMode::Auto
            }
        };
        self.surfaces.insert(
            viewport_id,
            SurfaceState {
                surface,
                width,
                height,
                alpha_mode,
                resizing,
                needs_reconfigure: false,
                needs_recreate: false,
            },
        );
        // A fresh surface holds whatever the swapchain last contained, so its first frame has
        // to define every pixel before anything else is drawn into it.
        self.primer.mark_new_surface(viewport_id);
        let Some(width) = NonZeroU32::new(width) else {
            log::debug!("The window width was zero; skipping generate textures");
            return;
        };
        let Some(height) = NonZeroU32::new(height) else {
            log::debug!("The window height was zero; skipping generate textures");
            return;
        };
        self.resize_and_generate_depth_texture_view_and_msaa_view(viewport_id, width, height);
    }

    /// Returns the maximum texture dimension supported by the most recently initialized
    /// device, if one has been created.
    ///
    /// This remains available after [`Self::suspend()`] so window systems that need the
    /// capability while creating another window do not have to keep the GPU resident.
    pub fn max_texture_side(&self) -> Option<usize> {
        self.max_texture_side
    }

    /// Whether this painter created the managed-font dual-source pipeline.
    ///
    /// `None` means no device has been initialized yet, or the renderer's pipelines have not
    /// been compiled yet ([`Self::ensure_renderer`]). The value remains available while paused
    /// or suspended and reports the actual feature of that last device, not just what the
    /// selected adapter advertised.
    pub fn subpixel_text(&self) -> Option<bool> {
        self.subpixel_text
    }

    /// Whether `viewport_id`'s surface still owes its first presented frame.
    ///
    /// True from the moment [`Self::set_window`] (or a surface re-create) installs that
    /// viewport's surface until it has presented once — either through
    /// [`Self::present_clear`] or through [`Self::paint_and_update_textures`]. A caller uses it
    /// to put the correct background colour on screen while the full first egui pass is still
    /// being prepared, instead of showing an unpainted backbuffer.
    ///
    /// Per viewport: a second window that has not presented yet does not make the first one
    /// ask for another primer.
    pub fn needs_primer(&self, viewport_id: ViewportId) -> bool {
        self.primer.needs_primer(viewport_id)
    }

    /// Drop every per-surface object, including which surfaces still owe a primer.
    ///
    /// The primer requirement belongs to the surface: once the surface is gone there is
    /// nothing left whose pixels are undefined, and the re-created surface is marked again by
    /// [`Self::install_surface`]. Keeping both in one place is what stops one teardown path
    /// from clearing the surfaces and leaving a stale primer request behind.
    fn drop_all_surfaces(&mut self) {
        self.surfaces.clear();
        self.depth_texture_view.clear();
        self.msaa_texture_view.clear();
        self.screen_capture_state = None;
        self.primer.clear();
    }

    /// Release the swapchain while keeping the device and its pipelines resident.
    ///
    /// The surfaces, their depth and MSAA views and any capture target are dropped; the
    /// adapter, device, queue, renderer (pipelines), CPU-side texture snapshots,
    /// `max_texture_side` and `subpixel_text` are kept. Restoring therefore
    /// costs a surface re-create and a configure instead of a device initialisation plus a
    /// pipeline compile plus a full font-atlas re-upload, which is what made the window blank
    /// for the whole re-init after a minimize.
    ///
    /// This deliberately does not wait for the GPU. The blocking
    /// `poll(PollType::Wait { .. })` in [`Self::suspend`] is bounded at 250 ms and would be
    /// spent on the minimize path, exactly when the window is going away; a non-blocking poll
    /// lets wgpu retire submissions that already finished and release the surface's
    /// backbuffers without waiting for a frame that is still in flight.
    pub fn pause(&mut self) {
        self.drop_all_surfaces();

        let Some(render_state) = self.render_state.as_ref() else {
            return;
        };
        if let Err(err) = render_state.device.poll(wgpu::PollType::Poll) {
            log::debug!("WGPU poll while pausing failed: {err}");
        }
    }

    /// Release all resident GPU objects while keeping the painter reusable.
    ///
    /// This is the full teardown: device, renderer, surfaces and capture state all go away.
    /// It is correct after [`Self::pause`] (which only dropped the surfaces), and the next call
    /// to [`Self::set_window()`] recreates the device, renderer and surface lazily. Managed
    /// textures are replayed from CPU-side snapshots so egui UI state and the font atlas
    /// survive the interruption. Capabilities needed to create other windows, such as
    /// [`Self::max_texture_side()`], are retained.
    ///
    /// Prefer [`Self::pause`] when the window is coming back: keeping the device and pipelines
    /// is what makes minimize/restore instant.
    pub fn suspend(&mut self) {
        self.drop_all_surfaces();

        let Some(render_state) = self.render_state.take() else {
            return;
        };

        // Finish submitted work before explicitly destroying textures. The timeout prevents
        // a wedged GPU from blocking hide/minimize indefinitely; wgpu still defers the
        // destruction safely when a submission takes longer than this.
        if let Err(err) = render_state.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_millis(250)),
        }) {
            log::debug!("WGPU did not become idle while suspending: {err}");
        }

        if let Some(renderer) = render_state.renderer() {
            let mut renderer = renderer.write();
            for id in self.texture_snapshots.ids() {
                renderer.free_texture(&id);
            }
        }

        drop(render_state);
    }

    /// Present one cleared frame, without the renderer.
    ///
    /// [`Self::set_window`] leaves the painter with a usable device and surface but an
    /// unprimed renderer, so this is the cheapest way to define every pixel of a freshly
    /// shown or freshly restored window: one load-op clear, one submit, one present, and no
    /// pipelines, font atlas or app textures involved. The next
    /// [`Self::paint_and_update_textures`] covers the clear with the real frame.
    ///
    /// Marks the viewport as primed (see [`Self::needs_primer`]) and reports the one start-up
    /// timing line on the first present. Correct on the first frame and after a full surface
    /// re-create; a zero-sized surface presents nothing and returns `Ok`.
    ///
    /// # Errors
    /// [`crate::WgpuError::NoRenderState`] before [`Self::set_window`] created a device, and
    /// [`crate::WgpuError::NoSurface`] when the viewport has no surface.
    pub fn present_clear(
        &mut self,
        viewport_id: ViewportId,
        clear_color: [f32; 4],
        window: &Arc<winit::window::Window>,
    ) -> Result<(), crate::WgpuError> {
        profiling::function_scope!();

        // If the previous frame produced `CurrentSurfaceTexture::Lost`, the action taken in
        // `paint_and_update_textures` set `needs_recreate`; recreate before re-borrowing the
        // render state and the surfaces below.
        if self
            .surfaces
            .get(&viewport_id)
            .is_some_and(|state| state.needs_recreate)
            && let Err(err) = self.recreate_surface(viewport_id, window)
        {
            log::error!("Failed to recreate surface for {viewport_id:?}: {err}");
            return Err(err);
        }

        let Some(render_state) = self.render_state.as_ref() else {
            return Err(crate::WgpuError::NoRenderState);
        };

        // Apply any runtime changes requested via `RenderState::surface_config`, exactly as
        // `paint_and_update_textures` does.
        if render_state.surface_config != self.config.surface {
            self.config.surface = render_state.surface_config;
            #[expect(clippy::iter_over_hash_type)]
            for surface in self.surfaces.values_mut() {
                surface.needs_reconfigure = true;
            }
        }
        let surface_config = self.config.surface;

        let Some(surface_state) = self.surfaces.get_mut(&viewport_id) else {
            return Err(crate::WgpuError::NoSurface);
        };
        if surface_state.width == 0 || surface_state.height == 0 {
            return Ok(());
        }

        if surface_state.needs_reconfigure {
            Self::configure_surface(surface_state, render_state, &surface_config);
            surface_state.needs_reconfigure = false;
        }

        let output_frame = match surface_state.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                surface_state.needs_reconfigure = true;
                frame
            }
            other => {
                match (*self.config.on_surface_status)(&other) {
                    SurfaceErrorAction::Reconfigure => {
                        Self::configure_surface(surface_state, render_state, &surface_config);
                        self.context.request_repaint_of(viewport_id);
                    }
                    SurfaceErrorAction::RecreateSurface => {
                        // Ownership keeps the recreation out of this frame; the next paint
                        // consumes `needs_recreate` at the top.
                        surface_state.needs_recreate = true;
                        self.context.request_repaint_of(viewport_id);
                    }
                    SurfaceErrorAction::SkipFrame => {}
                }
                return Ok(());
            }
        };

        let mut encoder =
            render_state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("egui_clear_encoder"),
                });
        let target_view = output_frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        {
            profiling::scope!("clear_render_pass");
            // No MSAA and no depth attachment: nothing is drawn, so there is nothing to
            // resolve or depth-test, and the next paint clears again anyway.
            let _render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui_clear_render"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear_color[0] as f64,
                            g: clear_color[1] as f64,
                            b: clear_color[2] as f64,
                            a: clear_color[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }

        {
            profiling::scope!("Queue::submit");
            render_state.queue.submit([encoder.finish()]);
        }

        window.pre_present_notify();

        {
            profiling::scope!("present");
            render_state.queue.present(output_frame);
        }

        self.primer.mark_presented(viewport_id);
        if let Some(message) = self.startup.first_present_message() {
            log::info!("{message}");
        }

        Ok(())
    }

    fn resize_and_generate_depth_texture_view_and_msaa_view(
        &mut self,
        viewport_id: ViewportId,
        width_in_pixels: NonZeroU32,
        height_in_pixels: NonZeroU32,
    ) {
        profiling::function_scope!();

        let width = width_in_pixels.get();
        let height = height_in_pixels.get();

        let render_state = self.render_state.as_ref().unwrap();
        let surface_state = self.surfaces.get_mut(&viewport_id).unwrap();

        surface_state.width = width;
        surface_state.height = height;

        let configure_start = web_time::Instant::now();
        Self::configure_surface(surface_state, render_state, &self.config.surface);
        self.startup.record_surface(configure_start);

        if let Some(depth_format) = self.options.depth_stencil_format {
            self.depth_texture_view.insert(
                viewport_id,
                render_state
                    .device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: Some("egui_depth_texture"),
                        size: wgpu::Extent3d {
                            width,
                            height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: self.options.msaa_samples.max(1),
                        dimension: wgpu::TextureDimension::D2,
                        format: depth_format,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[depth_format],
                    })
                    .create_view(&wgpu::TextureViewDescriptor::default()),
            );
        }

        if let Some(render_state) = (self.options.msaa_samples > 1)
            .then_some(self.render_state.as_ref())
            .flatten()
        {
            let texture_format = render_state.target_format;
            self.msaa_texture_view.insert(
                viewport_id,
                render_state
                    .device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: Some("egui_msaa_texture"),
                        size: wgpu::Extent3d {
                            width,
                            height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: self.options.msaa_samples.max(1),
                        dimension: wgpu::TextureDimension::D2,
                        format: texture_format,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        view_formats: &[texture_format],
                    })
                    .create_view(&wgpu::TextureViewDescriptor::default()),
            );
        }
    }

    /// Handles changes of the resizing state.
    ///
    /// Should be called prior to the first [`Painter::on_window_resized`] call and after the last in
    /// the chain. Used to apply platform-specific logic, e.g. OSX Metal window resize jitter fix.
    pub fn on_window_resize_state_change(&mut self, viewport_id: ViewportId, resizing: bool) {
        profiling::function_scope!();

        let Some(state) = self.surfaces.get_mut(&viewport_id) else {
            return;
        };
        if state.resizing == resizing {
            if resizing {
                log::debug!(
                    "Painter::on_window_resize_state_change() redundant call while resizing"
                );
            } else {
                log::debug!(
                    "Painter::on_window_resize_state_change() redundant call after resizing"
                );
            }
            return;
        }

        // Set before reconfiguring so macOS live resize uses the temporary latency bump above.
        state.resizing = resizing;

        // Resizing is a bit tricky on macOS.
        // It requires enabling ["present_with_transaction"](https://developer.apple.com/documentation/quartzcore/cametallayer/presentswithtransaction)
        // flag to avoid jittering during the resize. Even though resize jittering on macOS
        // is common across rendering backends, the solution for wgpu/metal is known.
        //
        // See https://github.com/emilk/egui/issues/903
        #[cfg(all(target_os = "macos", feature = "macos-window-resize-jitter-fix"))]
        {
            // SAFETY: `as_hal::<Metal>()` returns `None` unless this surface is backed by wgpu's
            // Metal backend.
            unsafe {
                if let (Some(render_state), Some(hal_surface)) = (
                    self.render_state.as_ref(),
                    state.surface.as_hal::<wgpu::hal::api::Metal>(),
                ) {
                    hal_surface
                        .render_layer()
                        .lock()
                        .setPresentsWithTransaction(resizing);

                    Self::configure_surface(state, render_state, &self.config.surface);
                }
            }
        }
    }

    pub fn on_window_resized(
        &mut self,
        viewport_id: ViewportId,
        width_in_pixels: NonZeroU32,
        height_in_pixels: NonZeroU32,
    ) {
        profiling::function_scope!();

        if self.surfaces.contains_key(&viewport_id) {
            self.resize_and_generate_depth_texture_view_and_msaa_view(
                viewport_id,
                width_in_pixels,
                height_in_pixels,
            );
        } else {
            log::debug!(
                "Ignoring window resize notification with no surface created via Painter::set_window()"
            );
        }
    }

    /// Returns two things:
    ///
    /// The approximate number of seconds spent on vsync-waiting (if any),
    /// and the captures captured screenshot if it was requested.
    ///
    /// If `capture_data` isn't empty, a screenshot will be captured.
    #[expect(clippy::too_many_arguments)]
    pub fn paint_and_update_textures(
        &mut self,
        viewport_id: ViewportId,
        pixels_per_point: f32,
        clear_color: [f32; 4],
        clipped_primitives: &[epaint::ClippedPrimitive],
        textures_delta: &mut epaint::textures::TexturesDelta,
        capture_data: Vec<UserData>,
        window: &Arc<winit::window::Window>,
    ) -> f32 {
        profiling::function_scope!();

        /// Guard to ensure that commands are always submitted to the renderer queue
        /// so that calls to [`write_buffer()`](https://docs.rs/wgpu/latest/wgpu/struct.Queue.html#method.write_buffer)
        /// are completed even if we take a codepath which doesn't submit commands and avoids
        /// internal buffers growing indefinitely.
        ///
        /// This may happen, for example, if no output frame is resolved.
        /// See <https://github.com/emilk/egui/pull/7928> for full context.
        struct RendererQueueGuard<'q> {
            queue: &'q wgpu::Queue,
            commands_submitted: bool,
        }

        impl Drop for RendererQueueGuard<'_> {
            fn drop(&mut self) {
                // Only submit an empty command buffer array if no commands were
                // explicitly submitted.
                if !self.commands_submitted {
                    self.queue.submit([]);
                }
            }
        }

        let capture = !capture_data.is_empty();
        let mut vsync_sec = 0.0;

        // If the previous frame produced `CurrentSurfaceTexture::Lost`, the action match
        // below set `needs_recreate`. Recreate the surface now, before re-borrowing
        // `self.render_state` / `self.surfaces` for the rest of the paint.
        if self
            .surfaces
            .get(&viewport_id)
            .is_some_and(|s| s.needs_recreate)
            && let Err(err) = self.recreate_surface(viewport_id, window)
        {
            log::error!("Failed to recreate surface for {viewport_id:?}: {err}");
            self.context
                .request_repaint_after_for(std::time::Duration::from_millis(100), viewport_id);
            return vsync_sec;
        }

        // Apply any runtime changes requested via `RenderState::surface_config`.
        // We diff against the already-applied values in `self.config.surface`
        // and, if anything differs, mark every surface as needing reconfiguration so
        // the existing `needs_reconfigure` pathway below picks them up.
        if let Some(render_state) = self.render_state.as_ref()
            && render_state.surface_config != self.config.surface
        {
            self.config.surface = render_state.surface_config;
            #[expect(clippy::iter_over_hash_type)]
            for surface in self.surfaces.values_mut() {
                surface.needs_reconfigure = true;
            }
        }

        // A painter that presented only the clear colour, or whose renderer was dropped with
        // its render state, still has to draw: compile the pipelines (and replay the managed
        // textures recorded so far) before this frame's deltas are recorded and applied.
        self.ensure_renderer();

        // egui emits each texture change once. Record the resulting full images before
        // draining the deltas so a later renderer recreation can restore them.
        self.texture_snapshots.record(textures_delta);

        let Some(render_state) = self.render_state.as_mut() else {
            return vsync_sec;
        };

        // Idempotent: this is the renderer created by `ensure_renderer` above, and the `Arc`
        // clone keeps it usable while the rest of the state stays borrowed.
        let renderer = render_state.ensure_renderer(self.options);

        let mut render_queue_guard = RendererQueueGuard {
            queue: &render_state.queue,
            commands_submitted: false,
        };

        let Some(surface_state) = self.surfaces.get_mut(&viewport_id) else {
            return vsync_sec;
        };
        if surface_state.width == 0 || surface_state.height == 0 {
            return vsync_sec;
        }

        let mut encoder =
            render_state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("encoder"),
                });

        // Upload all resources for the GPU.
        let screen_descriptor = renderer::ScreenDescriptor {
            size_in_pixels: [surface_state.width, surface_state.height],
            pixels_per_point,
        };

        let subpixel_text_options = self.context.fonts(|fonts| {
            let options = fonts.options();
            (options.subpixel, options.text_gamma, options.text_contrast)
        });

        let user_cmd_bufs = {
            let mut renderer = renderer.write();
            renderer.set_subpixel_text_options(
                subpixel_text_options.0,
                subpixel_text_options.1,
                subpixel_text_options.2,
            );
            #[expect(clippy::iter_over_hash_type)] // Order doesn't matter here
            for (id, image_deltas) in textures_delta.set.drain() {
                for image_delta in image_deltas {
                    renderer.update_texture(
                        &render_state.device,
                        &render_state.queue,
                        id,
                        &image_delta,
                    );
                }
            }

            renderer.update_buffers(
                &render_state.device,
                &render_state.queue,
                &mut encoder,
                clipped_primitives,
                &screen_descriptor,
            )
        };

        if surface_state.needs_reconfigure {
            Self::configure_surface(surface_state, render_state, &self.config.surface);
            surface_state.needs_reconfigure = false;
        }

        let output_frame = {
            profiling::scope!("get_current_texture");
            // This is what vsync-waiting happens on my Mac.
            let start = web_time::Instant::now();
            let output_frame = surface_state.surface.get_current_texture();
            vsync_sec += start.elapsed().as_secs_f32();
            output_frame
        };

        let output_frame = match output_frame {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                surface_state.needs_reconfigure = true;
                frame
            }
            other => {
                match (*self.config.on_surface_status)(&other) {
                    SurfaceErrorAction::Reconfigure => {
                        Self::configure_surface(surface_state, render_state, &self.config.surface);
                        self.context.request_repaint_of(viewport_id);
                    }
                    SurfaceErrorAction::RecreateSurface => {
                        // Because of ownership, I could not find an easy way to do a full recovery here,
                        // as that would involve dropping the old surface and creating a new one.
                        // For now, we defer the recreation to the beginning of the next frame (which
                        // we ensure to arrive via `request_repaint_of`). A cleaner solution would be
                        // to untangle the ownership of `RenderState`.
                        surface_state.needs_recreate = true;
                        self.context.request_repaint_of(viewport_id);
                    }
                    SurfaceErrorAction::SkipFrame => {}
                }
                return vsync_sec;
            }
        };

        let mut capture_buffer = None;
        {
            let renderer = renderer.read();

            let target_texture = if capture {
                let capture_state = self.screen_capture_state.get_or_insert_with(|| {
                    CaptureState::new(&render_state.device, &output_frame.texture)
                });
                capture_state.update(&render_state.device, &output_frame.texture);

                &capture_state.texture
            } else {
                &output_frame.texture
            };
            let target_view = target_texture.create_view(&wgpu::TextureViewDescriptor::default());

            let (view, resolve_target) = (self.options.msaa_samples > 1)
                .then_some(self.msaa_texture_view.get(&viewport_id))
                .flatten()
                .map_or((&target_view, None), |texture_view| {
                    (texture_view, Some(&target_view))
                });

            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui_render"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear_color[0] as f64,
                            g: clear_color[1] as f64,
                            b: clear_color[2] as f64,
                            a: clear_color[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: self.depth_texture_view.get(&viewport_id).map(|view| {
                    wgpu::RenderPassDepthStencilAttachment {
                        view,
                        depth_ops: self
                            .options
                            .depth_stencil_format
                            .is_some_and(|depth_stencil_format| {
                                depth_stencil_format.has_depth_aspect()
                            })
                            .then_some(wgpu::Operations {
                                load: wgpu::LoadOp::Clear(1.0),
                                // It is very unlikely that the depth buffer is needed after egui finished rendering
                                // so no need to store it. (this can improve performance on tiling GPUs like mobile chips or Apple Silicon)
                                store: wgpu::StoreOp::Discard,
                            }),
                        stencil_ops: self
                            .options
                            .depth_stencil_format
                            .is_some_and(|depth_stencil_format| {
                                depth_stencil_format.has_stencil_aspect()
                            })
                            .then_some(wgpu::Operations {
                                load: wgpu::LoadOp::Clear(0),
                                store: wgpu::StoreOp::Discard,
                            }),
                    }
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // Forgetting the pass' lifetime means that we are no longer compile-time protected from
            // runtime errors caused by accessing the parent encoder before the render pass is dropped.
            // Since we don't pass it on to the renderer, we should be perfectly safe against this mistake here!
            renderer.render(
                &mut render_pass.forget_lifetime(),
                clipped_primitives,
                &screen_descriptor,
            );

            if capture && let Some(capture_state) = &mut self.screen_capture_state {
                capture_buffer = Some(capture_state.copy_textures(
                    &render_state.device,
                    &output_frame,
                    &mut encoder,
                ));
            }
        }

        let encoded = {
            profiling::scope!("CommandEncoder::finish");
            encoder.finish()
        };

        // Submit the commands: both the main buffer and user-defined ones.
        {
            profiling::scope!("Queue::submit");
            // wgpu doesn't document where vsync can happen. Maybe here?
            let start = web_time::Instant::now();
            render_state
                .queue
                .submit(std::iter::chain(user_cmd_bufs, [encoded]));
            vsync_sec += start.elapsed().as_secs_f32();
        };

        // Ensure that the queue guard does not do unnecessary work when dropped
        render_queue_guard.commands_submitted = true;

        // Free textures marked for destruction **after** queue submit since they might still be used in the current frame.
        // Calling `wgpu::Texture::destroy` on a texture that is still in use would invalidate the command buffer(s) it is used in.
        // However, once we called `wgpu::Queue::submit`, it is up for wgpu to determine how long the underlying gpu resource has to live.
        {
            let mut renderer = renderer.write();
            #[expect(clippy::iter_over_hash_type)] // Order doesn't matter here
            for id in textures_delta.free.drain() {
                renderer.free_texture(&id);
            }
        }

        if let Some(capture_buffer) = capture_buffer
            && let Some(screen_capture_state) = &mut self.screen_capture_state
        {
            screen_capture_state.read_screen_rgba(
                self.context.clone(),
                capture_buffer,
                capture_data,
                self.capture_tx.clone(),
                viewport_id,
            );
        }

        window.pre_present_notify();

        {
            profiling::scope!("present");
            // wgpu doesn't document where vsync can happen. Maybe here?
            let start = web_time::Instant::now();
            render_state.queue.present(output_frame);
            vsync_sec += start.elapsed().as_secs_f32();
        }

        self.primer.mark_presented(viewport_id);
        if let Some(message) = self.startup.first_present_message() {
            log::info!("{message}");
        }

        vsync_sec
    }

    /// Call this at the beginning of each frame to receive the requested screenshots.
    pub fn handle_screenshots(&self, events: &mut Vec<Event>) {
        for (viewport_id, user_data, screenshot) in self.capture_rx.try_iter() {
            let screenshot = Arc::new(screenshot);
            for data in user_data {
                events.push(Event::Screenshot {
                    viewport_id,
                    user_data: data,
                    image: Arc::clone(&screenshot),
                });
            }
        }
    }

    pub fn gc_viewports(&mut self, active_viewports: &ViewportIdSet) {
        self.surfaces.retain(|id, _| active_viewports.contains(id));
        self.depth_texture_view
            .retain(|id, _| active_viewports.contains(id));
        self.msaa_texture_view
            .retain(|id, _| active_viewports.contains(id));
        self.primer.retain(active_viewports);
    }

    pub fn destroy(&mut self) {
        self.suspend();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epaint::{Color32, ColorImage, ImageDelta};

    fn colors(texture: &TextureSnapshot) -> &[Color32] {
        let epaint::ImageData::Color(image) = &texture.image;
        &image.pixels
    }

    #[test]
    fn texture_snapshots_compose_partial_updates_for_replay() {
        let id = epaint::TextureId::Managed(1);
        let full = ColorImage::new(
            [3, 2],
            vec![
                Color32::from_rgb(1, 0, 0),
                Color32::from_rgb(2, 0, 0),
                Color32::from_rgb(3, 0, 0),
                Color32::from_rgb(4, 0, 0),
                Color32::from_rgb(5, 0, 0),
                Color32::from_rgb(6, 0, 0),
            ],
        );
        let patch = ColorImage::new([1, 1], vec![Color32::from_rgb(99, 0, 0)]);

        let mut snapshots = TextureSnapshots::default();
        let mut deltas = epaint::textures::TexturesDelta::default();
        deltas.push(
            id,
            ImageDelta::full(full, epaint::textures::TextureOptions::NEAREST),
        );
        snapshots.record(&deltas);
        deltas.clear();

        deltas.push(
            id,
            ImageDelta::partial([1, 1], patch, epaint::textures::TextureOptions::LINEAR),
        );
        snapshots.record(&deltas);
        deltas.clear();

        let snapshot = &snapshots.textures[&id];
        assert_eq!(snapshot.options, epaint::textures::TextureOptions::LINEAR);
        assert_eq!(
            colors(snapshot),
            &[
                Color32::from_rgb(1, 0, 0),
                Color32::from_rgb(2, 0, 0),
                Color32::from_rgb(3, 0, 0),
                Color32::from_rgb(4, 0, 0),
                Color32::from_rgb(99, 0, 0),
                Color32::from_rgb(6, 0, 0),
            ]
        );
        assert_eq!(snapshot.full_delta().pos, None);
    }

    #[test]
    fn texture_snapshots_forget_freed_textures() {
        let id = epaint::TextureId::Managed(1);
        let mut snapshots = TextureSnapshots::default();
        let mut deltas = epaint::textures::TexturesDelta::default();
        deltas.push(
            id,
            ImageDelta::full(
                ColorImage::filled([2, 2], Color32::WHITE),
                epaint::textures::TextureOptions::NEAREST,
            ),
        );
        snapshots.record(&deltas);
        deltas.clear();
        assert!(snapshots.textures.contains_key(&id));

        deltas.free(id);
        snapshots.record(&deltas);
        deltas.clear();
        assert!(!snapshots.textures.contains_key(&id));
    }

    #[test]
    fn texture_snapshots_skip_duplicating_font_atlas_pixels() {
        let font_id = epaint::TextureId::default();
        let mut snapshots = TextureSnapshots::default();
        let mut deltas = epaint::textures::TexturesDelta::default();
        deltas.push(
            font_id,
            ImageDelta::full(
                ColorImage::filled([4, 4], Color32::WHITE),
                epaint::textures::TextureOptions::NEAREST,
            ),
        );
        snapshots.record(&deltas);
        deltas.clear();

        assert!(
            !snapshots.textures.contains_key(&font_id),
            "font atlas must not be duplicated in CPU snapshot memory"
        );
        assert_eq!(
            snapshots.font_atlas_options,
            Some(epaint::textures::TextureOptions::NEAREST)
        );
        assert!(snapshots.ids().any(|id| id == font_id));

        deltas.push(
            font_id,
            ImageDelta::partial(
                [1, 1],
                ColorImage::filled([1, 1], Color32::WHITE),
                epaint::textures::TextureOptions::LINEAR,
            ),
        );
        snapshots.record(&deltas);
        deltas.clear();
        assert_eq!(
            snapshots.font_atlas_options,
            Some(epaint::textures::TextureOptions::LINEAR)
        );

        deltas.free(font_id);
        snapshots.record(&deltas);
        deltas.clear();
        assert_eq!(snapshots.font_atlas_options, None);
        assert!(!snapshots.ids().any(|id| id == font_id));
    }

    fn second_viewport() -> ViewportId {
        ViewportId::from_hash_of("second viewport")
    }

    #[test]
    fn primer_state_requires_a_present_after_each_new_surface() {
        let mut primer = PrimerState::default();
        assert!(!primer.needs_primer(ViewportId::ROOT));

        primer.mark_new_surface(ViewportId::ROOT);
        assert!(primer.needs_primer(ViewportId::ROOT));

        primer.mark_presented(ViewportId::ROOT);
        assert!(!primer.needs_primer(ViewportId::ROOT));

        // A re-created surface holds undefined pixels again.
        primer.mark_new_surface(ViewportId::ROOT);
        assert!(primer.needs_primer(ViewportId::ROOT));
    }

    #[test]
    fn primer_state_tracks_each_viewport_separately() {
        let mut primer = PrimerState::default();
        primer.mark_new_surface(ViewportId::ROOT);
        primer.mark_new_surface(second_viewport());

        primer.mark_presented(ViewportId::ROOT);
        assert!(!primer.needs_primer(ViewportId::ROOT), "the root viewport presented");
        assert!(
            primer.needs_primer(second_viewport()),
            "the second viewport has not presented yet, whatever the root did"
        );

        primer.mark_presented(second_viewport());
        assert!(!primer.needs_primer(second_viewport()));
    }

    #[test]
    fn primer_state_clear_forgets_every_pending_surface() {
        // `Painter::drop_all_surfaces` is the only caller: both teardown paths drop the
        // surfaces and their primer requirement together, so nothing is left whose pixels are
        // undefined.
        let mut primer = PrimerState::default();
        primer.mark_new_surface(ViewportId::ROOT);
        primer.mark_new_surface(second_viewport());

        primer.clear();
        assert!(!primer.needs_primer(ViewportId::ROOT));
        assert!(!primer.needs_primer(second_viewport()));
    }

    #[test]
    fn primer_state_forgets_closed_viewports() {
        let mut primer = PrimerState::default();
        primer.mark_new_surface(ViewportId::ROOT);
        primer.mark_new_surface(second_viewport());

        let active: ViewportIdSet = [ViewportId::ROOT].into_iter().collect();
        primer.retain(&active);

        assert!(!primer.needs_primer(second_viewport()));
        assert!(primer.needs_primer(ViewportId::ROOT));
        primer.mark_presented(ViewportId::ROOT);
        assert!(!primer.needs_primer(ViewportId::ROOT));
    }

    /// Timing values a test can reason about, without a GPU deciding how long anything took.
    fn startup_timings(
        device_ms: Option<f32>,
        surface_ms: Option<f32>,
        renderer_ms: Option<f32>,
    ) -> StartupTimings {
        let mut timings = StartupTimings::new(web_time::Instant::now());
        timings.device_ms = device_ms;
        timings.surface_ms = surface_ms;
        timings.renderer_ms = renderer_ms;
        timings
    }

    #[test]
    fn startup_message_reports_every_phase() {
        let timings = startup_timings(Some(4.0), Some(2.5), Some(11.75));
        assert_eq!(
            timings.message(17.75),
            "wgpu first frame presented 17.8 ms after painter creation: \
             device 4.0 ms, surface 2.5 ms, pipelines 11.8 ms"
        );
    }

    #[test]
    fn startup_message_marks_phases_that_never_ran() {
        let timings = startup_timings(Some(4.0), None, None);
        assert!(timings.message(9.0).contains("surface n/a ms"));
        assert!(timings.message(9.0).contains("pipelines n/a ms"));
    }

    #[test]
    fn late_pipeline_phase_is_reported_after_the_first_present() {
        let mut timings = startup_timings(Some(4.0), Some(2.0), None);

        // Nothing is reported before a frame reached the screen.
        assert!(timings.late_renderer_message().is_none());

        assert!(timings.first_present_message().is_some());
        assert!(timings.late_renderer_message().is_none());

        timings.renderer_ms = Some(13.5);
        let message = timings.late_renderer_message().expect("late phase reported");
        assert!(message.contains("pipelines compiled 13.5 ms"), "{message}");
    }

    #[test]
    fn startup_line_is_reported_once_and_keeps_the_first_measurement() {
        let mut timings = StartupTimings::new(web_time::Instant::now());

        timings.record_device(web_time::Instant::now());
        let first_device_ms = timings.device_ms.expect("device phase recorded");

        // A second device (after `suspend`) must not overwrite the start-up measurement.
        timings.record_device(web_time::Instant::now());
        assert_eq!(timings.device_ms, Some(first_device_ms));

        assert!(timings.first_present_message().is_some());
        assert!(timings.first_present_message().is_none(), "reported only once");
    }
}
