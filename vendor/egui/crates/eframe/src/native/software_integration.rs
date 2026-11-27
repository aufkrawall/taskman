//! Derived from eframe's `glow_integration.rs`.
//!
//! `UPSTREAM_BASE`: 05297424601afdf5966e5e01c8c63eef2696df54 (tag 0.36.1)
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

use std::{
    cell::RefCell,
    num::NonZeroU32,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

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
            EventResult, ResidencyAction, ResidencyTimer, UserEvent, WinitApp, create_egui_context,
            sleep_if_invisible_or_minimized,
        },
    },
};

struct AtomicSoftwareFrameStats {
    frames: AtomicU64,
    skipped_damage: AtomicU64,
    dirty_tiles: AtomicU64,
    total_tiles: AtomicU64,
    pass_ns_sum: AtomicU64,
    pass_ns_max: AtomicU64,
    paint_ns_sum: AtomicU64,
    paint_ns_max: AtomicU64,
    present_ns_sum: AtomicU64,
    present_ns_max: AtomicU64,
}

static SOFTWARE_FRAME_STATS: AtomicSoftwareFrameStats = AtomicSoftwareFrameStats {
    frames: AtomicU64::new(0),
    skipped_damage: AtomicU64::new(0),
    dirty_tiles: AtomicU64::new(0),
    total_tiles: AtomicU64::new(0),
    pass_ns_sum: AtomicU64::new(0),
    pass_ns_max: AtomicU64::new(0),
    paint_ns_sum: AtomicU64::new(0),
    paint_ns_max: AtomicU64::new(0),
    present_ns_sum: AtomicU64::new(0),
    present_ns_max: AtomicU64::new(0),
};

fn update_max(slot: &AtomicU64, val: u64) {
    slot.fetch_max(val, Ordering::Relaxed);
}

pub(crate) fn software_frame_stats_take() -> crate::SoftwareFrameStats {
    crate::SoftwareFrameStats {
        frames: SOFTWARE_FRAME_STATS.frames.swap(0, Ordering::Relaxed),
        skipped_damage: SOFTWARE_FRAME_STATS
            .skipped_damage
            .swap(0, Ordering::Relaxed),
        dirty_tiles: SOFTWARE_FRAME_STATS.dirty_tiles.swap(0, Ordering::Relaxed),
        total_tiles: SOFTWARE_FRAME_STATS.total_tiles.swap(0, Ordering::Relaxed),
        pass_ns_sum: SOFTWARE_FRAME_STATS.pass_ns_sum.swap(0, Ordering::Relaxed),
        pass_ns_max: SOFTWARE_FRAME_STATS.pass_ns_max.swap(0, Ordering::Relaxed),
        paint_ns_sum: SOFTWARE_FRAME_STATS.paint_ns_sum.swap(0, Ordering::Relaxed),
        paint_ns_max: SOFTWARE_FRAME_STATS.paint_ns_max.swap(0, Ordering::Relaxed),
        present_ns_sum: SOFTWARE_FRAME_STATS
            .present_ns_sum
            .swap(0, Ordering::Relaxed),
        present_ns_max: SOFTWARE_FRAME_STATS
            .present_ns_max
            .swap(0, Ordering::Relaxed),
    }
}

/// Whether `window` is a Wayland surface.
fn is_wayland(window: &winit::window::Window) -> bool {
    use winit::raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    window
        .window_handle()
        .is_ok_and(|handle| matches!(handle.as_raw(), RawWindowHandle::Wayland(_)))
}

/// Whether this frame must repaint every pixel before the painter even looks at the
/// buffer's age.
///
/// An age of 0 means undefined contents; an older buffer (Wayland alternates two, so
/// it reports 2 on every frame) is repainted incrementally by
/// `paint_shapes_damaged_aged` from its record of what the frames since changed, and
/// falls back to a full repaint on its own when that record is too short.
fn should_force_full_paint(
    has_presented_once: bool,
    presented_while_visible: bool,
    buffer_age: u8,
) -> bool {
    !has_presented_once || !presented_while_visible || buffer_age == 0
}

/// Clamp `rect` to `[0..width, 0..height]`, returning `None` when the intersection is empty.
fn clamp_pixel_rect(
    rect: egui_software::PixelRect,
    width: u32,
    height: u32,
) -> Option<egui_software::PixelRect> {
    let max_w = i32::try_from(width).ok()?;
    let max_h = i32::try_from(height).ok()?;
    let min_x = rect.min_x.clamp(0, max_w);
    let min_y = rect.min_y.clamp(0, max_h);
    let max_x = rect.max_x.clamp(0, max_w);
    let max_y = rect.max_y.clamp(0, max_h);
    (min_x < max_x && min_y < max_y).then_some(egui_software::PixelRect {
        min_x,
        min_y,
        max_x,
        max_y,
    })
}

fn rect_contains(outer: egui_software::PixelRect, inner: egui_software::PixelRect) -> bool {
    outer.min_x <= inner.min_x
        && outer.min_y <= inner.min_y
        && outer.max_x >= inner.max_x
        && outer.max_y >= inner.max_y
}

/// Combine egui's tile-damage rects with any OS-invalidated update rect (`WM_PAINT`).
///
/// Even when zero egui tiles changed, a non-empty OS update region means the windowing
/// system needs those pixels re-blitted from our retained buffer. Returns `(rects, is_full)`.
fn rects_to_present(
    dirty_rects: &[egui_software::PixelRect],
    os_update_rect: Option<egui_software::PixelRect>,
    width: u32,
    height: u32,
) -> (Vec<egui_software::PixelRect>, bool) {
    let Some(full_rect) = clamp_pixel_rect(
        egui_software::PixelRect {
            min_x: 0,
            min_y: 0,
            max_x: i32::try_from(width).unwrap_or(i32::MAX),
            max_y: i32::try_from(height).unwrap_or(i32::MAX),
        },
        width,
        height,
    ) else {
        return (Vec::new(), false);
    };

    let os_clamped = os_update_rect.and_then(|r| clamp_pixel_rect(r, width, height));
    if os_clamped == Some(full_rect) || (dirty_rects.len() == 1 && dirty_rects[0] == full_rect) {
        return (vec![full_rect], true);
    }

    let mut rects: Vec<egui_software::PixelRect> = dirty_rects
        .iter()
        .copied()
        .filter_map(|r| clamp_pixel_rect(r, width, height))
        .collect();

    if let Some(os_rc) = os_clamped
        && !rects.iter().any(|r| rect_contains(*r, os_rc))
    {
        rects.push(os_rc);
    }

    let is_full = rects.len() == 1 && rects[0] == full_rect;
    (rects, is_full)
}

/// Track the app's own visibility request, so eframe's first-frame reveal cannot
/// override it (see [`EpiIntegration::post_rendering`]).
fn note_visibility_commands(app_requested_hidden: &mut bool, commands: &[egui::ViewportCommand]) {
    for command in commands {
        if let egui::ViewportCommand::Visible(visible) = command {
            *app_requested_hidden = !visible;
        }
    }
}

#[cfg(target_os = "windows")]
#[expect(unsafe_code)]
mod platform_surface {
    use std::{
        num::NonZeroU32,
        ptr::{self, NonNull},
        slice,
        sync::Arc,
    };

    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use windows_sys::Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi,
    };
    use winit::window::Window;

    /// Upper bound on buffer allocation (64 megapixels = 256 MiB, well above dual-8K).
    const MAX_BUFFER_PIXELS: usize = 64 * 1024 * 1024;

    #[repr(C)]
    struct BitmapInfo {
        bmi_header: Gdi::BITMAPINFOHEADER,
        bmi_colors: [Gdi::RGBQUAD; 3],
    }

    const ZERO_QUAD: Gdi::RGBQUAD = Gdi::RGBQUAD {
        rgbBlue: 0,
        rgbGreen: 0,
        rgbRed: 0,
        rgbReserved: 0,
    };

    pub(super) struct Win32Buffer {
        dc: Gdi::HDC,
        bitmap: Gdi::HBITMAP,
        old_obj: Gdi::HGDIOBJ,
        pixels: NonNull<u32>,
        width: u32,
        height: u32,
        len: usize,
        presented: bool,
    }

    impl Drop for Win32Buffer {
        fn drop(&mut self) {
            // SAFETY: `dc`, `bitmap`, and `old_obj` are owned GDI handles created on this thread.
            unsafe {
                if !self.old_obj.is_null() {
                    Gdi::SelectObject(self.dc, self.old_obj);
                }
                if !self.bitmap.is_null() {
                    Gdi::DeleteObject(self.bitmap);
                }
                if !self.dc.is_null() {
                    Gdi::DeleteDC(self.dc);
                }
            }
        }
    }

    impl Win32Buffer {
        fn new(width: NonZeroU32, height: NonZeroU32) -> Result<Self, String> {
            let w_u32 = width.get();
            let h_u32 = height.get();
            let w_i32 =
                i32::try_from(w_u32).map_err(|_error| "width out of i32 range".to_owned())?;
            let h_i32 =
                i32::try_from(h_u32).map_err(|_error| "height out of i32 range".to_owned())?;
            let len = (w_u32 as usize)
                .checked_mul(h_u32 as usize)
                .filter(|&n| n > 0 && n <= MAX_BUFFER_PIXELS)
                .ok_or_else(|| format!("invalid buffer dimensions {w_u32}x{h_u32}"))?;

            // SAFETY: Creates a memory DC compatible with the current display on the window thread.
            let dc = unsafe { Gdi::CreateCompatibleDC(ptr::null_mut()) };
            if dc.is_null() {
                return Err(format!(
                    "CreateCompatibleDC failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let bitmap_info = BitmapInfo {
                bmi_header: Gdi::BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<Gdi::BITMAPINFOHEADER>() as u32,
                    biWidth: w_i32,
                    biHeight: -h_i32, // top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: Gdi::BI_BITFIELDS,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmi_colors: [
                    Gdi::RGBQUAD {
                        rgbRed: 0xff,
                        ..ZERO_QUAD
                    },
                    Gdi::RGBQUAD {
                        rgbGreen: 0xff,
                        ..ZERO_QUAD
                    },
                    Gdi::RGBQUAD {
                        rgbBlue: 0xff,
                        ..ZERO_QUAD
                    },
                ],
            };

            let mut raw_pixels: *mut u32 = ptr::null_mut();
            // SAFETY: `bitmap_info` is a valid 32-bpp BI_BITFIELDS header and `raw_pixels` receives
            // the GDI-allocated section pointer.
            let bitmap = unsafe {
                Gdi::CreateDIBSection(
                    dc,
                    ptr::from_ref(&bitmap_info).cast(),
                    Gdi::DIB_RGB_COLORS,
                    ptr::from_mut(&mut raw_pixels).cast(),
                    ptr::null_mut(),
                    0,
                )
            };
            let Some(pixels) = (!bitmap.is_null())
                .then(|| NonNull::new(raw_pixels))
                .flatten()
            else {
                let err = std::io::Error::last_os_error();
                // SAFETY: Clean up any partially created GDI objects before returning error.
                unsafe {
                    if !bitmap.is_null() {
                        Gdi::DeleteObject(bitmap);
                    }
                    Gdi::DeleteDC(dc);
                }
                return Err(format!("CreateDIBSection({w_u32}x{h_u32}) failed: {err}"));
            };

            // SAFETY: `dc` and `bitmap` are valid handles just created above.
            let old_obj = unsafe { Gdi::SelectObject(dc, bitmap) };
            if old_obj.is_null() {
                // SAFETY: Clean up GDI handles on SelectObject failure.
                unsafe {
                    Gdi::DeleteObject(bitmap);
                    Gdi::DeleteDC(dc);
                }
                return Err("SelectObject failed for DIB section".to_owned());
            }

            Ok(Self {
                dc,
                bitmap,
                old_obj,
                pixels,
                width: w_u32,
                height: h_u32,
                len,
                presented: false,
            })
        }

        pub(super) fn age(&self) -> u8 {
            u8::from(self.presented)
        }

        pub(super) fn pixels_mut(&mut self) -> &mut [u32] {
            // SAFETY: `pixels` points to `len` 32-bit pixels allocated by `CreateDIBSection` and
            // exclusively borrowed via `&mut self`.
            unsafe { slice::from_raw_parts_mut(self.pixels.as_ptr(), self.len) }
        }
    }

    /// One frame's pixels: the surface's DIB, borrowed for the frame. The
    /// other platform's `FrameBuffer` owns its buffer instead, so this is the
    /// same shape to the shared paint path.
    pub(super) struct FrameBuffer<'a> {
        buffer: &'a mut Win32Buffer,
    }

    impl FrameBuffer<'_> {
        pub(super) fn age(&self) -> u8 {
            self.buffer.age()
        }

        pub(super) fn pixels_mut(&mut self) -> &mut [u32] {
            self.buffer.pixels_mut()
        }
    }

    pub(super) struct SurfaceState {
        hwnd: HWND,
        _window: Arc<Window>,
        buffer: Option<Win32Buffer>,
        pub(super) size: (u32, u32),
    }

    impl SurfaceState {
        pub(super) fn new(window: &Arc<Window>) -> crate::Result<Self> {
            let raw = window
                .window_handle()
                .map_err(|err| crate::Error::AppCreation(format!("window_handle: {err}").into()))?
                .as_raw();
            let RawWindowHandle::Win32(handle) = raw else {
                return Err(crate::Error::AppCreation(
                    "expected Win32 window handle".into(),
                ));
            };
            let hwnd = handle.hwnd.get() as HWND;
            if hwnd.is_null() {
                return Err(crate::Error::AppCreation("Win32 HWND is null".into()));
            }
            // Verify that a device context can be acquired for this window.
            // SAFETY: `hwnd` is a live window handle created by winit on this thread.
            let dc = unsafe { Gdi::GetDC(hwnd) };
            if dc.is_null() {
                return Err(crate::Error::AppCreation(
                    format!("GetDC failed: {}", std::io::Error::last_os_error()).into(),
                ));
            }
            // SAFETY: Releases the common DC back to the window immediately.
            unsafe {
                Gdi::ReleaseDC(hwnd, dc);
            }
            Ok(Self {
                hwnd,
                _window: Arc::clone(window),
                buffer: None,
                size: (0, 0),
            })
        }

        pub(super) fn resize(
            &mut self,
            width: NonZeroU32,
            height: NonZeroU32,
        ) -> Result<(), String> {
            if self
                .buffer
                .as_ref()
                .is_some_and(|b| b.width == width.get() && b.height == height.get())
            {
                self.size = (width.get(), height.get());
                return Ok(());
            }
            let new_buffer = Win32Buffer::new(width, height)?;
            self.buffer = Some(new_buffer);
            self.size = (width.get(), height.get());
            Ok(())
        }

        pub(super) fn buffer_mut(&mut self) -> Result<FrameBuffer<'_>, String> {
            let buffer = self
                .buffer
                .as_mut()
                .ok_or_else(|| "surface buffer not allocated".to_owned())?;
            Ok(FrameBuffer { buffer })
        }

        /// Query the OS update region (`GetUpdateRect`) before `DefWindowProcW` validates it.
        pub(super) fn os_update_rect(&self) -> Option<egui_software::PixelRect> {
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            // SAFETY: `self.hwnd` is valid for the lifetime of `_window`.
            let has_update = unsafe { Gdi::GetUpdateRect(self.hwnd, &mut rc, 0) != 0 };
            if has_update && rc.right > rc.left && rc.bottom > rc.top {
                Some(egui_software::PixelRect {
                    min_x: rc.left,
                    min_y: rc.top,
                    max_x: rc.right,
                    max_y: rc.bottom,
                })
            } else {
                None
            }
        }

        /// Blit `rects` from the DIB buffer to the window DC, checking `BitBlt`'s return value
        /// and validating only regions that actually succeeded.
        pub(super) fn present_rects(
            &mut self,
            rects: &[egui_software::PixelRect],
            validate_full: bool,
        ) -> Result<(), String> {
            let Some(buffer) = self.buffer.as_mut() else {
                return Err("surface buffer not allocated".to_owned());
            };
            // Acquire a fresh DC per present rather than holding a common DC across frames or
            // display-mode changes.
            // SAFETY: `self.hwnd` is valid on this thread.
            let window_dc = unsafe { Gdi::GetDC(self.hwnd) };
            if window_dc.is_null() {
                buffer.presented = false;
                return Err(format!(
                    "GetDC returned null during present: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let max_w = i32::try_from(buffer.width).unwrap_or(i32::MAX);
            let max_h = i32::try_from(buffer.height).unwrap_or(i32::MAX);
            for rect in rects.iter().copied() {
                let min_x = rect.min_x.clamp(0, max_w);
                let min_y = rect.min_y.clamp(0, max_h);
                let max_x = rect.max_x.clamp(0, max_w);
                let max_y = rect.max_y.clamp(0, max_h);
                let width = max_x - min_x;
                let height = max_y - min_y;
                if width <= 0 || height <= 0 {
                    continue;
                }
                // SAFETY: `window_dc` and `buffer.dc` are valid DCs and the rectangle is clamped
                // to the allocated DIB dimensions.
                let ok = unsafe {
                    Gdi::BitBlt(
                        window_dc,
                        min_x,
                        min_y,
                        width,
                        height,
                        buffer.dc,
                        min_x,
                        min_y,
                        Gdi::SRCCOPY,
                    )
                };
                if ok == 0 {
                    let err = std::io::Error::last_os_error();
                    // SAFETY: Always release the common window DC before returning.
                    unsafe {
                        Gdi::ReleaseDC(self.hwnd, window_dc);
                    }
                    buffer.presented = false;
                    return Err(format!(
                        "BitBlt({min_x},{min_y} {width}x{height}) failed: {err}"
                    ));
                }
                if !validate_full {
                    let valid = RECT {
                        left: min_x,
                        top: min_y,
                        right: max_x,
                        bottom: max_y,
                    };
                    // SAFETY: Validates only the rectangle whose pixels were just blitted.
                    unsafe {
                        Gdi::ValidateRect(self.hwnd, &valid);
                    }
                }
            }

            // SAFETY: Release the common window DC.
            unsafe {
                Gdi::ReleaseDC(self.hwnd, window_dc);
                if validate_full {
                    Gdi::ValidateRect(self.hwnd, ptr::null());
                }
            }
            buffer.presented = true;
            Ok(())
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod platform_surface {
    use std::{num::NonZeroU32, sync::Arc};
    use winit::window::Window;

    /// One frame's pixels, in the shape the shared paint path asks for.
    ///
    /// `softbuffer` hands out a `Buffer` that derefs to the pixel slice and also
    /// reports the age of its contents. That age is the only sound answer to the
    /// question the shared paint path asks: a backend may hand back a buffer that
    /// was never painted (`age == 0`) or one that holds the frame from *two*
    /// presents ago (`age == 2`; Wayland keeps two `wl_shm` buffers and swaps them
    /// on every present, and a released buffer's contents are undefined).
    /// Painting incrementally into either one leaves every undamaged pixel as
    /// whatever that buffer happened to contain -- stale or uninitialised
    /// regions, which is exactly how the Linux window ends up with black boxes.
    pub(super) struct FrameBuffer<'a> {
        buffer: softbuffer::Buffer<'a, Arc<Window>, Arc<Window>>,
    }

    impl FrameBuffer<'_> {
        /// `softbuffer`'s own age: `1` only for the buffer that was presented on
        /// the previous frame, at the size the pixels were painted at.
        pub(super) fn age(&self) -> u8 {
            self.buffer.age()
        }

        pub(super) fn pixels_mut(&mut self) -> &mut [u32] {
            &mut self.buffer
        }
    }

    pub(super) struct SurfaceState {
        /// Field order is load-bearing: `surface` borrows from `_context`, and Rust drops
        /// fields in declaration order.
        surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
        _context: softbuffer::Context<Arc<Window>>,
        pub(super) size: (u32, u32),
    }

    impl SurfaceState {
        pub(super) fn new(window: &Arc<Window>) -> crate::Result<Self> {
            let context = softbuffer::Context::new(Arc::clone(window)).map_err(|err| {
                crate::Error::AppCreation(format!("softbuffer context: {err}").into())
            })?;
            let surface =
                softbuffer::Surface::new(&context, Arc::clone(window)).map_err(|err| {
                    crate::Error::AppCreation(format!("softbuffer surface: {err}").into())
                })?;
            Ok(Self {
                surface,
                _context: context,
                size: (0, 0),
            })
        }

        pub(super) fn resize(
            &mut self,
            width: NonZeroU32,
            height: NonZeroU32,
        ) -> Result<(), String> {
            if self.size == (width.get(), height.get()) {
                return Ok(());
            }
            self.surface
                .resize(width, height)
                .map_err(|err| format!("resize: {err}"))?;
            self.size = (width.get(), height.get());
            Ok(())
        }

        pub(super) fn buffer_mut(&mut self) -> Result<FrameBuffer<'_>, String> {
            let buffer = self
                .surface
                .buffer_mut()
                .map_err(|err| format!("softbuffer buffer: {err}"))?;
            Ok(FrameBuffer { buffer })
        }

        #[expect(clippy::unused_self)]
        pub(super) fn os_update_rect(&self) -> Option<egui_software::PixelRect> {
            // The X11/Wayland backends report damage to the window system
            // themselves (`present_with_damage`), so there is no OS-side
            // update region to merge in.
            None
        }

        pub(super) fn present_rects(
            &mut self,
            rects: &[egui_software::PixelRect],
            validate_full: bool,
        ) -> Result<(), String> {
            let buffer = self
                .surface
                .buffer_mut()
                .map_err(|err| format!("softbuffer buffer: {err}"))?;
            let presented = if validate_full {
                buffer.present()
            } else {
                let sb_rects: Vec<softbuffer::Rect> = rects
                    .iter()
                    .filter_map(|r| {
                        let w = NonZeroU32::new(u32::try_from(r.width()).ok()?)?;
                        let h = NonZeroU32::new(u32::try_from(r.height()).ok()?)?;
                        Some(softbuffer::Rect {
                            x: u32::try_from(r.min_x).ok()?,
                            y: u32::try_from(r.min_y).ok()?,
                            width: w,
                            height: h,
                        })
                    })
                    .collect();
                if sb_rects.is_empty() {
                    buffer.present()
                } else {
                    buffer.present_with_damage(&sb_rects)
                }
            };
            presented.map_err(|err| format!("softbuffer present: {err}"))?;
            Ok(())
        }
    }
}

use platform_surface::SurfaceState;

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
    surface: Rc<RefCell<Option<SurfaceState>>>,
    /// Tracks how long the window has been hidden so non-Windows SHM surfaces
    /// can be released after `HIDDEN_RESIDENCY_GRACE`.
    residency: ResidencyTimer,

    /// Whether any frame has been presented yet. The first one has to happen even
    /// while the window is hidden, because it is what triggers `post_rendering` to show
    /// the window.
    has_presented_once: bool,

    /// Whether the full buffer has been presented while the window was actually
    /// visible and unminimized. Frame 1 paints while hidden (before `post_rendering`
    /// calls `window.set_visible(true)`), and a tray/minimize hide discards the
    /// DWM surface even though `softbuffer` still reports `age() == 1`. Until a
    /// full `present()` succeeds on the visible window, unchanged tiles would stay
    /// at the OS background brush (white).
    presented_while_visible: bool,

    /// Set once the first frame has actually reached the screen, so the log records
    /// that the CPU path is live and what it decided about sub-pixel text. A renderer
    /// that starts but never paints is otherwise indistinguishable from one that works.
    logged_first_frame: bool,

    /// Texture deltas that arrived while the window was not paintable (minimised, or
    /// zero-sized). Dropping a `TexturesDelta` panics, and skipping one would leave the
    /// atlas permanently stale, so they accumulate until a frame can consume them.
    pending_deltas: egui::TexturesDelta,

    /// The app itself asked for the window to be hidden and has not asked for it
    /// back since. `post_rendering` reveals the window after the first painted
    /// frame, which must not override that request: a start-in-tray launch sends
    /// `ViewportCommand::Visible(false)` in its first frame, and the reveal would
    /// leave the window on screen with nothing left to hide it.
    app_requested_hidden: bool,
}

impl Drop for SoftwareWinitRunning<'_> {
    fn drop(&mut self) {
        // A `TexturesDelta` left unapplied panics in its `Drop` (debug builds):
        // exiting while the window is hidden, with a font/theme/texture change
        // still queued, would panic during teardown. The atlas is gone with the
        // window anyway.
        self.pending_deltas.clear();
    }
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

        let surface_state = SurfaceState::new(&window)?;

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
            surface: Rc::new(RefCell::new(Some(surface_state))),
            residency: ResidencyTimer::default(),
            has_presented_once: false,
            presented_while_visible: false,
            logged_first_frame: false,
            pending_deltas: Default::default(),
            app_requested_hidden: false,
        });

        Ok(())
    }
}

impl SoftwareWinitRunning<'_> {
    fn sync_hidden_residency(&mut self, visible: bool) {
        let now = Instant::now();
        let was_suspended = self.residency.is_suspended();
        let action = self.residency.observe(now, visible);
        match action {
            ResidencyAction::Keep => {}
            ResidencyAction::Pause | ResidencyAction::KeepResident => {
                if cfg!(not(target_os = "windows")) {
                    self.integration
                        .egui_ctx
                        .request_repaint_after(self.residency.remaining_grace(now));
                }
            }
            ResidencyAction::Suspend => {
                if cfg!(not(target_os = "windows")) {
                    let dropped = self.surface.borrow_mut().take().is_some();
                    self.presented_while_visible = false;
                    if dropped || !was_suspended {
                        log::debug!("Released the software surface after the grace");
                    }
                }
            }
        }
    }

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

    #[expect(clippy::unnecessary_wraps)]
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

        let initial_size = self.window.inner_size();
        let initial_hidden = self.window.is_visible() == Some(false)
            || self.window.is_minimized() == Some(true)
            || self.info.occluded == Some(true);

        if initial_hidden {
            self.presented_while_visible = false;
        }

        // After the first frame has been painted, a hidden, minimized, or occluded
        // window runs only `App::logic` (`update_logic_only`), matching `wgpu` and
        // `glow`. This keeps `App::ui` from running while off-screen (so the app
        // knows to release background telemetry providers and stretch its tick) and
        // avoids laying out into a 0x0 minimized viewport. If `logic` issues a
        // viewport command that reveals/unminimizes the window (e.g. tray restore
        // or Ctrl+Shift+Esc), we process those commands and immediately fall through
        // to paint and present the restored frame in this same turn.
        if self.has_presented_once
            && (initial_hidden || initial_size.width == 0 || initial_size.height == 0)
        {
            let mut raw_input = self.egui_winit.take_egui_input(&self.window);
            raw_input.viewports = std::iter::once((ViewportId::ROOT, self.info.clone())).collect();
            self.info.events.clear();

            let egui::LogicOutput {
                platform_output,
                viewport_commands,
            } = self
                .integration
                .update_logic_only(self.app.as_mut(), raw_input);

            self.egui_winit
                .handle_platform_output(&self.window, platform_output);

            if let Some(commands) = viewport_commands.get(&ViewportId::ROOT) {
                let mut actions_requested: Vec<ActionRequested> = Vec::new();
                note_visibility_commands(&mut self.app_requested_hidden, commands);
                process_viewport_commands(
                    &self.integration.egui_ctx,
                    &mut self.info,
                    commands.iter().cloned(),
                    &self.window,
                    &mut actions_requested,
                );
            }

            let now_hidden = self.window.is_visible() == Some(false)
                || self.window.is_minimized() == Some(true)
                || self.info.occluded == Some(true);
            let now_size = self.window.inner_size();
            if now_hidden || now_size.width == 0 || now_size.height == 0 {
                self.sync_hidden_residency(false);
                sleep_if_invisible_or_minimized(Some(&self.window));
                self.integration
                    .maybe_autosave(self.app.as_mut(), Some(&self.window));
                if self.integration.should_close() {
                    return Ok(EventResult::CloseRequested);
                }
                return Ok(EventResult::Wait);
            }

            egui_winit::update_viewport_info(
                &mut self.info,
                &self.integration.egui_ctx,
                &self.window,
                false,
            );
        }

        let mut raw_input = self.egui_winit.take_egui_input(&self.window);
        raw_input.viewports = std::iter::once((ViewportId::ROOT, self.info.clone())).collect();
        // Handed to egui now, so they must not be delivered again next frame -- a Close
        // event that repeats would fire the app's close handling on every frame forever.
        self.info.events.clear();

        // --- user code; holds no borrow of the surface ---
        let pass_start = Instant::now();
        let full_output = self.integration.update(
            self.app.as_mut(),
            None::<&DeferredViewportUiCallback>,
            raw_input,
        );
        let pass_ns = pass_start.elapsed().as_nanos() as u64;
        SOFTWARE_FRAME_STATS.frames.fetch_add(1, Ordering::Relaxed);
        SOFTWARE_FRAME_STATS
            .pass_ns_sum
            .fetch_add(pass_ns, Ordering::Relaxed);
        update_max(&SOFTWARE_FRAME_STATS.pass_ns_max, pass_ns);

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

        // Process viewport commands BEFORE painting if they reveal or unminimize the
        // window, so the present below lands directly onto the visible window surface.
        for (id, output) in &viewport_output {
            if *id == ViewportId::ROOT {
                self.handle_root_viewport_output(output);
            } else {
                log::warn!(
                    "eframe: the software renderer supports only the root viewport; \
                     ignoring viewport {id:?}"
                );
            }
        }

        let size = self.window.inner_size();
        let hidden = self.window.is_visible() == Some(false)
            || self.window.is_minimized() == Some(true)
            || self.info.occluded == Some(true);
        // TaskMan fork: the first-frame exception paints a still-hidden window so
        // Windows' reveal has a frame behind it. On Wayland a present maps the
        // surface, so a window the app hid (start minimized to the tray) or one
        // still waiting for its remap configure must never present: winit
        // reports both as `is_visible() == Some(false)`.
        let wayland_hidden = self.window.is_visible() == Some(false) && is_wayland(&self.window);
        let paintable = size.width > 0
            && size.height > 0
            && (!hidden || (!self.has_presented_once && !wayland_hidden));

        if hidden {
            self.presented_while_visible = false;
        }
        if paintable {
            self.sync_hidden_residency(!hidden);
            // Sub-pixel mode *and* its blend parameters both come from the atlas's
            // options, so the way glyphs were rasterized and the way they are blended
            // cannot disagree.
            let text_options = self.integration.egui_ctx.fonts(|f| *f.options());
            self.painter.set_text_options(&text_options);
            let subpixel = text_options.subpixel;

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

            if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
            {
                let mut surface_slot = self.surface.borrow_mut();
                if surface_slot.is_none() {
                    match SurfaceState::new(&self.window) {
                        Ok(fresh) => *surface_slot = Some(fresh),
                        Err(err) => {
                            log::warn!("software renderer surface recreate failed: {err}");
                            self.presented_while_visible = false;
                            self.integration
                                .egui_ctx
                                .request_repaint_after(std::time::Duration::from_millis(50));
                        }
                    }
                }
                if let Some(state) = surface_slot.as_mut() {
                    let os_dirty = if hidden { None } else { state.os_update_rect() };
                    if let Err(err) = state.resize(w, h) {
                        log::warn!("software renderer resize({w}x{h}) failed: {err}");
                        self.presented_while_visible = false;
                        self.integration
                            .egui_ctx
                            .request_repaint_after(std::time::Duration::from_millis(50));
                    } else {
                        let paint_outcome = match state.buffer_mut() {
                            Ok(mut buffer) => {
                                let buffer_age = buffer.age();
                                let force_full = should_force_full_paint(
                                    self.has_presented_once,
                                    self.presented_while_visible,
                                    buffer_age,
                                );
                                let paint_start = Instant::now();
                                let damage = if let Some(mut target) = egui_software::Target::new(
                                    buffer.pixels_mut(),
                                    size.width,
                                    size.height,
                                ) {
                                    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                                    let clear_color = egui::Color32::from_rgb(
                                        byte(clear[0]),
                                        byte(clear[1]),
                                        byte(clear[2]),
                                    );
                                    self.painter.paint_shapes_damaged_aged(
                                        &mut target,
                                        &shape_ctx,
                                        shapes,
                                        clear_color,
                                        force_full,
                                        buffer_age,
                                    )
                                } else {
                                    egui_software::DamageStats::default()
                                };
                                let paint_ns = paint_start.elapsed().as_nanos() as u64;
                                SOFTWARE_FRAME_STATS
                                    .paint_ns_sum
                                    .fetch_add(paint_ns, Ordering::Relaxed);
                                update_max(&SOFTWARE_FRAME_STATS.paint_ns_max, paint_ns);
                                SOFTWARE_FRAME_STATS
                                    .dirty_tiles
                                    .fetch_add(damage.dirty_tiles as u64, Ordering::Relaxed);
                                SOFTWARE_FRAME_STATS
                                    .total_tiles
                                    .fetch_add(damage.total_tiles as u64, Ordering::Relaxed);
                                Some((damage, force_full))
                            }
                            Err(err) => {
                                log::warn!("software renderer buffer access failed: {err}");
                                self.presented_while_visible = false;
                                self.integration
                                    .egui_ctx
                                    .request_repaint_after(std::time::Duration::from_millis(50));
                                None
                            }
                        };

                        if let Some((damage, force_full)) = paint_outcome {
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

                            let (mut present_rects, full_screen) = rects_to_present(
                                &damage.dirty_rects,
                                os_dirty,
                                size.width,
                                size.height,
                            );
                            if present_rects.is_empty() && !hidden && is_wayland(&self.window) {
                                // Wayland paces redraws by the frame callback that only a
                                // commit earns, so an unchanged frame that skipped its present
                                // left nothing to wait on: a repaint request that changes no
                                // pixel (a tooltip timer, an animation at rest) spun the loop.
                                // The back buffer already holds this frame -- nothing changed
                                // since its own -- so commit it with a token damage rect.
                                SOFTWARE_FRAME_STATS
                                    .skipped_damage
                                    .fetch_add(1, Ordering::Relaxed);
                                present_rects.push(egui_software::PixelRect {
                                    min_x: 0,
                                    min_y: 0,
                                    max_x: 1,
                                    max_y: 1,
                                });
                            }
                            if present_rects.is_empty() {
                                SOFTWARE_FRAME_STATS
                                    .skipped_damage
                                    .fetch_add(1, Ordering::Relaxed);
                                // No present, so no buffer aged: the painter's record of
                                // per-present changes must not count this frame.
                                self.painter.forget_unpresented_frame();
                            } else {
                                let present_start = Instant::now();
                                // TaskMan fork: announce the present so winit can
                                // throttle the next redraw to the compositor's
                                // frame callback (Wayland). Without it every input
                                // event of a scroll or scrollbar drag repainted the
                                // whole window at the event rate, not the display
                                // rate — a full core on a 1000 Hz mouse. The GPU
                                // backends already do this in `egui-wgpu`.
                                self.window.pre_present_notify();
                                match state.present_rects(&present_rects, full_screen) {
                                    Ok(()) => {
                                        let present_ns = present_start.elapsed().as_nanos() as u64;
                                        SOFTWARE_FRAME_STATS
                                            .present_ns_sum
                                            .fetch_add(present_ns, Ordering::Relaxed);
                                        update_max(
                                            &SOFTWARE_FRAME_STATS.present_ns_max,
                                            present_ns,
                                        );
                                        if !hidden && (full_screen || force_full) {
                                            self.presented_while_visible = true;
                                        }
                                    }
                                    Err(err) => {
                                        log::warn!("software renderer present failed: {err}");
                                        self.painter.forget_unpresented_frame();
                                        self.presented_while_visible = false;
                                        self.integration.egui_ctx.request_repaint_after(
                                            std::time::Duration::from_millis(50),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }

            #[expect(clippy::iter_over_hash_type)] // a set of ids; order is irrelevant
            for id in &deltas.free {
                self.painter.free_texture(*id);
            }
            let mut deltas = deltas;
            deltas.clear();

            self.integration
                .post_rendering(&self.window, !self.app_requested_hidden);

            // Frame 1 painted and presented while the window was still hidden
            // (`IsWindowVisible == false`), so Windows discarded that `BitBlt`.
            // Now that `post_rendering` revealed the window, immediately blit
            // the already-painted buffer if kept (`age == 1`), or schedule a redraw.
            let visible_now = self.window.is_visible() != Some(false)
                && self.window.is_minimized() != Some(true)
                && self.info.occluded != Some(true);
            if !self.presented_while_visible && visible_now {
                let full_rect = egui_software::PixelRect {
                    min_x: 0,
                    min_y: 0,
                    max_x: i32::try_from(size.width).unwrap_or(0),
                    max_y: i32::try_from(size.height).unwrap_or(0),
                };
                let mut presented_now = false;
                let mut present_failed = false;
                if let Ok(mut slot) = self.surface.try_borrow_mut()
                    && let Some(state) = slot.as_mut()
                    && state.buffer_mut().is_ok_and(|b| b.age() == 1)
                {
                    self.window.pre_present_notify();
                    match state.present_rects(&[full_rect], true) {
                        Ok(()) => presented_now = true,
                        Err(err) => {
                            // Do not request an immediate redraw here: that
                            // would bypass the 50 ms backoff the paint path
                            // scheduled and spin at the event rate while the
                            // DC/display is broken.
                            present_failed = true;
                            log::warn!("software renderer reveal re-present failed: {err}");
                            self.integration
                                .egui_ctx
                                .request_repaint_after(std::time::Duration::from_millis(50));
                        }
                    }
                }
                if presented_now {
                    self.presented_while_visible = true;
                } else if !present_failed {
                    // No kept buffer yet: a fresh frame must be painted before
                    // this window can show anything but stale pixels.
                    self.window.request_redraw();
                }
            }
        } else {
            if hidden {
                self.presented_while_visible = false;
            }
            self.sync_hidden_residency(false);
            // Nothing was painted, so the shapes are dropped here. The deltas are NOT:
            // they stay in `pending_deltas` for the next paintable frame.
            drop(shapes);
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
        // Repaint scheduling is driven by `set_request_repaint_callback` (which
        // sends `UserEvent::RequestRepaint`), matching `wgpu_integration` and
        // `glow_integration`. Returning `EventResult::Wait` here prevents duplicate
        // synchronous repaint scheduling inside `WM_PAINT`.
        Ok(EventResult::Wait)
    }

    fn handle_root_viewport_output(&mut self, output: &ViewportOutput) {
        // Screenshot and cut/copy/paste requests come back here. The software renderer
        // has no separate framebuffer to read back, so they are collected and dropped
        // rather than silently pretended-to-be-handled.
        note_visibility_commands(&mut self.app_requested_hidden, &output.commands);
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
                } else {
                    running.presented_while_visible = false;
                }
            }
            winit::event::WindowEvent::Occluded(is_occluded) => {
                running.info.occluded = Some(*is_occluded);
                if !*is_occluded {
                    running.presented_while_visible = false;
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

#[cfg(test)]
mod tests {
    use super::{rects_to_present, should_force_full_paint};
    use egui_software::PixelRect;

    #[test]
    fn full_paint_required_until_presented_while_visible() {
        // Frame 1 (hidden): must force full paint.
        assert!(should_force_full_paint(false, false, 0));
        // Frame 2 right after Frame 1 revealed the window: buffer reports
        // age == 1 from the hidden Frame 1, so `presented_while_visible == false`
        // must still force a full paint/present onto the visible window.
        assert!(should_force_full_paint(true, false, 1));
        // Once presented while visible and buffer age is 1, incremental damage is valid.
        assert!(!should_force_full_paint(true, true, 1));
        // Undefined contents (new/resized/failed present) force full.
        assert!(should_force_full_paint(true, true, 0));
        // An older buffer is left to the painter, which repaints what the frames
        // since changed (`egui_software/tests/buffer_age.rs` pins that pixel for pixel)
        // and paints in full past its record.
        assert!(!should_force_full_paint(true, true, 2));
        assert!(!should_force_full_paint(true, true, 3));
    }

    #[test]
    fn os_update_rect_forces_present_even_when_zero_egui_tiles_changed() {
        let os_rect = PixelRect {
            min_x: 10,
            min_y: 20,
            max_x: 150,
            max_y: 90,
        };
        // Zero egui damage + no OS update region -> skip present.
        let (rects, is_full) = rects_to_present(&[], None, 800, 600);
        assert!(rects.is_empty());
        assert!(!is_full);

        // Zero egui damage + OS WM_PAINT update region -> must blit the OS region.
        let (rects, is_full) = rects_to_present(&[], Some(os_rect), 800, 600);
        assert_eq!(rects, vec![os_rect]);
        assert!(!is_full);

        // Full-window OS update region promotes to full-window present.
        let full_os = PixelRect {
            min_x: -5,
            min_y: 0,
            max_x: 900,
            max_y: 600,
        };
        let (rects, is_full) = rects_to_present(&[], Some(full_os), 800, 600);
        assert!(is_full);
        assert_eq!(
            rects,
            vec![PixelRect {
                min_x: 0,
                min_y: 0,
                max_x: 800,
                max_y: 600,
            }]
        );

        // Partial egui damage + disjoint OS update rect includes both.
        let tile = PixelRect {
            min_x: 200,
            min_y: 200,
            max_x: 264,
            max_y: 264,
        };
        let (rects, is_full) = rects_to_present(&[tile], Some(os_rect), 800, 600);
        assert!(!is_full);
        assert_eq!(rects, vec![tile, os_rect]);

        // OS update rect already covered by an egui damage rect is not duplicated.
        let covering = PixelRect {
            min_x: 0,
            min_y: 0,
            max_x: 300,
            max_y: 300,
        };
        let (rects, is_full) = rects_to_present(&[covering], Some(os_rect), 800, 600);
        assert!(!is_full);
        assert_eq!(rects, vec![covering]);
    }
}
