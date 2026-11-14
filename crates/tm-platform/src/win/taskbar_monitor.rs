//! Windows taskbar performance monitor (`UpdateLayeredWindow` + software Direct2D/DirectWrite).
//!
//! Renders CPU, RAM, and network rates directly into a 32-bit premultiplied
//! ARGB DIB section and presents it via `UpdateLayeredWindow` (`ULW_ALPHA`)
//! automatically positioned inside the largest non-overlapping open region on
//! the primary Windows taskbar (preferring the gap beside `TrayNotifyWnd`).
//!
//! Design invariants (avoiding DWM / Explorer interference):
//! - **Zero screen reads:** never calls `GetDC(NULL)`, `GetPixel`, or `BitBlt`
//!   from the screen DC (which causes DWM screen-capture fences). Taskbar
//!   light/dark contrast comes from `SystemUsesLightTheme` + `WM_SETTINGCHANGE`.
//! - **Zero cross-process `SetParent`:** runs as an independent `WS_EX_LAYERED |
//!   WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` popup owned by `Shell_TrayWnd` on its
//!   own dedicated thread, never attaching to Explorer's Taskbar thread.
//! - **Zero GPU swapchain:** uses `D2D1_RENDER_TARGET_TYPE_SOFTWARE` into a
//!   cached DIB section and only calls `UpdateLayeredWindow` when rendered
//!   content, hover state, or DPI/geometry actually changes.
//! - **Collision-free automagic placement:** queries visible taskbar buttons,
//!   Widgets, and tray controls out-of-process via a dedicated MTA UI
//!   Automation scanner thread (`FindAllBuildCache` with `AutomationElementMode_None`
//!   and an 80 ms transaction timeout, triggered only on shell/tray layout
//!   events), relocating across open taskbar gaps, compacting when tight, and
//!   hiding cleanly if the taskbar is 100% full.
//! - **Fullscreen-safe:** hides automatically when a fullscreen window covers
//!   the taskbar monitor (preserving DWM Independent Flip / MPO and never
//!   floating over games/videos) or when an auto-hide taskbar slides away.

use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_TRANSITIONS_FORCEDISABLED, DwmGetWindowAttribute, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, HMONITOR, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromWindow, SelectObject,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoCreateInstance,
    CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    AutomationElementMode_None, CUIAutomation, CUIAutomation8, HWINEVENTHOOK, IUIAutomation,
    IUIAutomation2, IUIAutomationCacheRequest, IUIAutomationCondition, SetWinEventHook,
    TreeScope_Descendants, TreeScope_Element, UIA_BoundingRectanglePropertyId,
    UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
    UIA_ControlTypePropertyId, UIA_EditControlTypeId, UIA_IsOffscreenPropertyId,
    UIA_ListItemControlTypeId, UIA_MenuItemControlTypeId, UIA_SplitButtonControlTypeId,
    UnhookWinEvent,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{
    ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    SHAppBarMessage, SHQueryUserNotificationState,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CreatePopupMenu, CreateWindowExW, DeregisterShellHookWindow, DestroyMenu,
    DestroyWindow, DispatchMessageW, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, EnumWindows, FindWindowExW, FindWindowW,
    GW_HWNDPREV, GWL_EXSTYLE, GWLP_HWNDPARENT, GWLP_USERDATA, GetClassNameW, GetCursorPos,
    GetDesktopWindow, GetForegroundWindow, GetLayeredWindowAttributes, GetMessageW, GetShellWindow,
    GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, HMENU,
    HSHELL_WINDOWACTIVATED, HSHELL_WINDOWCREATED, HSHELL_WINDOWDESTROYED, HSHELL_WINDOWREPLACED,
    HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, InsertMenuW, IsChild, IsIconic, IsWindowVisible,
    KillTimer, LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE,
    MF_BYPOSITION, MF_SEPARATOR, MF_STRING, MSG, OBJID_WINDOW, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterShellHookWindow, RegisterWindowMessageW, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING, SWP_NOSIZE, SWP_NOZORDER,
    SetForegroundWindow, SetMenuDefaultItem, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, ULW_ALPHA,
    UpdateLayeredWindow, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_CLOSE,
    WM_CREATE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_NULL, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BOOL, Interface, PCWSTR, w};

use crate::taskbar_monitor::{Action, Config, Metrics};

const WM_APP_REFRESH: u32 = WM_APP + 10;
const WM_MOUSELEAVE: u32 = 0x02A3;
const UIA_SETTLE_TIMER_ID: usize = 1;
const UIA_SETTLE_DELAY_MS: u32 = 300;
// `HSHELL_HIGHBIT | HSHELL_WINDOWACTIVATED`: Windows Shell notification fired
// when a fullscreen / "rude" app enters or leaves fullscreen focus.
const HSHELL_RUDEAPPACTIVATED_MSG: u32 = 0x8004;

/// Global HWNDs and coalescing flags so WinEvent hooks (which carry no
/// user-data pointer) can wake `"tm-taskbar"` immediately without flooding its
/// message queue.
static LIVE_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static LIVE_FG_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static LIVE_TRAY_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static REFRESH_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAYOUT_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn post_refresh() {
    let raw = LIVE_HWND.load(std::sync::atomic::Ordering::Acquire);
    if raw != 0 && !REFRESH_PENDING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        let target = HWND(raw as *mut std::ffi::c_void);
        let posted =
            unsafe { PostMessageW(Some(target), WM_APP_REFRESH, WPARAM(0), LPARAM(0)).is_ok() };
        if !posted {
            REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

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
        post_refresh();
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

    fn intersects(self, other: Self) -> bool {
        self.right > other.left
            && self.left < other.right
            && self.bottom > other.top
            && self.top < other.bottom
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
    compact: bool,
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

    fn measure_layout(
        &self,
        display: &RenderedDisplay,
        max_taskbar_dip_h: f32,
    ) -> Option<MeasuredLayout> {
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
        let net_arrow_gap = if has_net {
            if display.compact { 1.0 } else { 1.5 }
        } else {
            0.0
        };
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
        let sys_label_gap = if has_sys {
            if display.compact { 1.5 } else { 2.5 }
        } else {
            0.0
        };
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
        let col_gap = if has_net && has_sys {
            if display.compact { 4.0 } else { 7.0 }
        } else {
            0.0
        };
        let pad_x = if display.compact { 3.0 } else { 6.0 };
        let content_h = if two_rows {
            row_h * 2.0 + row_gap
        } else {
            row_h
        };
        let pad_y = ((max_h - content_h) * 0.5).clamp(1.0, 4.0);
        let dip_w = (pad_x * 2.0 + net_col_w + col_gap + sys_col_w).ceil();
        let dip_h = (content_h + pad_y * 2.0).clamp(22.0, max_h).ceil();

        let dpi_scale = (display.dpi.max(96) as f32) / 96.0;
        let px_w = (dip_w * dpi_scale).round().max(1.0) as i32;
        let px_h = (dip_h * dpi_scale).round().max(1.0) as i32;

        Some(MeasuredLayout {
            px_w,
            px_h,
            dip_w,
            dip_h,
            dpi_scale,
            pad_x,
            row_h,
            row_gap,
            content_h,
            col_gap,
            has_net,
            has_sys,
            has_cpu,
            has_ram,
            arrow_w,
            net_arrow_gap,
            net_val_w,
            net_col_w,
            sys_label_w,
            sys_label_gap,
            sys_val_w,
            up_layout,
            down_layout,
            sent_layout,
            recv_layout,
            cpu_label_layout,
            cpu_val_layout,
            ram_label_layout,
            ram_val_layout,
        })
    }

    fn render_layout(
        &mut self,
        display: &RenderedDisplay,
        layout: MeasuredLayout,
        hwnd: Option<isize>,
    ) -> Option<(i32, i32, HDC)> {
        let px_w = layout.px_w;
        let px_h = layout.px_h;
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
        let snap_dip = |v: f32| -> f32 { (v * layout.dpi_scale).round() / layout.dpi_scale };
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
                            right: layout.dip_w - 0.5,
                            bottom: layout.dip_h - 0.5,
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

            let draw_text = |tl: Option<IDWriteTextLayout>, x: f32, y: f32, max_w: f32| {
                if let Some(tl) = tl {
                    let _ = tl.SetMaxWidth(max_w);
                    let _ = tl.SetMaxHeight(layout.row_h);
                    let _ = tl.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                    self.dc_rt.DrawTextLayout(
                        windows_numerics::Vector2 { X: x, Y: y },
                        &tl,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            };

            let top_y = snap_dip(((layout.dip_h - layout.content_h) * 0.5).max(0.0));
            let row2_y = snap_dip(top_y + layout.row_h + layout.row_gap);
            let single_y = snap_dip(((layout.dip_h - layout.row_h) * 0.5).max(0.0));

            if layout.has_net {
                let arrow_x = snap_dip(layout.pad_x);
                let val_x = snap_dip(layout.pad_x + layout.arrow_w + layout.net_arrow_gap);
                draw_text(layout.up_layout, arrow_x, top_y, layout.arrow_w);
                draw_text(layout.sent_layout, val_x, top_y, layout.net_val_w);
                draw_text(layout.down_layout, arrow_x, row2_y, layout.arrow_w);
                draw_text(layout.recv_layout, val_x, row2_y, layout.net_val_w);
            }

            if layout.has_sys {
                let label_x = snap_dip(
                    layout.pad_x
                        + layout.net_col_w
                        + if layout.has_net { layout.col_gap } else { 0.0 },
                );
                let val_x = snap_dip(label_x + layout.sys_label_w + layout.sys_label_gap);
                let draw_sys_row =
                    |lbl: Option<IDWriteTextLayout>, val: Option<IDWriteTextLayout>, y: f32| {
                        draw_text(lbl, label_x, y, layout.sys_label_w);
                        draw_text(val, val_x, y, layout.sys_val_w);
                    };

                if layout.has_cpu && layout.has_ram {
                    draw_sys_row(layout.cpu_label_layout, layout.cpu_val_layout, top_y);
                    draw_sys_row(layout.ram_label_layout, layout.ram_val_layout, row2_y);
                } else if layout.has_cpu {
                    draw_sys_row(layout.cpu_label_layout, layout.cpu_val_layout, single_y);
                } else if layout.has_ram {
                    draw_sys_row(layout.ram_label_layout, layout.ram_val_layout, single_y);
                }
            }

            if self.dc_rt.EndDraw(None, None).is_err() {
                return None;
            }
            Some((px_w, px_h, surface.hdc))
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn measure_and_render(
        &mut self,
        display: &RenderedDisplay,
        max_taskbar_dip_h: f32,
        hwnd: Option<isize>,
    ) -> Option<(i32, i32, HDC)> {
        let layout = self.measure_layout(display, max_taskbar_dip_h)?;
        self.render_layout(display, layout, hwnd)
    }
}

struct MeasuredLayout {
    px_w: i32,
    px_h: i32,
    dip_w: f32,
    dip_h: f32,
    dpi_scale: f32,
    pad_x: f32,
    row_h: f32,
    row_gap: f32,
    content_h: f32,
    col_gap: f32,
    has_net: bool,
    has_sys: bool,
    has_cpu: bool,
    has_ram: bool,
    arrow_w: f32,
    net_arrow_gap: f32,
    net_val_w: f32,
    net_col_w: f32,
    sys_label_w: f32,
    sys_label_gap: f32,
    sys_val_w: f32,
    up_layout: Option<IDWriteTextLayout>,
    down_layout: Option<IDWriteTextLayout>,
    sent_layout: Option<IDWriteTextLayout>,
    recv_layout: Option<IDWriteTextLayout>,
    cpu_label_layout: Option<IDWriteTextLayout>,
    cpu_val_layout: Option<IDWriteTextLayout>,
    ram_label_layout: Option<IDWriteTextLayout>,
    ram_val_layout: Option<IDWriteTextLayout>,
}

struct WindowCtx {
    hwnd: HWND,
    owner_hwnd: isize,
    user_closing: bool,
    shared: Arc<Mutex<SharedState>>,
    callback: Arc<dyn Fn(Action) + Send + Sync>,
    renderer: Option<Renderer>,
    uia_scanner: Arc<UiaScanner>,
    light_taskbar: bool,
    hovered: bool,
    tracking_mouse: bool,
    last_display: Option<RenderedDisplay>,
    last_size: (i32, i32),
    last_pos: Option<(i32, i32)>,
    last_visible: bool,
    last_anchor_slot: Option<AnchorSlot>,
    last_compact: bool,
    last_env_geom: Option<(isize, PixelRect, Option<PixelRect>, u32)>,
    pending_uia_scan: bool,
    msg_taskbar_created: u32,
    msg_shell_hook: u32,
    tracked_fg_tid: u32,
    fg_loc_hook: HWINEVENTHOOK,
    tracked_tray_tid: u32,
    tray_hook: HWINEVENTHOOK,
}

impl Drop for WindowCtx {
    fn drop(&mut self) {
        self.unhook_dynamic_hooks();
    }
}

impl WindowCtx {
    fn mark_layout_dirty(&mut self, arm_settle: bool) {
        self.pending_uia_scan = true;
        if arm_settle && !self.hwnd.0.is_null() {
            unsafe {
                let _ = SetTimer(
                    Some(self.hwnd),
                    UIA_SETTLE_TIMER_ID,
                    UIA_SETTLE_DELAY_MS,
                    None,
                );
            }
        }
    }

    fn unhook_dynamic_hooks(&mut self) {
        LIVE_FG_HWND.store(0, std::sync::atomic::Ordering::Release);
        LIVE_TRAY_HWND.store(0, std::sync::atomic::Ordering::Release);
        if !self.fg_loc_hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.fg_loc_hook);
            }
            self.fg_loc_hook = HWINEVENTHOOK::default();
        }
        if !self.tray_hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.tray_hook);
            }
            self.tray_hook = HWINEVENTHOOK::default();
        }
        self.tracked_fg_tid = 0;
        self.tracked_tray_tid = 0;
    }

    fn sync_foreground_hook(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
        LIVE_FG_HWND.store(fg.0 as isize, std::sync::atomic::Ordering::Release);
        let mut pid = 0u32;
        let tid = if fg.0.is_null() {
            0
        } else {
            unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) }
        };
        let target_tid = if tid != 0 && pid != std::process::id() {
            tid
        } else {
            0
        };
        if target_tid == self.tracked_fg_tid {
            return;
        }
        if !self.fg_loc_hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.fg_loc_hook);
            }
            self.fg_loc_hook = HWINEVENTHOOK::default();
        }
        self.tracked_fg_tid = target_tid;
        if target_tid != 0 {
            self.fg_loc_hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    None,
                    Some(fg_location_event_proc),
                    0,
                    target_tid,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
        }
    }

    fn sync_tray_hook(&mut self, tray_hwnd: HWND) {
        if tray_hwnd.0.is_null() {
            return;
        }
        LIVE_TRAY_HWND.store(tray_hwnd.0 as isize, std::sync::atomic::Ordering::Release);
        let tid = unsafe { GetWindowThreadProcessId(tray_hwnd, None) };
        if tid == self.tracked_tray_tid {
            return;
        }
        if !self.tray_hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.tray_hook);
            }
            self.tray_hook = HWINEVENTHOOK::default();
        }
        self.tracked_tray_tid = tid;
        if tid != 0 {
            self.tray_hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_SHOW,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    None,
                    Some(tray_event_proc),
                    0,
                    tid,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
        }
    }

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

        // Track the active foreground window's thread even when hidden during
        // fullscreen so exiting fullscreen (F11 / Esc / resize) wakes immediately.
        self.sync_foreground_hook();
        if self.owner_hwnd != 0 {
            self.sync_tray_hook(HWND(self.owner_hwnd as *mut std::ffi::c_void));
        }

        let env = query_taskbar_env(self.hwnd);
        let Some(env) = env else {
            self.last_env_geom = None;
            self.pending_uia_scan = true;
            self.set_visible(false);
            return;
        };

        self.sync_owner(env.tray_hwnd);
        self.sync_tray_hook(env.tray_hwnd);

        let tray_raw = env.tray_hwnd.0 as isize;
        let geom = (tray_raw, env.taskbar_rect, env.tray_notify_rect, env.dpi);
        let geom_changed = self.last_env_geom != Some(geom);
        if geom_changed {
            self.last_env_geom = Some(geom);
            self.pending_uia_scan = true;
        }

        let uia_occupied = self.uia_scanner.snapshot_for(tray_raw, env.taskbar_rect);
        if self.pending_uia_scan || uia_occupied.is_none() {
            self.pending_uia_scan = false;
            self.uia_scanner.request_scan(tray_raw, env.taskbar_rect);
        }

        let max_dip_h = env.taskbar_rect.height() as f32 / (env.dpi.max(96) as f32 / 96.0);
        let normal_display = build_display(
            &config,
            &metrics,
            self.light_taskbar,
            self.hovered,
            env.dpi,
            false,
        );
        let Some(renderer) = self.renderer.as_mut() else {
            self.set_visible(false);
            return;
        };
        let Some(normal_layout) = renderer.measure_layout(&normal_display, max_dip_h) else {
            self.set_visible(false);
            return;
        };
        let normal_size = (normal_layout.px_w, normal_layout.px_h);

        let same_footprint = self.last_display.as_ref().is_some_and(|d| {
            d.show_net == normal_display.show_net
                && d.cpu_val.is_some() == normal_display.cpu_val.is_some()
                && d.ram_val.is_some() == normal_display.ram_val.is_some()
                && d.net_sample == normal_display.net_sample
                && d.font_family == normal_display.font_family
                && (d.font_size_pt - normal_display.font_size_pt).abs() < 0.01
                && d.dpi == normal_display.dpi
        });
        let normal_placement = solve_taskbar_placement(
            env.taskbar_rect,
            env.tray_notify_rect,
            uia_occupied.as_deref(),
            normal_size,
            env.dpi,
            self.last_anchor_slot,
            self.last_compact && same_footprint && !force_render,
        );

        let (display, layout, placement) = if let Some(placement) = normal_placement {
            (normal_display, normal_layout, placement)
        } else {
            let compact_display = build_display(
                &config,
                &metrics,
                self.light_taskbar,
                self.hovered,
                env.dpi,
                true,
            );
            let Some(compact_layout) = renderer.measure_layout(&compact_display, max_dip_h) else {
                self.set_visible(false);
                return;
            };
            let compact_size = (compact_layout.px_w, compact_layout.px_h);
            let Some(placement) = solve_taskbar_placement(
                env.taskbar_rect,
                env.tray_notify_rect,
                uia_occupied.as_deref(),
                compact_size,
                env.dpi,
                self.last_anchor_slot,
                false,
            ) else {
                // Taskbar is completely full even in compact form: step out of the way
                // instead of overlapping taskbar buttons or stealing their clicks.
                self.set_visible(false);
                return;
            };
            (compact_display, compact_layout, placement)
        };

        self.last_anchor_slot = Some(placement.slot);
        self.last_compact = display.compact;
        let (w, h) = (layout.px_w, layout.px_h);
        let (x, y) = placement.pos;
        let needs_render = force_render
            || self.last_display.as_ref() != Some(&display)
            || self.last_size != (w, h);

        if needs_render {
            if let Some((rw, rh, src_dc)) =
                renderer.render_layout(&display, layout, Some(self.hwnd.0 as isize))
            {
                self.last_display = Some(display);
                self.last_size = (rw, rh);
                self.present_layered(x, y, rw, rh, src_dc);
                self.last_pos = Some((x, y));
                ensure_just_above_taskbar(self.hwnd, env.tray_hwnd, !self.last_visible);
                let _ = self.set_visible(true);
            } else {
                self.set_visible(false);
            }
            return;
        }

        if self.last_pos != Some((x, y)) {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    x,
                    y,
                    w,
                    h,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING,
                );
            }
            self.last_pos = Some((x, y));
        }
        ensure_just_above_taskbar(self.hwnd, env.tray_hwnd, !self.last_visible);
        let _ = self.set_visible(true);
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
    compact: bool,
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

    let cpu_label = config
        .show_cpu
        .then(|| if compact { "C" } else { "CPU" }.to_string());
    let cpu_val = config.show_cpu.then(|| format_taskbar_pct(metrics.cpu_pct));
    let ram_label = config
        .show_ram
        .then(|| if compact { "R" } else { "RAM" }.to_string());
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
        compact,
    }
}

struct TaskbarEnv {
    tray_hwnd: HWND,
    taskbar_rect: PixelRect,
    tray_notify_rect: Option<PixelRect>,
    dpi: u32,
}

fn query_taskbar_env(own_hwnd: HWND) -> Option<TaskbarEnv> {
    unsafe {
        let tray_hwnd = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        if tray_hwnd.0.is_null()
            || !IsWindowVisible(tray_hwnd).as_bool()
            || is_window_cloaked(tray_hwnd)
        {
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

        if is_fullscreen_covering_monitor(monitor, monitor_rect, tray_hwnd, own_hwnd)
            || is_fullscreen_window_above_taskbar(monitor_rect, tray_hwnd, own_hwnd)
        {
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
    if is_window_cloaked(fg) {
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

fn is_window_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .is_ok();
    ok && cloaked != 0
}

fn is_transparent_overlay(hwnd: HWND) -> bool {
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    if (ex_style & WS_EX_LAYERED.0) == 0 {
        return false;
    }
    if (ex_style & WS_EX_TRANSPARENT.0) != 0 {
        return true;
    }
    let mut alpha = 255u8;
    let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
    unsafe { GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)) }.is_ok()
        && (flags & LWA_ALPHA) != LAYERED_WINDOW_ATTRIBUTES_FLAGS(0)
        && alpha == 0
}

struct AboveTaskbarScan {
    tray_hwnd: HWND,
    own_hwnd: HWND,
    desktop_hwnd: HWND,
    shell_hwnd: HWND,
    monitor_rect: PixelRect,
    covered: bool,
    saw_tray: bool,
}

unsafe extern "system" fn above_taskbar_enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = unsafe { &mut *(lparam.0 as *mut AboveTaskbarScan) };
    if hwnd == scan.tray_hwnd {
        scan.saw_tray = true;
        return BOOL(0);
    }
    if hwnd == scan.own_hwnd || hwnd == scan.desktop_hwnd || hwnd == scan.shell_hwnd {
        return BOOL(1);
    }
    if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
        return BOOL(1);
    }
    if is_desktop_shell_class(hwnd) || is_window_cloaked(hwnd) || is_transparent_overlay(hwnd) {
        return BOOL(1);
    }
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok()
        && PixelRect::from_win32(r).covers(scan.monitor_rect)
    {
        scan.covered = true;
    }
    BOOL(1)
}

/// Detects fullscreen windows still sitting above `tray_hwnd` in Z-order even
/// when they no longer hold foreground focus (e.g. while holding Alt+Tab over
/// a fullscreen game so the task switcher dialog is foreground above both).
fn is_fullscreen_window_above_taskbar(
    monitor_rect: PixelRect,
    tray_hwnd: HWND,
    own_hwnd: HWND,
) -> bool {
    let mut scan = AboveTaskbarScan {
        tray_hwnd,
        own_hwnd,
        desktop_hwnd: unsafe { GetDesktopWindow() },
        shell_hwnd: unsafe { GetShellWindow() },
        monitor_rect,
        covered: false,
        saw_tray: false,
    };
    unsafe {
        let _ = EnumWindows(
            Some(above_taskbar_enum_cb),
            LPARAM(&mut scan as *mut AboveTaskbarScan as isize),
        );
    }
    scan.saw_tray && scan.covered
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnchorSlot {
    /// Positioned beside `TrayNotifyWnd` (stepped back past any adjacent XAML
    /// controls such as Widgets or Copilot).
    TrayAdjacent,
    /// Positioned in an alternate open taskbar gap, anchored toward the
    /// taskbar edge so centered icons growing/shrinking keep it stationary.
    AlternateGap { anchor_to_end: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlacementResult {
    pos: (i32, i32),
    slot: AnchorSlot,
}

#[derive(Clone, Copy, Debug)]
struct Gap1D {
    avail_start: i32,
    avail_end: i32,
    tray_adjacent: bool,
    dist_to_tray: i32,
}

impl Gap1D {
    fn avail_len(self) -> i32 {
        self.avail_end - self.avail_start
    }
}

/// Computes collision-free widget coordinates inside `taskbar_rect` from out-of-process
/// UIA obstacle rectangles plus `tray_notify_rect`, preferring the tray-adjacent slot
/// and falling back to an alternate open gap before reporting `None` (hide).
/// When `uia_occupied` is `None` or empty, falls back to classic Win32 tray placement.
fn solve_taskbar_placement(
    taskbar_rect: PixelRect,
    tray_notify_rect: Option<PixelRect>,
    uia_occupied: Option<&[PixelRect]>,
    widget_size: (i32, i32),
    dpi: u32,
    prev_slot: Option<AnchorSlot>,
    prefer_keep_compact: bool,
) -> Option<PlacementResult> {
    let (w, h) = widget_size;
    if w <= 0 || h <= 0 || taskbar_rect.is_empty() {
        return None;
    }

    let Some(uia_rects) = uia_occupied.filter(|s| !s.is_empty()) else {
        // Safety fallback: if UIA has not reported elements yet or Explorer UIA is
        // unavailable/empty, keep the widget visible using TrayNotifyWnd geometry.
        return compute_widget_position(
            taskbar_rect,
            tray_notify_rect,
            taskbar_rect,
            widget_size,
            dpi,
        )
        .map(|pos| PlacementResult {
            pos,
            slot: AnchorSlot::TrayAdjacent,
        });
    };

    let scale = dpi.max(96) as f32 / 96.0;
    let gap = ((6.0 * scale).round() as i32).max(2);
    let hysteresis = ((16.0 * scale).round() as i32).max(8);
    let tray_adj_limit = (320.0 * scale).round() as i32;

    let horizontal = taskbar_rect.width() >= taskbar_rect.height();
    let project = |r: PixelRect| -> (i32, i32) {
        if horizontal {
            (r.left, r.right)
        } else {
            (r.top, r.bottom)
        }
    };
    let (bar_start, bar_end) = project(taskbar_rect);
    let (cross_pos, widget_len) = if horizontal {
        (taskbar_rect.top + (taskbar_rect.height() - h) / 2, w)
    } else {
        (taskbar_rect.left + (taskbar_rect.width() - w) / 2, h)
    };
    let bar_len = bar_end - bar_start;
    let req_len = if prefer_keep_compact {
        widget_len + hysteresis
    } else {
        widget_len
    };
    if bar_len < req_len + 2 * gap {
        return None;
    }

    let notify_span = tray_notify_rect
        .filter(|n| n.intersects(taskbar_rect))
        .map(project)
        .map(|(s, e)| (s.max(bar_start), e.min(bar_end)))
        .filter(|(s, e)| e > s);

    // Filter out bogus container-sized UIA elements spanning most of the taskbar
    // (e.g., XAML root composition bridges or toolbar frames).
    let mut intervals: Vec<(i32, i32)> = uia_rects
        .iter()
        .copied()
        .filter(|r| !r.is_empty() && r.intersects(taskbar_rect))
        .filter_map(|r| {
            let (raw_s, raw_e) = project(r);
            let span = raw_e - raw_s;
            if bar_len >= 400 && span >= bar_len * 60 / 100 {
                return None;
            }
            let s = raw_s.max(bar_start);
            let e = raw_e.min(bar_end);
            (e > s).then_some((s, e))
        })
        .collect();
    if let Some(ns) = notify_span {
        intervals.push(ns);
    }
    if intervals.is_empty() {
        return compute_widget_position(
            taskbar_rect,
            tray_notify_rect,
            taskbar_rect,
            widget_size,
            dpi,
        )
        .map(|pos| PlacementResult {
            pos,
            slot: AnchorSlot::TrayAdjacent,
        });
    }

    intervals.sort_unstable();
    let mut merged: Vec<(i32, i32)> = Vec::with_capacity(intervals.len());
    for (s, e) in intervals {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
            continue;
        }
        merged.push((s, e));
    }

    let (tray_at_end, tray_edge) = match notify_span {
        Some((ns, ne)) => {
            if ns - bar_start >= bar_end - ne {
                (true, ns)
            } else {
                (false, ne)
            }
        }
        None => (true, bar_end),
    };

    let mut gaps = Vec::new();
    let mut push_gap = |g_start: i32, g_end: i32| {
        let avail_start = g_start + gap;
        let avail_end = g_end - gap;
        if avail_end - avail_start < req_len {
            return;
        }
        let (on_main_side, dist_to_tray) = if tray_at_end {
            (g_end <= tray_edge + 2, (tray_edge - g_end).max(0))
        } else {
            (g_start >= tray_edge - 2, (g_start - tray_edge).max(0))
        };
        if !on_main_side {
            return;
        }
        gaps.push(Gap1D {
            avail_start,
            avail_end,
            tray_adjacent: dist_to_tray <= tray_adj_limit,
            dist_to_tray,
        });
    };

    let mut cursor = bar_start;
    for (s, e) in merged {
        if s > cursor {
            push_gap(cursor, s);
        }
        cursor = cursor.max(e);
    }
    if bar_end > cursor {
        push_gap(cursor, bar_end);
    }

    if gaps.is_empty() {
        return None;
    }

    let best_tray = gaps
        .iter()
        .copied()
        .filter(|g| g.tray_adjacent)
        .min_by_key(|g| (g.dist_to_tray, std::cmp::Reverse(g.avail_len())));
    let best_alt = gaps
        .iter()
        .copied()
        .filter(|g| !g.tray_adjacent)
        .max_by_key(|g| g.avail_len());

    let was_alt = matches!(prev_slot, Some(AnchorSlot::AlternateGap { .. }));
    let (chosen, slot, anchor_to_end) = match (best_tray, best_alt) {
        (Some(tray_g), Some(alt_g)) => {
            // Require extra margin before leaving an alternate open gap back to
            // the tray side so hover/animations at the threshold never oscillate.
            if was_alt && tray_g.avail_len() < widget_len + hysteresis {
                let end = (bar_end - alt_g.avail_end) < (alt_g.avail_start - bar_start);
                (alt_g, AnchorSlot::AlternateGap { anchor_to_end: end }, end)
            } else {
                (tray_g, AnchorSlot::TrayAdjacent, tray_at_end)
            }
        }
        (Some(tray_g), None) => (tray_g, AnchorSlot::TrayAdjacent, tray_at_end),
        (None, Some(alt_g)) => {
            let end = (bar_end - alt_g.avail_end) < (alt_g.avail_start - bar_start);
            (alt_g, AnchorSlot::AlternateGap { anchor_to_end: end }, end)
        }
        (None, None) => return None,
    };

    let primary = if anchor_to_end {
        chosen.avail_end - widget_len
    } else {
        chosen.avail_start
    };
    let pos = if horizontal {
        (primary, cross_pos)
    } else {
        (cross_pos, primary)
    };
    Some(PlacementResult { pos, slot })
}

struct ComMtaGuard {
    initialized: bool,
}

impl ComMtaGuard {
    fn enter() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            initialized: hr.is_ok(),
        }
    }
}

impl Drop for ComMtaGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

struct UiaSession {
    uia: IUIAutomation,
    condition: IUIAutomationCondition,
    cache_request: IUIAutomationCacheRequest,
}

impl UiaSession {
    fn new() -> windows::core::Result<Self> {
        let uia: IUIAutomation = unsafe {
            if let Ok(uia2) =
                CoCreateInstance::<_, IUIAutomation2>(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
            {
                let _ = uia2.SetConnectionTimeout(50);
                let _ = uia2.SetTransactionTimeout(80);
                uia2.cast()?
            } else {
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?
            }
        };

        let control_view = unsafe { uia.ControlViewCondition()? };
        let not_offscreen = unsafe {
            let v = VARIANT::from(false);
            uia.CreatePropertyCondition(UIA_IsOffscreenPropertyId, &v)?
        };
        let control_types = [
            UIA_ButtonControlTypeId,
            UIA_SplitButtonControlTypeId,
            UIA_CheckBoxControlTypeId,
            UIA_MenuItemControlTypeId,
            UIA_EditControlTypeId,
            UIA_ComboBoxControlTypeId,
            UIA_ListItemControlTypeId,
        ];
        let mut type_conds = Vec::with_capacity(control_types.len());
        for ct in control_types {
            let v = VARIANT::from(ct.0);
            let c = unsafe { uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &v)? };
            type_conds.push(Some(c));
        }
        let or_types = unsafe { uia.CreateOrConditionFromNativeArray(&type_conds)? };
        let visible_controls = unsafe { uia.CreateAndCondition(&control_view, &not_offscreen)? };
        let condition = unsafe { uia.CreateAndCondition(&visible_controls, &or_types)? };

        let cache_request = unsafe { uia.CreateCacheRequest()? };
        unsafe {
            cache_request.AddProperty(UIA_BoundingRectanglePropertyId)?;
            cache_request.SetTreeScope(TreeScope_Element)?;
            cache_request.SetAutomationElementMode(AutomationElementMode_None)?;
        }

        Ok(Self {
            uia,
            condition,
            cache_request,
        })
    }

    fn scan_tray(&self, tray_raw: isize) -> windows::core::Result<Vec<PixelRect>> {
        let tray_hwnd = HWND(tray_raw as *mut std::ffi::c_void);
        if tray_raw == 0
            || !unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tray_hwnd)).as_bool()
            }
        {
            return Ok(Vec::new());
        }
        let root = unsafe { self.uia.ElementFromHandle(tray_hwnd)? };
        let found = unsafe {
            root.FindAllBuildCache(TreeScope_Descendants, &self.condition, &self.cache_request)?
        };
        let len = unsafe { found.Length()? };
        let mut rects = Vec::with_capacity(len.max(0) as usize);
        for i in 0..len {
            let Ok(elem) = (unsafe { found.GetElement(i) }) else {
                continue;
            };
            let Ok(rc) = (unsafe { elem.CachedBoundingRectangle() }) else {
                continue;
            };
            let px = PixelRect::from_win32(rc);
            if !px.is_empty() {
                rects.push(px);
            }
        }
        rects.sort_unstable_by_key(|r| (r.left, r.top, r.right, r.bottom));
        rects.dedup();
        Ok(rects)
    }
}

struct UiaCache {
    tray_hwnd: isize,
    taskbar_rect: PixelRect,
    rects: Option<Vec<PixelRect>>,
}

struct UiaShared {
    pending: Option<(isize, PixelRect)>,
    cache: UiaCache,
    shutdown: bool,
}

/// Scans `Shell_TrayWnd` UI Automation bounding rectangles on a dedicated MTA
/// thread so cross-process UIA COM calls never pump messages or block `"tm-taskbar"`.
struct UiaScanner {
    state: Arc<(Mutex<UiaShared>, Condvar)>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl UiaScanner {
    fn start() -> Self {
        let state = Arc::new((
            Mutex::new(UiaShared {
                pending: None,
                cache: UiaCache {
                    tray_hwnd: 0,
                    taskbar_rect: PixelRect {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    },
                    rects: None,
                },
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let worker_state = state.clone();
        let handle = std::thread::Builder::new()
            .name("tm-taskbar-uia".to_string())
            .spawn(move || uia_worker_main(worker_state))
            .ok();
        Self {
            state,
            worker: Mutex::new(handle),
        }
    }

    fn snapshot_for(&self, tray_raw: isize, taskbar_rect: PixelRect) -> Option<Vec<PixelRect>> {
        let guard = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
        if guard.cache.tray_hwnd == tray_raw && guard.cache.taskbar_rect == taskbar_rect {
            guard.cache.rects.clone()
        } else {
            None
        }
    }

    fn request_scan(&self, tray_raw: isize, taskbar_rect: PixelRect) {
        if tray_raw == 0 || taskbar_rect.is_empty() {
            return;
        }
        let mut guard = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
        if guard.shutdown {
            return;
        }
        guard.pending = Some((tray_raw, taskbar_rect));
        self.state.1.notify_one();
    }

    fn shutdown(&self) {
        {
            let mut guard = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
            guard.shutdown = true;
            guard.pending = None;
            self.state.1.notify_all();
        }
        if let Ok(mut slot) = self.worker.lock()
            && let Some(handle) = slot.take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for UiaScanner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn uia_worker_main(state: Arc<(Mutex<UiaShared>, Condvar)>) {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _com = ComMtaGuard::enter();
    let min_interval = Duration::from_millis(80);
    let mut session: Option<UiaSession> = None;
    let mut last_scan: Option<Instant> = None;

    loop {
        let target = {
            let mut guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if guard.shutdown {
                    return;
                }
                if let Some(target) = guard.pending {
                    if let Some(prev_at) = last_scan {
                        let elapsed = prev_at.elapsed();
                        if elapsed < min_interval {
                            let (g, _) = state
                                .1
                                .wait_timeout(guard, min_interval - elapsed)
                                .unwrap_or_else(|e| e.into_inner());
                            guard = g;
                            continue;
                        }
                    }
                    guard.pending = None;
                    break target;
                }
                guard = state.1.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
        };

        last_scan = Some(Instant::now());
        if session.is_none() {
            session = UiaSession::new().ok();
        }
        let result = match session.as_ref() {
            Some(s) => match s.scan_tray(target.0) {
                Ok(rects) => Ok(rects),
                Err(_) => {
                    session = None;
                    Err(())
                }
            },
            None => Err(()),
        };

        let mut changed = false;
        if let Ok(rects) = result {
            let next_rects = (!rects.is_empty()).then_some(rects);
            let mut guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.shutdown {
                let same_target =
                    guard.cache.tray_hwnd == target.0 && guard.cache.taskbar_rect == target.1;
                if next_rects.is_some() || !same_target {
                    changed = !same_target || guard.cache.rects != next_rects;
                    guard.cache = UiaCache {
                        tray_hwnd: target.0,
                        taskbar_rect: target.1,
                        rects: next_rects,
                    };
                }
            }
        }
        if changed {
            post_refresh();
        }
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
/// popup menus, tray flyouts, or other programs' topmost windows, and never
/// remaining pinned as `TOPMOST` if Explorer itself was demoted behind a
/// fullscreen window.
fn ensure_just_above_taskbar(widget_hwnd: HWND, tray_hwnd: HWND, force_reposition: bool) {
    let tray_topmost =
        (unsafe { GetWindowLongPtrW(tray_hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0) != 0;
    let widget_topmost =
        (unsafe { GetWindowLongPtrW(widget_hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0) != 0;
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING;

    if !tray_topmost {
        if widget_topmost {
            unsafe {
                let _ = SetWindowPos(widget_hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
            }
        }
        // Keep the non-topmost widget directly above `tray_hwnd` so demoting
        // from `HWND_TOPMOST` via `HWND_NOTOPMOST` never places it above a
        // window that is currently covering the demoted taskbar.
        if let Ok(prev) = unsafe { GetWindow(tray_hwnd, GW_HWNDPREV) }
            && !prev.0.is_null()
            && prev != widget_hwnd
        {
            unsafe {
                let _ = SetWindowPos(widget_hwnd, Some(prev), 0, 0, 0, 0, flags);
            }
        }
        return;
    }

    if !force_reposition && widget_topmost && is_window_above(widget_hwnd, tray_hwnd) {
        return;
    }
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
    if !is_window_above(widget_hwnd, tray_hwnd) || !widget_topmost {
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
    event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if event == EVENT_SYSTEM_MOVESIZEEND {
        LAYOUT_DIRTY.store(true, std::sync::atomic::Ordering::Release);
    }
    post_refresh();
}

unsafe extern "system" fn fg_location_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    // OBJID_WINDOW (0) + CHILDID_SELF (0): ignore carets, scrollbars, and child DOM controls.
    if id_object == OBJID_WINDOW.0
        && id_child == 0
        && !hwnd.0.is_null()
        && hwnd.0 as isize == LIVE_FG_HWND.load(std::sync::atomic::Ordering::Acquire)
    {
        post_refresh();
    }
}

unsafe extern "system" fn tray_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    let tray = LIVE_TRAY_HWND.load(std::sync::atomic::Ordering::Acquire);
    if tray == 0 || hwnd.0.is_null() {
        return;
    }
    let tray_hwnd = HWND(tray as *mut std::ffi::c_void);
    let is_tray_or_child = hwnd.0 as isize == tray || unsafe { IsChild(tray_hwnd, hwnd).as_bool() };
    let relevant_event = event == EVENT_OBJECT_REORDER
        || (matches!(
            event,
            EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE | EVENT_OBJECT_LOCATIONCHANGE
        ) && id_object == OBJID_WINDOW.0
            && id_child == 0);
    if is_tray_or_child && relevant_event {
        LAYOUT_DIRTY.store(true, std::sync::atomic::Ordering::Release);
        post_refresh();
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
        ctx.mark_layout_dirty(true);
        ctx.refresh(true);
        return LRESULT(0);
    }

    if ctx.msg_shell_hook != 0 && msg == ctx.msg_shell_hook {
        match wparam.0 as u32 {
            HSHELL_WINDOWCREATED | HSHELL_WINDOWDESTROYED | HSHELL_WINDOWREPLACED => {
                ctx.mark_layout_dirty(true);
                ctx.refresh(false);
            }
            HSHELL_WINDOWACTIVATED | HSHELL_RUDEAPPACTIVATED_MSG => {
                ctx.refresh(false);
            }
            _ => {}
        }
        return LRESULT(0);
    }

    match msg {
        WM_APP_REFRESH => {
            REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
            if LAYOUT_DIRTY.swap(false, std::sync::atomic::Ordering::AcqRel) {
                ctx.mark_layout_dirty(true);
            }
            ctx.refresh(false);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == UIA_SETTLE_TIMER_ID {
                unsafe {
                    let _ = KillTimer(Some(hwnd), UIA_SETTLE_TIMER_ID);
                }
                ctx.mark_layout_dirty(false);
                ctx.refresh(false);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            ctx.mark_layout_dirty(true);
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
            ctx.mark_layout_dirty(true);
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
                let _ = KillTimer(Some(hwnd), UIA_SETTLE_TIMER_ID);
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
    let msg_shell_hook = unsafe { RegisterWindowMessageW(w!("SHELLHOOK")) };
    LAYOUT_DIRTY.store(false, std::sync::atomic::Ordering::Release);
    let uia_scanner = Arc::new(UiaScanner::start());

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
            uia_scanner: uia_scanner.clone(),
            light_taskbar: read_system_uses_light_theme(),
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_anchor_slot: None,
            last_compact: false,
            last_env_geom: None,
            pending_uia_scan: true,
            msg_taskbar_created,
            msg_shell_hook,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
        });

        let ctx_ptr: *mut WindowCtx = &mut *ctx_box;
        let hwnd = match unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
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

        // Disable DWM show/hide transition animations so entering or exiting
        // fullscreen applies on the very next compositor vblank without fade lag.
        let disable_transitions = windows::core::BOOL::from(true);
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_TRANSITIONS_FORCEDISABLED,
                (&disable_transitions as *const windows::core::BOOL).cast(),
                std::mem::size_of::<windows::core::BOOL>() as u32,
            );
        }

        ctx_box.hwnd = hwnd;
        REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
        LIVE_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::Release);

        // If `shutdown()` / `set_config(enabled = false)` raced before `LIVE_HWND`
        // was published, exit immediately instead of parking in `GetMessageW`.
        let still_enabled = {
            let guard = ctx_box.shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.config.enabled
        };
        if !still_enabled {
            LIVE_HWND.store(0, std::sync::atomic::Ordering::Release);
            REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, 0);
                let _ = DestroyWindow(hwnd);
            }
            drop(ctx_box);
            break;
        }

        let shell_hook_registered = unsafe { RegisterShellHookWindow(hwnd).as_bool() };
        let install_sys_hook = |min_ev: u32, max_ev: u32| unsafe {
            SetWinEventHook(
                min_ev,
                max_ev,
                None,
                Some(fg_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        let fg_hook = install_sys_hook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND);
        let movesize_hook = install_sys_hook(EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND);
        let minimize_hook = install_sys_hook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND);

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
        REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
        for hook in [fg_hook, movesize_hook, minimize_hook] {
            if !hook.is_invalid() {
                unsafe {
                    let _ = UnhookWinEvent(hook);
                }
            }
        }
        if shell_hook_registered {
            unsafe {
                let _ = DeregisterShellHookWindow(hwnd);
            }
        }
        ctx_box.unhook_dynamic_hooks();
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

    uia_scanner.shutdown();
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
        let display = build_display(&cfg, &metrics, false, false, 96, false);
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

        let hi_dpi = build_display(&cfg, &metrics, false, false, 144, false);
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
                &build_display(&cfg, &low_metrics, false, false, 96, false),
                48.0,
                None,
            )
            .expect("low load render");
        let snap1 = renderer.surface.as_ref().unwrap().pixels().to_vec();

        let (w2, h2, _) = renderer
            .measure_and_render(
                &build_display(&cfg, &high_metrics, false, false, 96, false),
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

    #[test]
    fn widget_demotes_with_taskbar_behind_fullscreen_and_restores_on_exit() {
        let hinstance = unsafe { GetModuleHandleW(None).unwrap() };
        let class_name = register_window_class(HINSTANCE(hinstance.0)).unwrap();

        let make_win = |ex_style, owner: Option<HWND>| unsafe {
            CreateWindowExW(
                ex_style,
                class_name,
                w!("fullscreen z-order test"),
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
        let mock_fullscreen = make_win(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, None);
        let mock_menu = make_win(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST, None);
        // Owned by mock_tray without explicitly forcing WS_EX_TOPMOST on creation.
        let widget = make_win(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            Some(mock_tray),
        );

        unsafe {
            let _ = ShowWindow(mock_fullscreen, SW_SHOWNOACTIVATE);
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

        ensure_just_above_taskbar(widget, mock_tray, true);
        assert!(is_window_above(widget, mock_tray));
        assert!(is_window_above(mock_menu, widget));

        // Simulate Explorer demoting Shell_TrayWnd behind a normal-band fullscreen window:
        // because widget is owned by mock_tray, Win32 demotes it behind mock_fullscreen
        // synchronously before ensure_just_above_taskbar even runs.
        unsafe {
            let _ = SetWindowPos(
                mock_tray,
                Some(windows::Win32::UI::WindowsAndMessaging::HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        assert!(
            is_window_above(mock_fullscreen, widget),
            "owned widget must drop behind fullscreen window atomically when taskbar demotes"
        );
        ensure_just_above_taskbar(widget, mock_tray, true);
        let widget_ex = unsafe { GetWindowLongPtrW(widget, GWL_EXSTYLE) } as u32;
        assert_eq!(
            widget_ex & WS_EX_TOPMOST.0,
            0,
            "widget must not stay TOPMOST when taskbar itself was demoted"
        );
        assert!(
            is_window_above(mock_fullscreen, widget),
            "widget must sit behind normal-band fullscreen window when taskbar is demoted"
        );

        // Simulate exiting fullscreen: Shell_TrayWnd re-enters TOPMOST band.
        unsafe {
            let _ = SetWindowPos(
                mock_tray,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
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
        ensure_just_above_taskbar(widget, mock_tray, true);
        assert!(is_window_above(widget, mock_tray));
        assert!(is_window_above(mock_menu, widget));

        unsafe {
            let _ = DestroyWindow(widget);
            let _ = DestroyWindow(mock_menu);
            let _ = DestroyWindow(mock_fullscreen);
            let _ = DestroyWindow(mock_tray);
        }
    }

    #[test]
    fn fullscreen_above_taskbar_stays_detected_under_alt_tab_and_ignores_cloaked_or_transparent() {
        use windows::Win32::Graphics::Dwm::DWMWA_CLOAK;
        use windows::Win32::UI::WindowsAndMessaging::HWND_TOP;

        let hinstance = unsafe { GetModuleHandleW(None).unwrap() };
        let class_name = register_window_class(HINSTANCE(hinstance.0)).unwrap();
        let mock_monitor = rect(-10_000, -10_000, -9_900, -9_900);

        let make_win = |ex_style, x: i32, y: i32, w: i32, h: i32| unsafe {
            CreateWindowExW(
                ex_style,
                class_name,
                w!("above-taskbar visibility test"),
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(HINSTANCE(hinstance.0)),
                None,
            )
            .unwrap()
        };

        let mock_tray = make_win(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            -10_000,
            -9_940,
            100,
            40,
        );
        let mock_fullscreen = make_win(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            -10_000,
            -10_000,
            100,
            100,
        );
        // Smaller centered overlay simulating the Windows Alt+Tab switcher above both.
        let mock_switcher = make_win(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            -9_980,
            -9_980,
            60,
            60,
        );

        unsafe {
            let _ = ShowWindow(mock_fullscreen, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(mock_tray, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(mock_switcher, SW_SHOWNOACTIVATE);
            // Normal desktop: tray is TOPMOST above fullscreen/normal windows; switcher alone
            // does not cover the monitor.
            let _ = SetWindowPos(
                mock_tray,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = SetWindowPos(
                mock_switcher,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        assert!(
            !is_fullscreen_window_above_taskbar(mock_monitor, mock_tray, HWND::default()),
            "taskbar above normal windows with Alt+Tab dialog on top must remain visible"
        );

        // Fullscreen game active and user holds Alt+Tab: taskbar stays demoted behind
        // mock_fullscreen while mock_switcher sits on top of both.
        unsafe {
            let _ = SetWindowPos(
                mock_tray,
                Some(windows::Win32::UI::WindowsAndMessaging::HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = SetWindowPos(
                mock_fullscreen,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        assert!(
            is_fullscreen_window_above_taskbar(mock_monitor, mock_tray, HWND::default()),
            "fullscreen window behind Alt+Tab switcher and above demoted taskbar must hide widget"
        );

        // If the fullscreen window is click-through transparent, it must not suppress the widget.
        let orig_ex = unsafe { GetWindowLongPtrW(mock_fullscreen, GWL_EXSTYLE) };
        unsafe {
            SetWindowLongPtrW(
                mock_fullscreen,
                GWL_EXSTYLE,
                orig_ex | (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0) as isize,
            );
        }
        assert!(
            !is_fullscreen_window_above_taskbar(mock_monitor, mock_tray, HWND::default()),
            "click-through transparent overlay must not suppress taskbar monitor"
        );
        unsafe {
            SetWindowLongPtrW(mock_fullscreen, GWL_EXSTYLE, orig_ex);
        }
        assert!(is_fullscreen_window_above_taskbar(
            mock_monitor,
            mock_tray,
            HWND::default()
        ));

        // If the fullscreen window is cloaked (e.g. another virtual desktop), ignore it.
        let cloak_on = BOOL::from(true);
        if unsafe {
            DwmSetWindowAttribute(
                mock_fullscreen,
                DWMWA_CLOAK,
                (&cloak_on as *const BOOL).cast(),
                std::mem::size_of::<BOOL>() as u32,
            )
        }
        .is_ok()
        {
            assert!(
                !is_fullscreen_window_above_taskbar(mock_monitor, mock_tray, HWND::default()),
                "cloaked fullscreen window must not suppress taskbar monitor"
            );
            let cloak_off = BOOL::from(false);
            let _ = unsafe {
                DwmSetWindowAttribute(
                    mock_fullscreen,
                    DWMWA_CLOAK,
                    (&cloak_off as *const BOOL).cast(),
                    std::mem::size_of::<BOOL>() as u32,
                )
            };
        }

        // Hiding the fullscreen window restores visibility.
        unsafe {
            let _ = ShowWindow(mock_fullscreen, SW_HIDE);
        }
        assert!(!is_fullscreen_window_above_taskbar(
            mock_monitor,
            mock_tray,
            HWND::default()
        ));

        unsafe {
            let _ = DestroyWindow(mock_switcher);
            let _ = DestroyWindow(mock_fullscreen);
            let _ = DestroyWindow(mock_tray);
        }
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> PixelRect {
        PixelRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn placement_steps_past_xaml_tray_adjacent_controls() {
        let taskbar = rect(0, 1032, 1920, 1080);
        let notify = rect(1700, 1032, 1920, 1080);
        // Win11 left-aligned: Start+Search+Apps occupy 0..480, Copilot sits right
        // next to TrayNotifyWnd at 1640..1694.
        let uia = [rect(0, 1032, 480, 1080), rect(1640, 1036, 1694, 1076)];
        let res = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&uia),
            (150, 36),
            96,
            None,
            false,
        )
        .expect("should fit left of Copilot");
        assert_eq!(res.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(res.pos.0, 1640 - 150 - 6);
        assert!(res.pos.0 + 150 <= 1640);
    }

    #[test]
    fn placement_relocates_to_open_centered_left_gap_with_hysteresis() {
        let taskbar = rect(0, 1032, 1920, 1080);
        let notify = rect(1700, 1032, 1920, 1080);
        let widgets = rect(8, 1036, 150, 1076);
        let copilot = rect(1640, 1036, 1694, 1076);

        // Right tray gap is 1640 - 1560 = 80px (too narrow for 150px widget),
        // while left open gap between Widgets (150) and centered Start+Apps (600) is 450px.
        let packed_right = [widgets, rect(600, 1034, 1560, 1078), copilot];
        let alt = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&packed_right),
            (150, 36),
            96,
            Some(AnchorSlot::TrayAdjacent),
            false,
        )
        .expect("should relocate to open left gap");
        assert_eq!(
            alt.slot,
            AnchorSlot::AlternateGap {
                anchor_to_end: false
            }
        );
        assert_eq!(alt.pos.0, 150 + 6);

        // Reopening right gap just above 150 + 2*6 = 162px (avail = 154px, < 150 + 16 hysteresis)
        // must keep widget anchored on the left instead of oscillating.
        let barely_open_right = [widgets, rect(600, 1034, 1474, 1078), copilot];
        let stayed = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&barely_open_right),
            (150, 36),
            96,
            Some(alt.slot),
            false,
        )
        .expect("should stay anchored in left gap within hysteresis band");
        assert_eq!(stayed.slot, alt.slot);
        assert_eq!(stayed.pos.0, 150 + 6);

        // Once right gap clears hysteresis (1640 - 1440 - 12 = 188px >= 166px), move back right.
        let wide_open_right = [widgets, rect(600, 1034, 1440, 1078), copilot];
        let returned = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&wide_open_right),
            (150, 36),
            96,
            Some(stayed.slot),
            false,
        )
        .expect("should return beside tray once ample space opens");
        assert_eq!(returned.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(returned.pos.0, 1640 - 150 - 6);
    }

    #[test]
    fn placement_adapts_compact_then_hides_when_packed_and_falls_back_without_uia() {
        let taskbar = rect(0, 1032, 1920, 1080);
        let notify = rect(1700, 1032, 1920, 1080);
        let chunk_items = |start: i32, end: i32| -> Vec<PixelRect> {
            let mut out = Vec::new();
            let mut x = start;
            while x < end {
                let next = (x + 120).min(end);
                out.push(rect(x, 1034, next, 1078));
                x = next;
            }
            out
        };

        // Only 142px raw -> 130px usable: normal (150px) does not fit, compact (118px) does.
        let tight = chunk_items(0, 1558);
        assert!(
            solve_taskbar_placement(
                taskbar,
                Some(notify),
                Some(&tight),
                (150, 36),
                96,
                None,
                false,
            )
            .is_none()
        );
        let compact_res = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&tight),
            (118, 36),
            96,
            None,
            false,
        )
        .expect("compact width should fit");
        assert_eq!(compact_res.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(compact_res.pos, (1700 - 118 - 6, 1032 + (48 - 36) / 2));

        // At 152px usable (>= 150 but < 150+16), prefer_keep_compact keeps compact active.
        let borderline = chunk_items(0, 1536);
        assert!(
            solve_taskbar_placement(
                taskbar,
                Some(notify),
                Some(&borderline),
                (150, 36),
                96,
                Some(AnchorSlot::TrayAdjacent),
                true,
            )
            .is_none()
        );

        // Packed end-to-end: clean None -> caller hides widget cleanly.
        let completely_full = chunk_items(0, 1695);
        assert!(
            solve_taskbar_placement(
                taskbar,
                Some(notify),
                Some(&completely_full),
                (118, 36),
                96,
                None,
                false,
            )
            .is_none()
        );

        // Missing/empty UIA falls back safely to TrayNotifyWnd placement instead of hiding.
        for empty_uia in [None, Some(&[][..])] {
            let fallback = solve_taskbar_placement(
                taskbar,
                Some(notify),
                empty_uia,
                (150, 36),
                96,
                None,
                false,
            )
            .expect("missing UIA must never hide");
            assert_eq!(fallback.pos, (1700 - 150 - 6, 1032 + (48 - 36) / 2));
        }
    }

    #[test]
    fn placement_ignores_container_rects_and_handles_negative_rtl_vertical() {
        // Negative secondary/left monitor coords + bogus container-wide XAML bridge (>= 60%).
        let neg_bar = rect(-1920, 1032, 0, 1080);
        let neg_notify = rect(-220, 1032, 0, 1080);
        let uia_with_container = [
            rect(-1920, 1032, -220, 1080), // 1700/1920 = 88% bogus container bridge
            rect(-1920, 1032, -1600, 1080),
        ];
        let neg_res = solve_taskbar_placement(
            neg_bar,
            Some(neg_notify),
            Some(&uia_with_container),
            (150, 36),
            96,
            None,
            false,
        )
        .expect("bogus >=60% container rect must be ignored");
        assert_eq!(neg_res.pos, (-220 - 150 - 6, 1032 + (48 - 36) / 2));

        // RTL taskbar: tray on the left edge [0..220], apps on [900..1920].
        let rtl_bar = rect(0, 1032, 1920, 1080);
        let rtl_notify = rect(0, 1032, 220, 1080);
        let rtl_uia = [rect(900, 1032, 1920, 1080)];
        let rtl_res = solve_taskbar_placement(
            rtl_bar,
            Some(rtl_notify),
            Some(&rtl_uia),
            (150, 36),
            96,
            None,
            false,
        )
        .expect("RTL tray-adjacent gap should fit");
        assert_eq!(rtl_res.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(rtl_res.pos.0, 220 + 6);

        // Vertical taskbar on right edge [1840..1920] x [0..1080], tray at bottom [880..1080].
        let vert_bar = rect(1840, 0, 1920, 1080);
        let vert_notify = rect(1840, 880, 1920, 1080);
        let vert_uia = [rect(1840, 0, 1920, 420), rect(1840, 830, 1920, 880)];
        let vert_res = solve_taskbar_placement(
            vert_bar,
            Some(vert_notify),
            Some(&vert_uia),
            (72, 38),
            96,
            None,
            false,
        )
        .expect("vertical tray-adjacent gap above 830 should fit");
        assert_eq!(vert_res.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(vert_res.pos.1, 830 - 38 - 6);
    }

    #[test]
    fn compact_mode_renders_narrower_bitmap_with_all_quadrants_lit() {
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
        let normal = build_display(&cfg, &metrics, false, false, 96, false);
        let compact = build_display(&cfg, &metrics, false, false, 96, true);
        let (nw, _nh, _) = renderer
            .measure_and_render(&normal, 48.0, None)
            .expect("normal render");
        let (cw, ch, _) = renderer
            .measure_and_render(&compact, 48.0, None)
            .expect("compact render");
        assert!(
            cw < nw,
            "compact widget width ({cw}) must be strictly narrower than normal ({nw})"
        );
        let pixels = renderer.surface.as_ref().unwrap().pixels();
        let mut quadrants = [0usize; 4];
        let half_w = (cw / 2) as usize;
        let half_h = (ch / 2) as usize;
        for y in 0..ch as usize {
            for x in 0..cw as usize {
                let idx = (y * cw as usize + x) * 4;
                if pixels[idx + 3] >= 64 && pixels[idx + 2] >= 64 {
                    let q = usize::from(y >= half_h) * 2 + usize::from(x >= half_w);
                    quadrants[q] += 1;
                }
            }
        }
        assert!(
            quadrants.iter().all(|&c| c >= 12),
            "compact layout should render all 4 quadrants cleanly inside {cw}x{ch}, got {quadrants:?}"
        );
        unsafe { CoUninitialize() };
    }
}
