//! Windows taskbar performance monitor (`UpdateLayeredWindow` + software Direct2D/DirectWrite).
//!
//! Renders CPU, RAM, and network rates directly into a 32-bit premultiplied
//! ARGB DIB section and presents it via `UpdateLayeredWindow` (`ULW_ALPHA`)
//! positioned beside `TrayNotifyWnd` on the primary Windows taskbar.
//!
//! Design invariants (avoiding DWM / Explorer interference):
//! - **Zero screen reads:** never calls `GetDC(NULL)`, `GetPixel`, or `BitBlt`
//!   from the screen DC (which causes DWM screen-capture fences). Taskbar
//!   light/dark contrast comes from `SystemUsesLightTheme` + `WM_SETTINGCHANGE`.
//! - **Zero cross-process `SetParent`:** runs as an independent `WS_EX_LAYERED |
//!   WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST` window on its own
//!   dedicated thread, never attaching to Explorer's Taskbar thread.
//! - **Zero GPU swapchain:** uses `D2D1_RENDER_TARGET_TYPE_SOFTWARE` into a
//!   cached DIB section and only calls `UpdateLayeredWindow` when rendered
//!   content, hover state, or DPI/geometry actually changes.
//! - **Fullscreen-safe:** hides automatically when a fullscreen window covers
//!   the taskbar monitor (preserving DWM Independent Flip / MPO and never
//!   floating over games/videos) or when an auto-hide taskbar slides away.

use std::sync::{Arc, Condvar, Mutex, OnceLock};

use tm_core::format::{format_taskbar_pct, format_taskbar_rate};
use tm_core::settings::{TaskbarColorMode, TaskbarNetUnit};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1DCRenderTarget,
    ID2D1Factory, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_PIXEL_GEOMETRY_BGR,
    DWRITE_PIXEL_GEOMETRY_RGB, DWRITE_RENDERING_MODE_DEFAULT, DWRITE_RENDERING_MODE_GDI_CLASSIC,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWriteCreateFactory, IDWriteFactory, IDWriteRenderingParams, IDWriteTextFormat,
    IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, HMONITOR, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromWindow, SelectObject,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{
    ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    SHAppBarMessage, SHQueryUserNotificationState,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CreatePopupMenu, CreateWindowExW, DestroyMenu, DestroyWindow, DispatchMessageW,
    EVENT_SYSTEM_FOREGROUND, FindWindowExW, FindWindowW, GW_HWNDPREV, GWL_EXSTYLE, GWLP_HWNDPARENT,
    GWLP_USERDATA, GetClassNameW, GetCursorPos, GetDesktopWindow, GetForegroundWindow, GetMessageW,
    GetShellWindow, GetWindow, GetWindowLongPtrW, GetWindowRect, HMENU, HWND_TOPMOST, IDC_ARROW,
    InsertMenuW, IsIconic, IsWindowVisible, LoadCursorW, MA_NOACTIVATE, MF_BYPOSITION,
    MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOOWNERZORDER, SWP_NOSENDCHANGING, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow,
    SetMenuDefaultItem, SetWindowLongPtrW, SetWindowPos, ShowWindow, TPM_BOTTOMALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, ULW_ALPHA,
    UpdateLayeredWindow, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_CLOSE,
    WM_CREATE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_NULL, WM_RBUTTONUP, WM_SETTINGCHANGE, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::taskbar_monitor::{Action, Config, Metrics};

const WM_APP_REFRESH: u32 = WM_APP + 10;
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Global HWND of the live taskbar monitor window so `SetWinEventHook` callbacks
/// (which carry no user-data pointer) can coalesce onto the window's queue.
static LIVE_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

#[derive(Debug)]
struct SharedState {
    config: Config,
    metrics: Metrics,
    generation: u64,
}

pub struct TaskbarMonitorHandle {
    shared: Arc<Mutex<SharedState>>,
    state_cv: Arc<Condvar>,
    callback: Arc<dyn Fn(Action) + Send + Sync>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl TaskbarMonitorHandle {
    pub fn new(callback: Arc<dyn Fn(Action) + Send + Sync>) -> Self {
        Self {
            shared: Arc::new(Mutex::new(SharedState {
                config: Config::default(),
                metrics: Metrics::default(),
                generation: 0,
            })),
            state_cv: Arc::new(Condvar::new()),
            callback,
            worker: Mutex::new(None),
        }
    }

    pub fn set_config(&self, config: Config) {
        let should_run = config.enabled;
        {
            let mut guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if guard.config == config {
                return;
            }
            guard.config = config;
            guard.generation = guard.generation.wrapping_add(1);
            self.state_cv.notify_all();
        }

        if should_run {
            self.ensure_started();
            self.post_refresh();
        } else {
            self.stop_worker();
        }
    }

    pub fn update_metrics(&self, metrics: Metrics) {
        let enabled = {
            let mut guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if guard.metrics == metrics {
                return;
            }
            guard.metrics = metrics;
            guard.generation = guard.generation.wrapping_add(1);
            guard.config.enabled
        };
        if enabled {
            self.post_refresh();
        }
    }

    pub fn shutdown(&self) {
        {
            let mut guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.config.enabled = false;
            guard.generation = guard.generation.wrapping_add(1);
            self.state_cv.notify_all();
        }
        self.stop_worker();
    }

    fn post_refresh(&self) {
        let raw = LIVE_HWND.load(std::sync::atomic::Ordering::Acquire);
        if raw != 0 {
            let hwnd = HWND(raw as *mut std::ffi::c_void);
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn ensure_started(&self) {
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = worker.as_ref()
            && !handle.is_finished()
        {
            return;
        }
        *worker = None;
        let shared = self.shared.clone();
        let state_cv = self.state_cv.clone();
        let callback = self.callback.clone();
        match std::thread::Builder::new()
            .name("tm-taskbar".into())
            .spawn(move || taskbar_thread_main(shared, state_cv, callback))
        {
            Ok(handle) => *worker = Some(handle),
            Err(error) => tracing::warn!(%error, "cannot start taskbar monitor thread"),
        }
    }

    fn stop_worker(&self) {
        let raw = LIVE_HWND.load(std::sync::atomic::Ordering::Acquire);
        if raw != 0 {
            let hwnd = HWND(raw as *mut std::ffi::c_void);
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TaskbarMonitorHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl PixelRect {
    fn from_win32(r: RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    fn width(self) -> i32 {
        (self.right - self.left).max(0)
    }

    fn height(self) -> i32 {
        (self.bottom - self.top).max(0)
    }

    fn is_empty(self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    fn covers(self, other: Self) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RenderedDisplay {
    show_net: bool,
    net_sent: Option<String>,
    net_recv: Option<String>,
    net_sample: String,
    cpu_label: Option<String>,
    cpu_val: Option<String>,
    ram_label: Option<String>,
    ram_val: Option<String>,
    sys_val_sample: String,
    font_family: String,
    font_size_pt: f32,
    text_rgb: [u8; 3],
    light_taskbar: bool,
    hovered: bool,
    dpi: u32,
}

struct DibSurface {
    hdc: HDC,
    bitmap: HBITMAP,
    old_obj: HGDIOBJ,
    #[cfg_attr(not(test), allow(dead_code))]
    bits: *const u8,
    width: i32,
    height: i32,
}

impl DibSurface {
    fn new(width: i32, height: i32) -> Option<Self> {
        let width = width.max(1);
        let height = height.max(1);
        unsafe {
            let hdc = CreateCompatibleDC(None);
            if hdc.is_invalid() {
                return None;
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    // Negative height -> top-down DIB.
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap =
                CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            if bitmap.is_invalid() || bits.is_null() {
                if !bitmap.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(bitmap.0));
                }
                let _ = DeleteDC(hdc);
                return None;
            }
            let old_obj = SelectObject(hdc, HGDIOBJ(bitmap.0));
            Some(Self {
                hdc,
                bitmap,
                old_obj,
                bits: bits.cast::<u8>(),
                width,
                height,
            })
        }
    }

    #[cfg(test)]
    fn pixels(&self) -> &[u8] {
        let len = (self.width.max(0) as usize)
            .saturating_mul(self.height.max(0) as usize)
            .saturating_mul(4);
        unsafe { std::slice::from_raw_parts(self.bits, len) }
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        unsafe {
            if !self.old_obj.is_invalid() {
                let _ = SelectObject(self.hdc, self.old_obj);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            }
            if !self.hdc.is_invalid() {
                let _ = DeleteDC(self.hdc);
            }
        }
    }
}

struct Renderer {
    d2d_factory: ID2D1Factory,
    dwrite_factory: IDWriteFactory,
    dc_rt: ID2D1DCRenderTarget,
    surface: Option<DibSurface>,
}

impl Renderer {
    fn new() -> Option<Self> {
        unsafe {
            let d2d_factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let dwrite_factory: IDWriteFactory =
                DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let dc_rt = d2d_factory.CreateDCRenderTarget(&props).ok()?;
            dc_rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            Some(Self {
                d2d_factory,
                dwrite_factory,
                dc_rt,
                surface: None,
            })
        }
    }

    fn create_text_format(
        &self,
        font_family: &str,
        font_size_pt: f32,
    ) -> Option<IDWriteTextFormat> {
        let family = if font_family.trim().is_empty() {
            "Segoe UI"
        } else {
            font_family.trim()
        };
        let wide_family: Vec<u16> = family.encode_utf16().chain([0]).collect();
        let dip_size = (font_size_pt.clamp(7.0, 18.0)) * (96.0 / 72.0);
        unsafe {
            let fmt = self
                .dwrite_factory
                .CreateTextFormat(
                    PCWSTR(wide_family.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    dip_size,
                    w!(""),
                )
                .ok()?;
            let _ = fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
            Some(fmt)
        }
    }

    fn measure_text(
        &self,
        fmt: &IDWriteTextFormat,
        text: &str,
    ) -> (f32, f32, Option<IDWriteTextLayout>) {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self
                .dwrite_factory
                .CreateTextLayout(&wide, fmt, 2000.0, 200.0)
            else {
                return (0.0, 0.0, None);
            };
            let mut metrics = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut metrics).is_err() {
                return (0.0, 0.0, Some(layout));
            }
            (
                metrics.widthIncludingTrailingWhitespace.ceil(),
                metrics.height.ceil(),
                Some(layout),
            )
        }
    }

    fn apply_text_rendering_params(&self, hwnd: Option<isize>, dpi: u32) {
        let params = crate::win::text_rendering::query(hwnd);
        let geom = if params.bgr {
            DWRITE_PIXEL_GEOMETRY_BGR
        } else {
            DWRITE_PIXEL_GEOMETRY_RGB
        };
        let mode = if dpi <= 120 {
            // Grid-fitted classic hinting keeps small taskbar fonts crisp at 100%–125% DPI.
            DWRITE_RENDERING_MODE_GDI_CLASSIC
        } else {
            DWRITE_RENDERING_MODE_DEFAULT
        };
        unsafe {
            // UpdateLayeredWindow uses scalar alpha, so D2D grayscale AA must stay active
            // while inheriting the monitor's ClearType gamma/contrast calibration.
            if let Ok(rp) = self.dwrite_factory.CreateCustomRenderingParams(
                params.gamma,
                params.contrast.max(0.5),
                0.0,
                geom,
                mode,
            ) {
                let rp_base: IDWriteRenderingParams = rp;
                self.dc_rt.SetTextRenderingParams(&rp_base);
            }
        }
    }

    fn measure_and_render(
        &mut self,
        display: &RenderedDisplay,
        max_taskbar_dip_h: f32,
        hwnd: Option<isize>,
    ) -> Option<(i32, i32, HDC)> {
        let _ = &self.d2d_factory;
        let fmt = self
            .create_text_format(&display.font_family, display.font_size_pt)
            .or_else(|| self.create_text_format("Segoe UI", display.font_size_pt))?;

        let has_net = display.show_net && display.net_sent.is_some() && display.net_recv.is_some();
        let has_cpu = display.cpu_label.is_some() && display.cpu_val.is_some();
        let has_ram = display.ram_label.is_some() && display.ram_val.is_some();
        let has_sys = has_cpu || has_ram;
        if !has_net && !has_sys {
            return None;
        }

        let (up_w, arrow_h1, up_layout) = if has_net {
            self.measure_text(&fmt, "↑")
        } else {
            (0.0, 0.0, None)
        };
        let (down_w, arrow_h2, down_layout) = if has_net {
            self.measure_text(&fmt, "↓")
        } else {
            (0.0, 0.0, None)
        };
        let arrow_w = up_w.max(down_w);

        let (sent_w, line_h1, sent_layout) = match display.net_sent.as_deref() {
            Some(s) if has_net => self.measure_text(&fmt, s),
            _ => (0.0, 0.0, None),
        };
        let (recv_w, line_h2, recv_layout) = match display.net_recv.as_deref() {
            Some(s) if has_net => self.measure_text(&fmt, s),
            _ => (0.0, 0.0, None),
        };
        let (net_sample_w, _, _) = if has_net {
            self.measure_text(&fmt, &display.net_sample)
        } else {
            (0.0, 0.0, None)
        };
        let net_arrow_gap = if has_net { 1.5 } else { 0.0 };
        let net_val_w = if has_net {
            sent_w.max(recv_w).max(net_sample_w)
        } else {
            0.0
        };
        let net_col_w = if has_net {
            arrow_w + net_arrow_gap + net_val_w
        } else {
            0.0
        };

        let (cpu_lw, cpu_lh, cpu_label_layout) = match display.cpu_label.as_deref() {
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (cpu_vw, line_h3, cpu_val_layout) = match display.cpu_val.as_deref() {
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (ram_lw, ram_lh, ram_label_layout) = match display.ram_label.as_deref() {
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (ram_vw, line_h4, ram_val_layout) = match display.ram_val.as_deref() {
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (sys_val_sample_w, _, _) = if has_sys {
            self.measure_text(&fmt, &display.sys_val_sample)
        } else {
            (0.0, 0.0, None)
        };
        let sys_label_w = if has_sys { cpu_lw.max(ram_lw) } else { 0.0 };
        let sys_label_gap = if has_sys { 2.5 } else { 0.0 };
        let sys_val_w = if has_sys {
            cpu_vw.max(ram_vw).max(sys_val_sample_w)
        } else {
            0.0
        };
        let sys_col_w = if has_sys {
            sys_label_w + sys_label_gap + sys_val_w
        } else {
            0.0
        };

        let row_h = line_h1
            .max(line_h2)
            .max(line_h3)
            .max(line_h4)
            .max(arrow_h1)
            .max(arrow_h2)
            .max(cpu_lh)
            .max(ram_lh)
            .max(12.0);
        let two_rows = has_net || (has_cpu && has_ram);
        let max_h = if max_taskbar_dip_h > 16.0 {
            max_taskbar_dip_h - 4.0
        } else {
            44.0
        };
        let mut row_gap = if two_rows { 1.0 } else { 0.0 };
        if two_rows && row_h * 2.0 + row_gap + 4.0 > max_h {
            row_gap = 0.0;
        }
        let col_gap = if has_net && has_sys { 7.0 } else { 0.0 };
        let pad_x = 6.0;
        let content_h = if two_rows {
            row_h * 2.0 + row_gap
        } else {
            row_h
        };
        let pad_y = ((max_h - content_h) * 0.5).clamp(1.0, 4.0);
        let dip_w = (pad_x * 2.0 + net_col_w + col_gap + sys_col_w).ceil();
        let dip_h = (content_h + pad_y * 2.0).clamp(22.0, max_h).ceil();

        let dpi_scale = (display.dpi.max(96) as f32) / 96.0;
        let snap_dip = |v: f32| -> f32 { (v * dpi_scale).round() / dpi_scale };
        let px_w = (dip_w * dpi_scale).round().max(1.0) as i32;
        let px_h = (dip_h * dpi_scale).round().max(1.0) as i32;

        let needs_alloc = self
            .surface
            .as_ref()
            .is_none_or(|s| s.width != px_w || s.height != px_h);
        if needs_alloc {
            self.surface = DibSurface::new(px_w, px_h);
        }
        let surface = self.surface.as_ref()?;

        let bind_rect = RECT {
            left: 0,
            top: 0,
            right: px_w,
            bottom: px_h,
        };
        let dpi_f = display.dpi.max(96) as f32;
        self.apply_text_rendering_params(hwnd, display.dpi.max(96));
        unsafe {
            self.dc_rt.SetDpi(dpi_f, dpi_f);
            self.dc_rt.BindDC(surface.hdc, &bind_rect).ok()?;
            self.dc_rt.BeginDraw();

            // 1/255 alpha makes the full widget box hit-testable for hover and
            // clicks while remaining visually indistinguishable from transparent.
            self.dc_rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0 / 255.0,
            }));

            if display.hovered {
                let hover_color = if display.light_taskbar {
                    D2D1_COLOR_F {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 0.06,
                    }
                } else {
                    D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.08,
                    }
                };
                if let Ok(hover_brush) = self.dc_rt.CreateSolidColorBrush(&hover_color, None) {
                    let rr = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: 0.5,
                            top: 0.5,
                            right: dip_w - 0.5,
                            bottom: dip_h - 0.5,
                        },
                        radiusX: 4.0,
                        radiusY: 4.0,
                    };
                    self.dc_rt.FillRoundedRectangle(&rr, &hover_brush);
                }
            }

            let text_color = D2D1_COLOR_F {
                r: f32::from(display.text_rgb[0]) / 255.0,
                g: f32::from(display.text_rgb[1]) / 255.0,
                b: f32::from(display.text_rgb[2]) / 255.0,
                a: 1.0,
            };
            let brush: ID2D1SolidColorBrush =
                match self.dc_rt.CreateSolidColorBrush(&text_color, None) {
                    Ok(b) => b,
                    Err(_) => {
                        let _ = self.dc_rt.EndDraw(None, None);
                        return None;
                    }
                };

            let top_y = snap_dip(((dip_h - content_h) * 0.5).max(0.0));
            let row2_y = snap_dip(top_y + row_h + row_gap);
            let single_y = snap_dip(((dip_h - row_h) * 0.5).max(0.0));

            if has_net {
                let arrow_x = snap_dip(pad_x);
                let val_x = snap_dip(pad_x + arrow_w + net_arrow_gap);
                if let Some(layout) = up_layout {
                    let _ = layout.SetMaxWidth(arrow_w);
                    let _ = layout.SetMaxHeight(row_h);
                    let _ = layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                    self.dc_rt.DrawTextLayout(
                        windows_numerics::Vector2 {
                            X: arrow_x,
                            Y: top_y,
                        },
                        &layout,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
                if let Some(layout) = sent_layout {
                    let _ = layout.SetMaxWidth(net_val_w);
                    let _ = layout.SetMaxHeight(row_h);
                    let _ = layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                    self.dc_rt.DrawTextLayout(
                        windows_numerics::Vector2 { X: val_x, Y: top_y },
                        &layout,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
                if let Some(layout) = down_layout {
                    let _ = layout.SetMaxWidth(arrow_w);
                    let _ = layout.SetMaxHeight(row_h);
                    let _ = layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                    self.dc_rt.DrawTextLayout(
                        windows_numerics::Vector2 {
                            X: arrow_x,
                            Y: row2_y,
                        },
                        &layout,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
                if let Some(layout) = recv_layout {
                    let _ = layout.SetMaxWidth(net_val_w);
                    let _ = layout.SetMaxHeight(row_h);
                    let _ = layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                    self.dc_rt.DrawTextLayout(
                        windows_numerics::Vector2 {
                            X: val_x,
                            Y: row2_y,
                        },
                        &layout,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            }

            if has_sys {
                let label_x = snap_dip(pad_x + net_col_w + if has_net { col_gap } else { 0.0 });
                let val_x = snap_dip(label_x + sys_label_w + sys_label_gap);
                let draw_sys_row =
                    |lbl: Option<IDWriteTextLayout>, val: Option<IDWriteTextLayout>, y: f32| {
                        if let Some(l) = lbl {
                            let _ = l.SetMaxWidth(sys_label_w);
                            let _ = l.SetMaxHeight(row_h);
                            let _ = l.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                            self.dc_rt.DrawTextLayout(
                                windows_numerics::Vector2 { X: label_x, Y: y },
                                &l,
                                &brush,
                                D2D1_DRAW_TEXT_OPTIONS_NONE,
                            );
                        }
                        if let Some(v) = val {
                            let _ = v.SetMaxWidth(sys_val_w);
                            let _ = v.SetMaxHeight(row_h);
                            let _ = v.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                            self.dc_rt.DrawTextLayout(
                                windows_numerics::Vector2 { X: val_x, Y: y },
                                &v,
                                &brush,
                                D2D1_DRAW_TEXT_OPTIONS_NONE,
                            );
                        }
                    };

                if has_cpu && has_ram {
                    draw_sys_row(cpu_label_layout, cpu_val_layout, top_y);
                    draw_sys_row(ram_label_layout, ram_val_layout, row2_y);
                } else if has_cpu {
                    draw_sys_row(cpu_label_layout, cpu_val_layout, single_y);
                } else if has_ram {
                    draw_sys_row(ram_label_layout, ram_val_layout, single_y);
                }
            }

            if self.dc_rt.EndDraw(None, None).is_err() {
                return None;
            }
            Some((px_w, px_h, surface.hdc))
        }
    }
}

struct WindowCtx {
    hwnd: HWND,
    owner_hwnd: isize,
    user_closing: bool,
    shared: Arc<Mutex<SharedState>>,
    callback: Arc<dyn Fn(Action) + Send + Sync>,
    renderer: Option<Renderer>,
    light_taskbar: bool,
    hovered: bool,
    tracking_mouse: bool,
    last_display: Option<RenderedDisplay>,
    last_size: (i32, i32),
    last_pos: Option<(i32, i32)>,
    last_visible: bool,
    msg_taskbar_created: u32,
}

impl WindowCtx {
    fn sync_owner(&mut self, tray_hwnd: HWND) {
        let raw = tray_hwnd.0 as isize;
        if raw != 0 && self.owner_hwnd != raw {
            // Setting Shell_TrayWnd as Win32 owner (not SetParent) keeps
            // independent message queues while guaranteeing Win32 keeps our
            // widget above the taskbar atomically on taskbar clicks.
            unsafe {
                SetWindowLongPtrW(self.hwnd, GWLP_HWNDPARENT, raw);
            }
            self.owner_hwnd = raw;
        }
    }

    fn refresh(&mut self, force_render: bool) {
        let (config, metrics) = {
            let guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (guard.config.clone(), guard.metrics)
        };

        if !config.enabled || (!config.show_cpu && !config.show_ram && !config.show_network) {
            self.set_visible(false);
            return;
        }

        let env = query_taskbar_env(self.hwnd);
        let Some(env) = env else {
            self.set_visible(false);
            return;
        };

        self.sync_owner(env.tray_hwnd);

        let display = build_display(&config, &metrics, self.light_taskbar, self.hovered, env.dpi);
        let size_changed_or_needed = force_render
            || self.last_display.as_ref() != Some(&display)
            || self.last_size == (0, 0);

        if size_changed_or_needed {
            let max_dip_h = env.taskbar_rect.height() as f32 / (env.dpi.max(96) as f32 / 96.0);
            if let Some(renderer) = self.renderer.as_mut()
                && let Some((w, h, src_dc)) =
                    renderer.measure_and_render(&display, max_dip_h, Some(self.hwnd.0 as isize))
            {
                self.last_display = Some(display);
                self.last_size = (w, h);

                let pos = compute_widget_position(
                    env.taskbar_rect,
                    env.tray_notify_rect,
                    env.monitor_rect,
                    (w, h),
                    env.dpi,
                );
                if let Some((x, y)) = pos {
                    self.present_layered(x, y, w, h, src_dc);
                    self.last_pos = Some((x, y));
                    let became_visible = self.set_visible(true);
                    ensure_just_above_taskbar(self.hwnd, env.tray_hwnd, became_visible);
                } else {
                    self.set_visible(false);
                }
                return;
            }
            self.set_visible(false);
            return;
        }

        if self.last_size != (0, 0) {
            if let Some((x, y)) = compute_widget_position(
                env.taskbar_rect,
                env.tray_notify_rect,
                env.monitor_rect,
                self.last_size,
                env.dpi,
            ) {
                if self.last_pos != Some((x, y)) {
                    unsafe {
                        let _ = SetWindowPos(
                            self.hwnd,
                            None,
                            x,
                            y,
                            self.last_size.0,
                            self.last_size.1,
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING,
                        );
                    }
                    self.last_pos = Some((x, y));
                }
                let became_visible = self.set_visible(true);
                ensure_just_above_taskbar(self.hwnd, env.tray_hwnd, became_visible);
            } else {
                self.set_visible(false);
            }
        }
    }

    fn present_layered(&self, x: i32, y: i32, w: i32, h: i32, src_dc: HDC) {
        let pt_dst = POINT { x, y };
        let size = SIZE { cx: w, cy: h };
        let pt_src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pt_dst),
                Some(&size),
                Some(src_dc),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    fn set_visible(&mut self, visible: bool) -> bool {
        if !visible {
            self.hovered = false;
            self.tracking_mouse = false;
        }
        if self.last_visible == visible {
            return false;
        }
        self.last_visible = visible;
        unsafe {
            let _ = ShowWindow(self.hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
        visible
    }

    fn show_context_menu(&mut self) {
        let config = {
            let guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.config.clone()
        };
        set_popup_menu_theme(config.menu_dark);
        let Ok(hmenu): Result<HMENU, _> = (unsafe { CreatePopupMenu() }) else {
            return;
        };
        let open_w: Vec<u16> = config.menu_open_label.encode_utf16().chain([0]).collect();
        let settings_w: Vec<u16> = config
            .menu_settings_label
            .encode_utf16()
            .chain([0])
            .collect();
        let exit_w: Vec<u16> = config.menu_exit_label.encode_utf16().chain([0]).collect();

        unsafe {
            let _ = InsertMenuW(
                hmenu,
                0,
                MF_BYPOSITION | MF_STRING,
                1,
                PCWSTR(open_w.as_ptr()),
            );
            let _ = InsertMenuW(
                hmenu,
                1,
                MF_BYPOSITION | MF_STRING,
                2,
                PCWSTR(settings_w.as_ptr()),
            );
            let _ = InsertMenuW(hmenu, 2, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
            let _ = InsertMenuW(
                hmenu,
                3,
                MF_BYPOSITION | MF_STRING,
                3,
                PCWSTR(exit_w.as_ptr()),
            );
            let _ = SetMenuDefaultItem(hmenu, 1, 0);

            let _ = SetForegroundWindow(self.hwnd);
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let cmd = TrackPopupMenuEx(
                hmenu,
                (TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN).0,
                pt.x,
                pt.y,
                self.hwnd,
                None,
            );
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(hmenu);

            if self.hovered {
                let mut cur = POINT::default();
                let mut wr = RECT::default();
                let inside = GetCursorPos(&mut cur).is_ok()
                    && GetWindowRect(self.hwnd, &mut wr).is_ok()
                    && cur.x >= wr.left
                    && cur.x < wr.right
                    && cur.y >= wr.top
                    && cur.y < wr.bottom;
                if !inside {
                    self.hovered = false;
                    self.tracking_mouse = false;
                    self.refresh(true);
                }
            }

            match cmd.0 {
                1 => (self.callback)(Action::OpenMain),
                2 => (self.callback)(Action::OpenSettings),
                3 => (self.callback)(Action::Exit),
                _ => {}
            }
        }
    }
}

fn build_display(
    config: &Config,
    metrics: &Metrics,
    light_taskbar: bool,
    hovered: bool,
    dpi: u32,
) -> RenderedDisplay {
    let (net_sent, net_recv, net_sample) = if config.show_network {
        let up = format_taskbar_rate(
            metrics.net_sent_bps,
            config.net_unit_bits,
            config.net_unit_mode,
        );
        let down = format_taskbar_rate(
            metrics.net_recv_bps,
            config.net_unit_bits,
            config.net_unit_mode,
        );
        let sample = match (config.net_unit_bits, config.net_unit_mode) {
            (false, TaskbarNetUnit::FixedKB) | (false, TaskbarNetUnit::Auto) => "999.9 KB/s",
            (false, TaskbarNetUnit::FixedMB) => "99.99 MB/s",
            (true, TaskbarNetUnit::FixedKB) | (true, TaskbarNetUnit::Auto) => "999.9 Kbps",
            (true, TaskbarNetUnit::FixedMB) => "99.99 Mbps",
        };
        (Some(up), Some(down), sample.to_string())
    } else {
        (None, None, String::new())
    };

    let cpu_label = config.show_cpu.then(|| "CPU".to_string());
    let cpu_val = config.show_cpu.then(|| format_taskbar_pct(metrics.cpu_pct));
    let ram_label = config.show_ram.then(|| "RAM".to_string());
    let ram_val = config.show_ram.then(|| format_taskbar_pct(metrics.ram_pct));

    let text_rgb = match config.color_mode {
        TaskbarColorMode::Auto => {
            if light_taskbar {
                [24, 24, 24]
            } else {
                [255, 255, 255]
            }
        }
        TaskbarColorMode::Custom => config.custom_color,
    };

    RenderedDisplay {
        show_net: config.show_network,
        net_sent,
        net_recv,
        net_sample,
        cpu_label,
        cpu_val,
        ram_label,
        ram_val,
        sys_val_sample: "100%".to_string(),
        font_family: config.font_family.clone(),
        font_size_pt: config.font_size,
        text_rgb,
        light_taskbar,
        hovered,
        dpi,
    }
}

struct TaskbarEnv {
    tray_hwnd: HWND,
    taskbar_rect: PixelRect,
    tray_notify_rect: Option<PixelRect>,
    monitor_rect: PixelRect,
    dpi: u32,
}

fn query_taskbar_env(own_hwnd: HWND) -> Option<TaskbarEnv> {
    unsafe {
        let tray_hwnd = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        if tray_hwnd.0.is_null() || !IsWindowVisible(tray_hwnd).as_bool() {
            return None;
        }
        let mut tray_r = RECT::default();
        GetWindowRect(tray_hwnd, &mut tray_r).ok()?;
        let taskbar_rect = PixelRect::from_win32(tray_r);
        if taskbar_rect.is_empty() {
            return None;
        }

        let monitor: HMONITOR = MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut mi).as_bool() {
            return None;
        }
        let monitor_rect = PixelRect::from_win32(mi.rcMonitor);

        if is_taskbar_autohidden(taskbar_rect, monitor_rect) {
            return None;
        }

        if is_fullscreen_covering_monitor(monitor, monitor_rect, tray_hwnd, own_hwnd) {
            return None;
        }

        let notify_hwnd = FindWindowExW(Some(tray_hwnd), None, w!("TrayNotifyWnd"), None).ok();
        let tray_notify_rect = notify_hwnd.and_then(|h| {
            if h.0.is_null() {
                return None;
            }
            let mut r = RECT::default();
            GetWindowRect(h, &mut r).ok()?;
            let pr = PixelRect::from_win32(r);
            (!pr.is_empty()).then_some(pr)
        });

        let dpi = GetDpiForWindow(tray_hwnd);
        Some(TaskbarEnv {
            tray_hwnd,
            taskbar_rect,
            tray_notify_rect,
            monitor_rect,
            dpi: if dpi == 0 { 96 } else { dpi },
        })
    }
}

fn is_taskbar_autohidden(taskbar_rect: PixelRect, monitor_rect: PixelRect) -> bool {
    let mut abd = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    let state = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut abd) };
    if (state as u32 & ABS_AUTOHIDE) == 0 {
        return false;
    }
    taskbar_slid_offscreen(taskbar_rect, monitor_rect)
}

fn taskbar_slid_offscreen(taskbar_rect: PixelRect, monitor_rect: PixelRect) -> bool {
    // When an auto-hide taskbar collapses, Windows leaves only a ~2 px sliver
    // inside the monitor edge.
    let vis_left = taskbar_rect.left.max(monitor_rect.left);
    let vis_top = taskbar_rect.top.max(monitor_rect.top);
    let vis_right = taskbar_rect.right.min(monitor_rect.right);
    let vis_bottom = taskbar_rect.bottom.min(monitor_rect.bottom);
    (vis_right - vis_left) <= 8 || (vis_bottom - vis_top) <= 8
}

unsafe fn is_fullscreen_covering_monitor(
    taskbar_monitor: HMONITOR,
    monitor_rect: PixelRect,
    tray_hwnd: HWND,
    own_hwnd: HWND,
) -> bool {
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() || fg == tray_hwnd || fg == own_hwnd {
        return false;
    }
    let desktop = unsafe { GetDesktopWindow() };
    let shell = unsafe { GetShellWindow() };
    if fg == desktop || fg == shell {
        return false;
    }
    if unsafe { !IsWindowVisible(fg).as_bool() || IsIconic(fg).as_bool() } {
        return false;
    }
    let fg_mon = unsafe { MonitorFromWindow(fg, MONITOR_DEFAULTTONULL) };
    if fg_mon != taskbar_monitor {
        return false;
    }
    if is_desktop_shell_class(fg) {
        return false;
    }
    if let Ok(state) = unsafe { SHQueryUserNotificationState() }
        && (state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE)
    {
        return true;
    }
    let mut fg_rect = RECT::default();
    if unsafe { GetWindowRect(fg, &mut fg_rect) }.is_err() {
        return false;
    }
    PixelRect::from_win32(fg_rect).covers(monitor_rect)
}

fn is_desktop_shell_class(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&buf[..len as usize]);
    matches!(
        name.as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

fn compute_widget_position(
    taskbar_rect: PixelRect,
    tray_notify_rect: Option<PixelRect>,
    _monitor_rect: PixelRect,
    widget_size: (i32, i32),
    dpi: u32,
) -> Option<(i32, i32)> {
    let (w, h) = widget_size;
    if w <= 0 || h <= 0 || taskbar_rect.is_empty() {
        return None;
    }
    let gap = ((6.0 * (dpi.max(96) as f32 / 96.0)).round() as i32).max(2);
    if taskbar_rect.width() >= taskbar_rect.height() {
        let x = match tray_notify_rect {
            Some(notify) => {
                let left_space = notify.left - taskbar_rect.left;
                let right_space = taskbar_rect.right - notify.right;
                if left_space >= right_space {
                    (notify.left - w - gap).max(taskbar_rect.left + gap)
                } else {
                    (notify.right + gap).min(taskbar_rect.right - w - gap)
                }
            }
            None => (taskbar_rect.right - w - gap).max(taskbar_rect.left + gap),
        };
        let y = taskbar_rect.top + (taskbar_rect.height() - h) / 2;
        Some((x, y))
    } else {
        let x = taskbar_rect.left + (taskbar_rect.width() - w) / 2;
        let y = match tray_notify_rect {
            Some(notify) => (notify.top - h - gap).max(taskbar_rect.top + gap),
            None => (taskbar_rect.bottom - h - gap).max(taskbar_rect.top + gap),
        };
        Some((x, y))
    }
}

fn is_window_above(target: HWND, anchor: HWND) -> bool {
    let mut cur = anchor;
    for _ in 0..128 {
        let Ok(prev) = (unsafe { GetWindow(cur, GW_HWNDPREV) }) else {
            return false;
        };
        if prev.0.is_null() {
            return false;
        }
        if prev == target {
            return true;
        }
        cur = prev;
    }
    false
}

/// Keeps `widget_hwnd` directly above `tray_hwnd`, never climbing in front of
/// popup menus, tray flyouts, or other programs' topmost windows.
fn ensure_just_above_taskbar(widget_hwnd: HWND, tray_hwnd: HWND, force_reposition: bool) {
    if !force_reposition && is_window_above(widget_hwnd, tray_hwnd) {
        return;
    }
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING;
    let mut cur = tray_hwnd;
    for _ in 0..64 {
        let Ok(prev) = (unsafe { GetWindow(cur, GW_HWNDPREV) }) else {
            break;
        };
        if prev.0.is_null() {
            break;
        }
        cur = prev;
        if prev == widget_hwnd {
            continue;
        }
        let visible = unsafe { IsWindowVisible(prev).as_bool() };
        let ex_style = unsafe { GetWindowLongPtrW(prev, GWL_EXSTYLE) } as u32;
        if visible
            && (ex_style & WS_EX_TOPMOST.0) != 0
            && unsafe { SetWindowPos(widget_hwnd, Some(prev), 0, 0, 0, 0, flags) }.is_ok()
            && is_window_above(widget_hwnd, tray_hwnd)
        {
            return;
        }
    }
    if !is_window_above(widget_hwnd, tray_hwnd) {
        unsafe {
            let _ = SetWindowPos(widget_hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
        }
    }
}

fn read_system_uses_light_theme() -> bool {
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut std::ffi::c_void),
            Some(&mut size),
        )
    };
    rc.is_ok() && data != 0
}

fn set_popup_menu_theme(dark: bool) {
    use windows::Win32::Foundation::{FARPROC, HMODULE};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::PCSTR;

    const FORCE_DARK: i32 = 2;
    const FORCE_LIGHT: i32 = 3;

    static UXTHEME: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    let exports = *UXTHEME.get_or_init(|| unsafe {
        let module: HMODULE = LoadLibraryW(w!("uxtheme.dll")).ok()?;
        let by_ordinal = |ordinal: u16| -> FARPROC {
            GetProcAddress(module, PCSTR(ordinal as usize as *const u8))
        };
        Some((by_ordinal(135)? as usize, by_ordinal(136)? as usize))
    });
    let Some((set_mode, flush)) = exports else {
        return;
    };
    unsafe {
        let set_preferred_app_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
        let flush_menu_themes: extern "system" fn() = std::mem::transmute(flush);
        set_preferred_app_mode(if dark { FORCE_DARK } else { FORCE_LIGHT });
        flush_menu_themes();
    }
}

unsafe extern "system" fn fg_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    let raw = LIVE_HWND.load(std::sync::atomic::Ordering::Acquire);
    if raw != 0 {
        let target = HWND(raw as *mut std::ffi::c_void);
        unsafe {
            let _ = PostMessageW(Some(target), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
        }
    }
}

fn register_window_class(hinstance: HINSTANCE) -> Option<PCWSTR> {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    let class_name = w!("TaskManTaskbarMonitorClass");
    let ok = *REGISTERED.get_or_init(|| unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&wc) != 0
    });
    ok.then_some(class_name)
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        let cs = unsafe {
            &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW)
        };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        return LRESULT(0);
    }

    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowCtx;
    if ptr.is_null() {
        return unsafe {
            windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
        };
    }
    let ctx = unsafe { &mut *ptr };

    if ctx.msg_taskbar_created != 0 && msg == ctx.msg_taskbar_created {
        ctx.refresh(true);
        return LRESULT(0);
    }

    match msg {
        WM_APP_REFRESH => {
            ctx.refresh(false);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            let light = read_system_uses_light_theme();
            if light != ctx.light_taskbar {
                ctx.light_taskbar = light;
                ctx.refresh(true);
            } else {
                ctx.refresh(false);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            ctx.refresh(true);
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            if !ctx.tracking_mouse {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                if unsafe { TrackMouseEvent(&mut tme) }.is_ok() {
                    ctx.tracking_mouse = true;
                }
            }
            if !ctx.hovered {
                ctx.hovered = true;
                ctx.refresh(true);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            ctx.tracking_mouse = false;
            if ctx.hovered {
                ctx.hovered = false;
                ctx.refresh(true);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            (ctx.callback)(Action::OpenMain);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            ctx.show_context_menu();
            LRESULT(0)
        }
        WM_CLOSE => {
            ctx.user_closing = true;
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, 0);
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe {
            windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
        },
    }
}

fn taskbar_thread_main(
    shared: Arc<Mutex<SharedState>>,
    state_cv: Arc<Condvar>,
    callback: Arc<dyn Fn(Action) + Send + Sync>,
) {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let hmodule = match unsafe { GetModuleHandleW(None) } {
        Ok(m) => HINSTANCE(m.0),
        Err(_) => {
            unsafe { CoUninitialize() };
            return;
        }
    };
    let Some(class_name) = register_window_class(hmodule) else {
        unsafe { CoUninitialize() };
        return;
    };

    let msg_taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };

    loop {
        let tray_owner = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }
            .ok()
            .filter(|h| !h.0.is_null());
        let Some(owner) = tray_owner else {
            let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.config.enabled {
                break;
            }
            let (g, _) = state_cv
                .wait_timeout(guard, std::time::Duration::from_millis(250))
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
            if !guard.config.enabled {
                break;
            }
            continue;
        };

        let renderer = Renderer::new();
        let mut ctx_box = Box::new(WindowCtx {
            hwnd: HWND::default(),
            owner_hwnd: owner.0 as isize,
            user_closing: false,
            shared: shared.clone(),
            callback: callback.clone(),
            renderer,
            light_taskbar: read_system_uses_light_theme(),
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            msg_taskbar_created,
        });

        let ctx_ptr: *mut WindowCtx = &mut *ctx_box;
        let hwnd = match unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                class_name,
                w!("TaskMan Taskbar Monitor"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                Some(owner),
                None,
                Some(hmodule),
                Some(ctx_ptr.cast()),
            )
        } {
            Ok(h) if !h.0.is_null() => h,
            _ => {
                break;
            }
        };

        ctx_box.hwnd = hwnd;
        LIVE_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::Release);

        // If `shutdown()` / `set_config(enabled = false)` raced before `LIVE_HWND`
        // was published, exit immediately instead of parking in `GetMessageW`.
        let still_enabled = {
            let guard = ctx_box.shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.config.enabled
        };
        if !still_enabled {
            LIVE_HWND.store(0, std::sync::atomic::Ordering::Release);
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, 0);
                let _ = DestroyWindow(hwnd);
            }
            drop(ctx_box);
            break;
        }

        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(fg_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };

        ctx_box.refresh(true);

        let mut msg = MSG::default();
        loop {
            let res = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if res.0 <= 0 {
                break;
            }
            let _ = unsafe { TranslateMessage(&msg) };
            unsafe { DispatchMessageW(&msg) };
        }

        LIVE_HWND.store(0, std::sync::atomic::Ordering::Release);
        if !hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(hook);
            }
        }
        let user_closing = ctx_box.user_closing;
        drop(ctx_box);

        let still_enabled = {
            let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.config.enabled
        };
        if user_closing || !still_enabled {
            break;
        }
    }

    unsafe { CoUninitialize() };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_left_of_tray_notify_on_ltr_taskbar() {
        let monitor = PixelRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let taskbar = PixelRect {
            left: 0,
            top: 1032,
            right: 1920,
            bottom: 1080,
        };
        let notify = PixelRect {
            left: 1700,
            top: 1032,
            right: 1920,
            bottom: 1080,
        };
        let pos = compute_widget_position(taskbar, Some(notify), monitor, (160, 36), 96).unwrap();
        assert_eq!(pos, (1700 - 160 - 6, 1032 + (48 - 36) / 2));
    }

    #[test]
    fn autohide_sliver_detection_hides_widget() {
        let monitor = PixelRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let collapsed = PixelRect {
            left: 0,
            top: 1078,
            right: 1920,
            bottom: 1080,
        };
        let expanded = PixelRect {
            left: 0,
            top: 1032,
            right: 1920,
            bottom: 1080,
        };
        assert!(taskbar_slid_offscreen(collapsed, monitor));
        assert!(!taskbar_slid_offscreen(expanded, monitor));
    }

    #[test]
    fn fullscreen_covers_check_matches_exact_or_oversized_windows() {
        let monitor = PixelRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let maximized = PixelRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1032,
        };
        let borderless_fs = PixelRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(!maximized.covers(monitor));
        assert!(borderless_fs.covers(monitor));
    }

    #[test]
    fn rendered_text_pixels_fall_inside_layered_bitmap_bounds() {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut renderer = Renderer::new().expect("Direct2D/DirectWrite renderer");
        let cfg = Config {
            enabled: true,
            show_cpu: true,
            show_ram: true,
            show_network: true,
            ..Config::default()
        };
        let metrics = Metrics {
            cpu_pct: Some(23.0),
            ram_pct: Some(48.0),
            net_sent_bps: Some(12_300.0),
            net_recv_bps: Some(456_700.0),
        };
        let display = build_display(&cfg, &metrics, false, false, 96);
        let (w, h, _hdc) = renderer
            .measure_and_render(&display, 48.0, None)
            .expect("rendered surface");
        assert!(w > 40 && h > 20, "unexpected widget size {w}x{h}");

        let pixels = renderer.surface.as_ref().unwrap().pixels();
        // Text color is white [255, 255, 255] while background clear is alpha 1/255.
        // Every quadrant (top-left upload, bottom-left download, top-right CPU,
        // bottom-right RAM) must contain rendered glyph pixels, while background
        // corner pixels remain nearly transparent.
        assert_eq!(
            &pixels[0..4],
            &[0, 0, 0, 1],
            "unexpected non-hovered corner pixel"
        );
        let mut quadrants = [0usize; 4];
        let half_w = (w / 2) as usize;
        let half_h = (h / 2) as usize;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let idx = (y * w as usize + x) * 4;
                if pixels[idx + 3] >= 64 && pixels[idx + 2] >= 64 {
                    let q = usize::from(y >= half_h) * 2 + usize::from(x >= half_w);
                    quadrants[q] += 1;
                }
            }
        }
        assert!(
            quadrants.iter().all(|&c| c >= 20),
            "expected >= 20 glyph pixels in all 4 quadrants of {w}x{h} bitmap, got {quadrants:?}"
        );

        let hi_dpi = build_display(&cfg, &metrics, false, false, 144);
        let (hw, hh, _hdc2) = renderer
            .measure_and_render(&hi_dpi, 48.0, None)
            .expect("hi-dpi rendered surface");
        assert!(hw > w && hh > h, "144 DPI should scale widget size");
        let hi_glyph_pixels = renderer
            .surface
            .as_ref()
            .unwrap()
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[3] >= 64 && px[2] >= 64)
            .count();
        assert!(
            hi_glyph_pixels >= 100,
            "expected >= 100 rendered glyph pixels at 144 DPI inside {hw}x{hh} bitmap, got {hi_glyph_pixels}"
        );
        unsafe { CoUninitialize() };
    }

    #[test]
    fn cpu_label_position_stays_fixed_across_digit_width_changes() {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut renderer = Renderer::new().expect("Direct2D/DirectWrite renderer");
        let cfg = Config {
            enabled: true,
            show_cpu: true,
            show_ram: false,
            show_network: true,
            ..Config::default()
        };
        let low_metrics = Metrics {
            cpu_pct: Some(1.0),
            ram_pct: None,
            net_sent_bps: Some(0.0),
            net_recv_bps: Some(0.0),
        };
        let high_metrics = Metrics {
            cpu_pct: Some(100.0),
            ram_pct: None,
            net_sent_bps: Some(0.0),
            net_recv_bps: Some(0.0),
        };

        let (w1, h1, _) = renderer
            .measure_and_render(
                &build_display(&cfg, &low_metrics, false, false, 96),
                48.0,
                None,
            )
            .expect("low load render");
        let snap1 = renderer.surface.as_ref().unwrap().pixels().to_vec();

        let (w2, h2, _) = renderer
            .measure_and_render(
                &build_display(&cfg, &high_metrics, false, false, 96),
                48.0,
                None,
            )
            .expect("high load render");
        let snap2 = renderer.surface.as_ref().unwrap().pixels().to_vec();

        assert_eq!((w1, h1), (w2, h2));

        // Locate the first lit glyph pixel inside the right-hand CPU column (x >= w/2):
        // that edge belongs to the "CPU" label and must not shift when "1%" becomes "100%".
        let first_cpu_x = |buf: &[u8]| -> usize {
            (w1 as usize / 2..w1 as usize)
                .find(|&x| (0..h1 as usize).any(|y| buf[(y * w1 as usize + x) * 4 + 3] >= 64))
                .expect("CPU label pixels present")
        };
        assert_eq!(first_cpu_x(&snap1), first_cpu_x(&snap2));

        // Ensure "1%" starts right after the "CPU"/"RAM" label slot without the wide
        // right-aligned padding void: total CPU-label-to-1%-end span stays compact.
        let last_cpu_x = (w1 as usize / 2..w1 as usize)
            .rfind(|&x| (0..h1 as usize).any(|y| snap1[(y * w1 as usize + x) * 4 + 3] >= 64))
            .expect("CPU percent pixels present");
        let cpu_span = last_cpu_x - first_cpu_x(&snap1);
        assert!(
            cpu_span <= 42,
            "CPU label + value should stay compact without right-align padding, got {cpu_span}px"
        );
        unsafe { CoUninitialize() };
    }

    #[test]
    fn widget_stays_above_taskbar_and_below_menus_and_topmost_windows() {
        let hinstance = unsafe { GetModuleHandleW(None).unwrap() };
        let class_name = register_window_class(HINSTANCE(hinstance.0)).unwrap();

        let make_win = |ex_style, owner: Option<HWND>| unsafe {
            CreateWindowExW(
                ex_style,
                class_name,
                w!("z-order test"),
                WS_POPUP,
                -200,
                -200,
                10,
                10,
                owner,
                None,
                Some(HINSTANCE(hinstance.0)),
                None,
            )
            .unwrap()
        };

        let mock_tray = make_win(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST, None);
        let mock_menu = make_win(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST, None);
        let widget = make_win(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            Some(mock_tray),
        );

        unsafe {
            let _ = ShowWindow(mock_tray, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(widget, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(mock_menu, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                mock_menu,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        // First force_reposition (simulating unhide while another topmost popup/window exists),
        // then steady-state ticks (force_reposition = false): both must keep widget above
        // mock_tray while keeping mock_menu on top of widget.
        ensure_just_above_taskbar(widget, mock_tray, true);
        assert!(is_window_above(widget, mock_tray));
        assert!(is_window_above(mock_menu, widget));

        ensure_just_above_taskbar(widget, mock_tray, false);
        assert!(is_window_above(widget, mock_tray));
        assert!(is_window_above(mock_menu, widget));

        unsafe {
            let _ = DestroyWindow(widget);
            let _ = DestroyWindow(mock_menu);
            let _ = DestroyWindow(mock_tray);
        }
    }
}
