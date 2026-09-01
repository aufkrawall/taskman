//! Derived from eframe's `glow_integration.rs`.
//!
//! UPSTREAM_BASE: 05297424601afdf5966e5e01c8c63eef2696df54 (tag 0.36.1)
//!
//! When rebasing the fork onto a new upstream tag, diff `glow_integration.rs` between
//! `UPSTREAM_BASE` and the new tag and port anything relevant here by hand, then update
//! the sha above in the same commit. `tools/fork-rebase.sh` in the parent repo prints
//! that diff. This is the single largest recurring maintenance cost of the fork; keeping
//! this file a thin shell over the *shared* `EpiIntegration` and `egui_winit` machinery is
//! what keeps that cost bounded.
//!
//! # A `softbuffer` backend
//!
//! Everything the GPU backends do with a driver, a swapchain and a shader compiler, this
//! does with a `CreateDIBSection` (on Win32; SHM on X11, `wl_shm` on Wayland,
//! `CoreGraphics` on macOS) and a `BitBlt`. [`egui_software`] paints into that buffer.
//!
//! # Deliberate limitation: one viewport
//!
//! `glow_integration.rs` is largely multi-window machinery -- deferred viewports,
//! immediate viewports, per-viewport GL surfaces, and the parent/child repaint routing
//! between them. This backend supports the **root viewport only**. Child viewports are
//! logged once and ignored rather than half-supported: a partial implementation of
//! viewport lifetime is the kind of thing that works in testing and strands a window
//! on someone's desktop in production.
//!
//! `egui::ViewportCommand`s that act on the root window (title, visibility, close,
//! decorations, always-on-top, ...) are handled in full via `egui_winit`.

use std::{cell::RefCell, num::NonZeroU32, rc::Rc, sync::Arc, time::Instant};

use egui::{DeferredViewportUiCallback, ViewportId, ViewportInfo, ViewportOutput};
#[cfg(feature = "accesskit")]
use egui_winit::accesskit_winit;
use egui_winit::{
    ActionRequested, EventResponse, create_winit_window_attributes, process_viewport_commands,
};
use winit::{
    event_loop::ActiveEventLoop,
    window::{Window, WindowId},
};

use crate::{
    App, AppCreator, CreationContext, NativeOptions, Result,
    native::{
        epi_integration::{self, EpiIntegration},
        winit_integration::{
            EventResult, UserEvent, WinitApp, create_egui_context, sleep_if_invisible_or_minimized,
        },
    },
};

pub struct SoftwareWinitApp<'app> {
    repaint_proxy: Arc<egui::mutex::Mutex<winit::event_loop::EventLoopProxy<UserEvent>>>,
    app_name: String,
    native_options: NativeOptions,
    running: Option<SoftwareWinitRunning<'app>>,

    // Only used to create the first window; taken on `resumed`.
    app_creator: Option<AppCreator<'app>>,

    /// Set when a caller supplied their own context (e.g. for testing).
    egui_ctx: Option<egui::Context>,
}

struct SoftwareWinitRunning<'app> {
    integration: EpiIntegration,
    app: Box<dyn 'app + App>,

    window: Arc<Window>,
    egui_winit: egui_winit::State,
    info: ViewportInfo,

    painter: egui_software::Painter,
    /// `Context` must outlive `Surface`, and both borrow the window, so they are kept
    /// together and behind an `Rc<RefCell<..>>` for the same reason glow keeps its
    /// painter that way: the repaint callback needs a weak handle.
    surface: Rc<RefCell<SurfaceState>>,

    /// Whether any frame has been presented yet. The first one has to happen even
    /// while the window is hidden, because it is what triggers `post_rendering` to show
    /// the window.
    has_presented_once: bool,

    /// Set once the first frame has actually reached the screen, so the log records
    /// that the CPU path is live and what it decided about sub-pixel text. A renderer
    /// that starts but never paints is otherwise indistinguishable from one that works.
    logged_first_frame: bool,

    /// Texture deltas that arrived while the window was not paintable (minimised, or
    /// zero-sized). Dropping a `TexturesDelta` panics, and skipping one would leave the
    /// atlas permanently stale, so they accumulate until a frame can consume them.
    pending_deltas: egui::TexturesDelta,
}

struct SurfaceState {
    /// Field order is load-bearing: `surface` borrows from `context`, and Rust drops
    /// fields in declaration order.
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    _context: softbuffer::Context<Arc<Window>>,
    /// Last size we told softbuffer about, so `resize` is not called every frame.
    size: (u32, u32),
}

impl<'app> SoftwareWinitApp<'app> {
    pub fn new(
        event_loop: &winit::event_loop::EventLoop<UserEvent>,
        app_name: &str,
        native_options: NativeOptions,
        app_creator: AppCreator<'app>,
    ) -> Self {
        Self {
            repaint_proxy: Arc::new(egui::mutex::Mutex::new(event_loop.create_proxy())),
            app_name: app_name.to_owned(),
            native_options,
            running: None,
            app_creator: Some(app_creator),
            egui_ctx: None,
        }
    }

    /// Supply a pre-built context, as `run_native` does when the caller provided one.
    pub fn set_egui_ctx(&mut self, egui_ctx: Option<egui::Context>) {
        self.egui_ctx = egui_ctx;
    }

    fn init_run_state(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        profiling::function_scope!();

        let storage = if let Some(file) = &self.native_options.persistence_path {
            epi_integration::create_storage_with_file(file)
        } else {
            epi_integration::create_storage(
                self.native_options
                    .viewport
                    .app_id
                    .as_ref()
                    .unwrap_or(&self.app_name),
            )
        };

        let egui_ctx = self
            .egui_ctx
            .take()
            .unwrap_or_else(|| create_egui_context(storage.as_deref()));

        let window = {
            let mut builder = epi_integration::viewport_builder(
                egui_ctx.zoom_factor(),
                event_loop,
                &mut self.native_options,
                epi_integration::load_window_settings(storage.as_deref()),
            )
            .with_visible(false); // shown after the first frame, to avoid a flash of empty window
            builder.title.get_or_insert_with(|| self.app_name.clone());
            let attrs = create_winit_window_attributes(&egui_ctx, builder.clone());
            let window = event_loop
                .create_window(attrs)
                .map_err(crate::Error::Winit)?;
            epi_integration::apply_window_settings(
                &window,
                epi_integration::load_window_settings(storage.as_deref()),
            );
            Arc::new(window)
        };

        let context = softbuffer::Context::new(Arc::clone(&window)).map_err(|err| {
            crate::Error::AppCreation(format!("softbuffer context: {err}").into())
        })?;
        let surface = softbuffer::Surface::new(&context, Arc::clone(&window)).map_err(|err| {
            crate::Error::AppCreation(format!("softbuffer surface: {err}").into())
        })?;

        let mut egui_winit = egui_winit::State::new(
            egui_ctx.clone(),
            ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            event_loop.system_theme(),
            // A CPU renderer has no texture-size limit of its own; this only bounds the
            // glyph atlas, and epaint already caps that through `TextOptions`.
            Some(16 * 1024),
        );

        let integration = EpiIntegration::new(
            egui_ctx.clone(),
            &window,
            &self.app_name,
            &self.native_options,
            storage,
            #[cfg(feature = "glow")]
            None,
            #[cfg(feature = "glow")]
            None,
            #[cfg(feature = "wgpu_no_default_features")]
            None,
        );

        {
            let event_loop_proxy = Arc::clone(&self.repaint_proxy);
            integration
                .egui_ctx
                .set_request_repaint_callback(move |info| {
                    log::trace!("request_repaint_callback: {info:?}");
                    let when = Instant::now() + info.delay;
                    let cumulative_pass_nr = info.current_cumulative_pass_nr;
                    event_loop_proxy
                        .lock()
                        .send_event(UserEvent::RequestRepaint {
                            viewport_id: info.viewport_id,
                            when,
                            cumulative_pass_nr,
                        })
                        .ok();
                });
        }

        #[cfg(feature = "accesskit")]
        {
            let event_loop_proxy = self.repaint_proxy.lock().clone();
            egui_winit.init_accesskit(event_loop, &window, event_loop_proxy);
        }

        if self
            .native_options
            .viewport
            .mouse_passthrough
            .unwrap_or(false)
            && let Err(err) = window.set_cursor_hittest(false)
        {
            log::warn!("set_cursor_hittest(false) failed: {err}");
        }

        let app_creator = std::mem::take(&mut self.app_creator)
            .expect("Single-use AppCreator has unexpectedly already been taken");

        crate::maybe_attach_inspection_plugin(&integration.egui_ctx, Some(self.app_name.clone()));

        let app: Box<dyn 'app + App> = {
            use raw_window_handle::{HasDisplayHandle as _, HasWindowHandle as _};
            let cc = CreationContext {
                egui_ctx: integration.egui_ctx.clone(),
                integration_info: integration.frame.info().clone(),
                storage: integration.frame.storage(),
                #[cfg(feature = "glow")]
                gl: None,
                #[cfg(feature = "glow")]
                get_proc_address: None,
                #[cfg(feature = "wgpu_no_default_features")]
                wgpu_render_state: None,
                window: Some(Arc::clone(&window)),
                raw_display_handle: window.display_handle().map(|h| h.as_raw()),
                raw_window_handle: window.window_handle().map(|h| h.as_raw()),
            };
            profiling::scope!("app_creator");
            app_creator(&cc).map_err(crate::Error::AppCreation)?
        };

        let mut info = ViewportInfo::default();
        egui_winit::update_viewport_info(&mut info, &integration.egui_ctx, &window, true);

        // Immediate viewports would need their own surface and paint pass. Rather than
        // render them wrongly, refuse them loudly once and let the parent frame proceed.
        egui::Context::set_immediate_viewport_renderer(|_ctx, viewport| {
            log::warn!(
                "eframe: the software renderer does not support immediate viewports; \
                 viewport {:?} will not be shown",
                viewport.ids.this
            );
        });

        self.running = Some(SoftwareWinitRunning {
            integration,
            app,
            window,
            egui_winit,
            info,
            painter: egui_software::Painter::new(),
            surface: Rc::new(RefCell::new(SurfaceState {
                surface,
                _context: context,
                size: (0, 0),
            })),
            has_presented_once: false,
            logged_first_frame: false,
            pending_deltas: Default::default(),
        });

        Ok(())
    }
}

impl SoftwareWinitRunning<'_> {
    /// Build the [`egui_software::ShapeContext`] for this frame.
    ///
    /// These are the same values `egui::Context::tessellate` reads internally; the
    /// software painter tessellates non-text shapes itself so it can intercept text.
    fn shape_context(&self, pixels_per_point: f32) -> egui_software::ShapeContext {
        let ctx = &self.integration.egui_ctx;
        egui_software::ShapeContext {
            pixels_per_point,
            options: ctx.tessellation_options(|o| *o),
            font_tex_size: ctx.fonts(|f| f.font_image_size()),
            prepared_discs: ctx.fonts(|f| f.fonts.texture_atlas().prepared_discs()),
        }
    }

    fn run_ui_and_paint(&mut self, event_loop: &ActiveEventLoop) -> Result<EventResult> {
        profiling::function_scope!();
        profiling::finish_frame!();

        let mut frame_timer = crate::stopwatch::Stopwatch::new();
        frame_timer.start();

        self.integration.pre_update();

        egui_winit::update_viewport_info(
            &mut self.info,
            &self.integration.egui_ctx,
            &self.window,
            false,
        );

        let mut raw_input = self.egui_winit.take_egui_input(&self.window);
        raw_input.viewports = std::iter::once((ViewportId::ROOT, self.info.clone())).collect();
        // Handed to egui now, so they must not be delivered again next frame -- a Close
        // event that repeats would fire the app's close handling on every frame forever.
        self.info.events.clear();

        // --- user code; holds no borrow of the surface ---
        let full_output = self.integration.update(
            self.app.as_mut(),
            None::<&DeferredViewportUiCallback>,
            raw_input,
        );

        let egui::FullOutput {
            platform_output,
            textures_delta,
            shapes,
            pixels_per_point,
            viewport_output,
        } = full_output;

        self.pending_deltas.append(textures_delta);

        self.egui_winit
            .handle_platform_output(&self.window, platform_output);

        // The *first* frame must be painted even though the window is still hidden:
        // `post_rendering` below is what reveals it, and eframe deliberately keeps the
        // window hidden until something has been drawn to avoid a white flash. After
        // that, painting a window nobody can see is pure waste -- and worse than waste
        // on Windows, where upstream documents an invisible window burning a whole core
        // (emilk/egui#7776). Forced continuous repaints into a hidden window also grow
        // the stack until the process dies, which is reproducible here with the painting
        // removed entirely, so it is the repaint loop and not the rasterizer.
        //
        // Skipping is safe because visibility changes arrive as viewport commands, which
        // are processed below regardless -- restoring from the tray resumes painting.
        let size = self.window.inner_size();
        let hidden =
            self.window.is_visible() == Some(false) || self.window.is_minimized() == Some(true);
        let paintable = size.width > 0 && size.height > 0 && (!hidden || !self.has_presented_once);

        if paintable {
            // Take the sub-pixel mode from the ATLAS, not from the style. The atlas is
            // what actually holds the coverage, so asking it makes the blend mode and the
            // rasterization mode agree by construction -- the one failure here that does
            // not error, it just draws wrong colours.
            // Sub-pixel mode *and* its blend parameters both come from the atlas's
            // options, so the way glyphs were rasterized and the way they are blended
            // cannot disagree. That is the one failure here that does not error -- it
            // just draws wrong colours.
            let (subpixel, gamma, contrast) = self.integration.egui_ctx.fonts(|f| {
                let o = f.options();
                (o.subpixel, o.text_gamma, o.text_contrast)
            });
            self.painter.set_subpixel(subpixel, gamma, contrast);

            let clear = self.app.clear_color(
                &self
                    .integration
                    .egui_ctx
                    .style_of(self.integration.egui_ctx.theme())
                    .visuals,
            );
            let shape_ctx = self.shape_context(pixels_per_point);

            // Applying deltas before painting, and frees after, is the order every egui
            // backend uses: a texture is only freed once no shape in the finished frame
            // references it.
            let deltas = std::mem::take(&mut self.pending_deltas);
            #[expect(clippy::iter_over_hash_type)] // per-id order is what matters, and is preserved
            for (id, image_deltas) in &deltas.set {
                for delta in image_deltas {
                    self.painter.set_texture(*id, delta);
                }
            }

            {
                let mut state = self.surface.borrow_mut();
                let (Some(w), Some(h)) =
                    (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                else {
                    return Ok(EventResult::Wait);
                };
                if state.size != (size.width, size.height) {
                    state.surface.resize(w, h).map_err(|err| {
                        crate::Error::AppCreation(format!("resize: {err}").into())
                    })?;
                    state.size = (size.width, size.height);
                }

                let mut buffer = state.surface.buffer_mut().map_err(|err| {
                    crate::Error::AppCreation(format!("softbuffer buffer: {err}").into())
                })?;

                if let Some(mut target) =
                    egui_software::Target::new(&mut buffer, size.width, size.height)
                {
                    // `clear_color` returns premultiplied gamma-space floats in 0..=1.
                    // The alpha is meaningless on an opaque window buffer, so only the RGB
                    // is used, scaled back to bytes.
                    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                    egui_software::Painter::clear(
                        &mut target,
                        egui::Color32::from_rgb(byte(clear[0]), byte(clear[1]), byte(clear[2])),
                    );
                    self.painter.paint_shapes(&mut target, &shape_ctx, shapes);
                }

                self.has_presented_once = true;
                if !self.logged_first_frame {
                    self.logged_first_frame = true;
                    log::info!(
                        "software renderer: first frame {}x{} px, sub-pixel text {:?}",
                        size.width,
                        size.height,
                        subpixel
                    );
                }

                buffer.present().map_err(|err| {
                    crate::Error::AppCreation(format!("softbuffer present: {err}").into())
                })?;
            }

            #[expect(clippy::iter_over_hash_type)] // a set of ids; order is irrelevant
            for id in &deltas.free {
                self.painter.free_texture(*id);
            }
            let mut deltas = deltas;
            deltas.clear();

            self.integration.post_rendering(&self.window);
        } else {
            // Nothing was painted, so the shapes are dropped here. The deltas are NOT:
            // they stay in `pending_deltas` for the next paintable frame.
            drop(shapes);
        }

        let mut result = EventResult::Wait;
        for (id, output) in viewport_output {
            if id == ViewportId::ROOT {
                self.handle_root_viewport_output(&output);
                if output.repaint_delay.is_zero() {
                    result = EventResult::RepaintNext(self.window.id());
                } else if let Some(when) = Instant::now().checked_add(output.repaint_delay) {
                    result = EventResult::RepaintAt(self.window.id(), when);
                }
            } else {
                log::warn!(
                    "eframe: the software renderer supports only the root viewport; \
                     ignoring viewport {id:?}"
                );
            }
        }

        sleep_if_invisible_or_minimized(Some(&self.window));

        self.integration
            .report_frame_time(frame_timer.total_time_sec());
        self.integration
            .maybe_autosave(self.app.as_mut(), Some(&self.window));

        if self.integration.should_close() {
            return Ok(EventResult::CloseRequested);
        }

        let _ = event_loop;
        Ok(result)
    }

    fn handle_root_viewport_output(&mut self, output: &ViewportOutput) {
        // Screenshot and cut/copy/paste requests come back here. The software renderer
        // has no separate framebuffer to read back, so they are collected and dropped
        // rather than silently pretended-to-be-handled.
        let mut actions_requested: Vec<ActionRequested> = Vec::new();
        process_viewport_commands(
            &self.integration.egui_ctx,
            &mut self.info,
            output.commands.clone(),
            &self.window,
            &mut actions_requested,
        );
        for action in actions_requested {
            log::debug!("software renderer: unhandled viewport action {action:?}");
        }
    }
}

impl WinitApp for SoftwareWinitApp<'_> {
    fn egui_ctx(&self) -> Option<&egui::Context> {
        self.running.as_ref().map(|r| &r.integration.egui_ctx)
    }

    fn window(&self, window_id: WindowId) -> Option<Arc<Window>> {
        self.running
            .as_ref()
            .filter(|r| r.window.id() == window_id)
            .map(|r| Arc::clone(&r.window))
    }

    fn window_id_from_viewport_id(&self, id: ViewportId) -> Option<WindowId> {
        (id == ViewportId::ROOT)
            .then(|| self.running.as_ref().map(|r| r.window.id()))
            .flatten()
    }

    fn save(&mut self) {
        if let Some(r) = &mut self.running {
            let window = Arc::clone(&r.window);
            r.integration.save(r.app.as_mut(), Some(&window));
        }
    }

    fn save_and_destroy(&mut self) {
        if let Some(mut r) = self.running.take() {
            let window = Arc::clone(&r.window);
            r.integration.save(r.app.as_mut(), Some(&window));
            #[cfg(feature = "glow")]
            r.app.on_exit(None);
            #[cfg(not(feature = "glow"))]
            r.app.on_exit();
        }
    }

    fn run_ui_and_paint(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
    ) -> Result<EventResult> {
        let Some(running) = &mut self.running else {
            return Ok(EventResult::Wait);
        };
        if running.window.id() != window_id {
            return Ok(EventResult::Wait);
        }
        running.run_ui_and_paint(event_loop)
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) -> Result<EventResult> {
        if self.running.is_none() {
            self.init_run_state(event_loop)?;
        }
        let window_id = self
            .running
            .as_ref()
            .map(|r| r.window.id())
            .expect("just initialised");
        Ok(EventResult::RepaintNow(window_id))
    }

    fn suspended(&mut self, _: &ActiveEventLoop) -> Result<EventResult> {
        // There is no GPU surface to lose, so nothing to tear down.
        Ok(EventResult::Wait)
    }

    fn device_event(
        &mut self,
        _: &ActiveEventLoop,
        _: winit::event::DeviceId,
        event: winit::event::DeviceEvent,
    ) -> Result<EventResult> {
        if let winit::event::DeviceEvent::MouseMotion { delta } = event
            && let Some(r) = &mut self.running
        {
            r.egui_winit.on_mouse_motion(delta);
            return Ok(EventResult::RepaintNext(r.window.id()));
        }
        Ok(EventResult::Wait)
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: winit::event::WindowEvent,
    ) -> Result<EventResult> {
        // Once `save_and_destroy` has run, `running` is gone and the window has been
        // dropped. Any event arriving after that means the shutdown is complete and the
        // loop should end: `CloseRequested` only tears the window down, it does not exit,
        // so without this the process lives on with no window and the close button
        // appears to do nothing.
        let Some(running) = &mut self.running else {
            return Ok(EventResult::Exit);
        };
        if running.window.id() != window_id {
            return Ok(EventResult::Wait);
        }

        // `EpiIntegration::on_window_event` forwards to `egui_winit` internally and
        // returns its response. Calling `egui_winit.on_window_event` again here would
        // deliver every click, key press and scroll twice.
        let EventResponse { repaint, consumed } =
            running
                .integration
                .on_window_event(&running.window, &mut running.egui_winit, &event);
        let _ = consumed;
        let _ = event_loop;

        // On Windows a resize is delivered synchronously while the user drags the frame.
        // Repainting on the next tick instead of now makes the content visibly lag behind
        // the window border, so resizes are painted inside the event handler.
        // See https://github.com/emilk/egui/issues/903.
        let mut repaint_asap = false;

        match &event {
            winit::event::WindowEvent::CloseRequested => {
                if running.integration.should_close() {
                    // `CloseRequested`, not `Exit`: the wrapper runs `save_and_destroy`
                    // on this variant, and windows must be dropped while the event loop
                    // is still running for winit to clean up properly.
                    return Ok(EventResult::CloseRequested);
                }

                // `egui_winit` does NOT translate this into a viewport event -- it only
                // asks for a repaint -- so the backend has to raise it, or the app never
                // learns the user clicked the close button. taskman vetoes the close and
                // hides to the tray instead, which is exactly the path this feeds.
                running.info.events.push(egui::ViewportEvent::Close);
                running.integration.egui_ctx.request_repaint();
                return Ok(EventResult::RepaintNext(window_id));
            }
            winit::event::WindowEvent::Destroyed => return Ok(EventResult::Exit),
            winit::event::WindowEvent::Resized(size) => {
                // winit signals "minimised" on Windows as a resize to 0x0.
                if 0 < size.width && 0 < size.height {
                    repaint_asap = true;
                }
            }
            _ => {}
        }

        if repaint_asap {
            Ok(EventResult::RepaintNow(window_id))
        } else if repaint {
            Ok(EventResult::RepaintNext(window_id))
        } else {
            Ok(EventResult::Wait)
        }
    }

    #[cfg(feature = "accesskit")]
    fn on_accesskit_event(&mut self, event: accesskit_winit::Event) -> Result<EventResult> {
        use super::winit_integration;

        let Some(running) = &mut self.running else {
            return Ok(EventResult::Wait);
        };
        if running.window.id() != event.window_id {
            return Ok(EventResult::Wait);
        }
        Ok(winit_integration::on_accesskit_window_event(
            &mut running.egui_winit,
            event.window_id,
            &event.window_event,
        ))
    }
}
