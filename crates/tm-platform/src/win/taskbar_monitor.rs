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
//!   light/dark contrast comes from `SystemUsesLightTheme` + `ColorPrevalence`
//!   / `AccentColor` on `WM_SETTINGCHANGE` and `WM_DWMCOLORIZATIONCOLORCHANGED`.
//! - **Independent input queue:** runs as an unowned `WS_EX_LAYERED |
//!   WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` popup on its own dedicated thread in
//!   window band 16 (`CreateWindowInBand`, falling back to standard band 1 when
//!   unavailable) so taskbar focus/clicks in band 1 cannot raise `Shell_TrayWnd`
//!   over the widget for even a single compositor frame, while hiding cleanly
//!   if a higher or peer popup/menu/flyout/topmost window overlaps its footprint.
//!   Neither cross-process parenting nor ownership may attach it to Explorer.
//! - **Zero GPU swapchain:** uses `D2D1_RENDER_TARGET_TYPE_SOFTWARE` into a
//!   cached DIB section and only calls `UpdateLayeredWindow` when rendered
//!   content, hover state, or DPI/geometry actually changes.
//! - **Collision-free automagic placement:** queries visible taskbar buttons,
//!   Widgets, and tray controls out-of-process via a dedicated MTA UI
//!   Automation scanner thread (`FindAllBuildCache` with `AutomationElementMode_None`
//!   and an 80 ms transaction timeout, triggered only on shell/tray layout
//!   events), relocating across open taskbar gaps, compacting when tight, and
//!   hiding cleanly if occupancy is unknown or the taskbar is 100% full. During
//!   revalidation, existing pixels stay fixed and the window passes clicks through.
//! - **Fullscreen suppression:** hides when a fullscreen window covers the
//!   taskbar monitor or an auto-hide taskbar slides away. Visibility changes
//!   wake the app so invisible telemetry returns to its background cadence.
//!   DWM presentation and MPO selection remain Windows/driver decisions.

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tm_core::format::{format_taskbar_pct, format_taskbar_rate};
use tm_core::settings::{TaskbarColorMode, TaskbarNetUnit};
use windows::Win32::Foundation::{
    COLORREF, CloseHandle, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
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
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_TRANSITIONS_FORCEDISABLED,
    DwmGetWindowAttribute, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CombineRgn,
    CreateCompatibleDC, CreateDIBSection, CreateRectRgn, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromWindow, RGN_DIFF, SelectObject, SetWindowRgn,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoCreateInstance,
    CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
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
    CS_DBLCLKS, ChangeWindowMessageFilterEx, CreatePopupMenu, CreateWindowExW,
    DeregisterShellHookWindow, DestroyMenu, DestroyWindow, DispatchMessageW, EVENT_OBJECT_CLOAKED,
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_REORDER,
    EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MENUPOPUPEND,
    EVENT_SYSTEM_MENUPOPUPSTART, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, EnumWindows, FindWindowExW, FindWindowW, GW_HWNDPREV, GWL_EXSTYLE,
    GWL_STYLE, GWLP_USERDATA, GetClassNameW, GetCursorPos, GetDesktopWindow, GetForegroundWindow,
    GetLayeredWindowAttributes, GetMessageW, GetShellWindow, GetWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, HMENU, HSHELL_WINDOWACTIVATED, HSHELL_WINDOWCREATED,
    HSHELL_WINDOWDESTROYED, HSHELL_WINDOWREPLACED, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST,
    IDC_ARROW, InsertMenuW, IsChild, IsIconic, IsWindowVisible, KillTimer,
    LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE, MF_BYPOSITION,
    MF_SEPARATOR, MF_STRING, MSG, MSGFLT_ALLOW, OBJID_WINDOW, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterShellHookWindow, RegisterWindowMessageW, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING, SWP_NOSIZE,
    SWP_NOZORDER, SetForegroundWindow, SetMenuDefaultItem, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTALIGN,
    TPM_RIGHTBUTTON, TPM_TOPALIGN, TrackPopupMenuEx, TranslateMessage, ULW_ALPHA,
    UpdateLayeredWindow, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_CLOSE,
    WM_CREATE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_DWMCOLORIZATIONCOLORCHANGED,
    WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NULL, WM_RBUTTONUP, WM_SETTINGCHANGE,
    WM_TIMER, WNDCLASSEXW, WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BOOL, Interface, PCWSTR, PWSTR, s, w};

use crate::taskbar_monitor::{Action, Config, Metrics};

const WM_APP_REFRESH: u32 = WM_APP + 10;
const WM_MOUSELEAVE: u32 = 0x02A3;
const UIA_SETTLE_TIMER_ID: usize = 1;
const UIA_SETTLE_DELAY_MS: u32 = 300;
const FULLSCREEN_EXIT_TIMER_ID: usize = 2;
// `USER_TIMER_MINIMUM` (10 ms): on Windows' default 15.625 ms tick, 16 ms spills
// onto the second tick (~31 ms), whereas 10 ms catches the next 15.6 ms tick
// without changing system timer resolution.
const FULLSCREEN_EXIT_RECHECK_MS: u32 = 10;
const FULLSCREEN_EXIT_MAX_RETRIES: u8 = 70;
const TOPMOST_RECHECK_TIMER_ID: usize = 3;
const TOPMOST_RECHECK_MS: u32 = 500;
const UIA_MIN_SCAN_INTERVAL: Duration = Duration::from_millis(80);
const UIA_FAILURE_BACKOFF_STEPS: [Duration; 4] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];
const MIN_ADAPTIVE_FONT_PT: f32 = 6.5;
// `HSHELL_HIGHBIT | HSHELL_WINDOWACTIVATED`: Windows Shell notification fired
// when a fullscreen / "rude" app enters or leaves fullscreen focus.
const HSHELL_RUDEAPPACTIVATED_MSG: u32 = 0x8004;
// Native Task Manager's band (`ZBID_SYSTEM_TOOLS`), strictly above
// `Shell_TrayWnd` in band 1 (`ZBID_DESKTOP`) without Explorer ownership.
const TASKBAR_MONITOR_BAND: u32 = 16;

#[derive(Clone, Copy, Debug)]
struct LayoutBounds {
    horizontal: bool,
    max_dip_w: f32,
    max_dip_h: f32,
}

impl LayoutBounds {
    fn from_taskbar(taskbar_rect: PixelRect, dpi: u32) -> Self {
        let scale = dpi.max(96) as f32 / 96.0;
        Self {
            horizontal: taskbar_rect.width() >= taskbar_rect.height(),
            max_dip_w: taskbar_rect.width() as f32 / scale,
            max_dip_h: taskbar_rect.height() as f32 / scale,
        }
    }

    #[cfg(test)]
    fn horizontal(max_dip_h: f32) -> Self {
        Self {
            horizontal: true,
            max_dip_w: f32::INFINITY,
            max_dip_h,
        }
    }

    fn avail_dip_h(self) -> f32 {
        if self.max_dip_h > 16.0 {
            (self.max_dip_h - 4.0).max(12.0)
        } else {
            (self.max_dip_h - 2.0).max(1.0)
        }
    }
}

fn layout_fits_cross_axis(layout: &MeasuredLayout, bounds: LayoutBounds) -> bool {
    if bounds.horizontal {
        layout.content_h + 2.0 <= bounds.avail_dip_h()
    } else {
        layout.dip_w <= (bounds.max_dip_w - 2.0).max(1.0)
    }
}

/// Global HWNDs and coalescing flags so WinEvent hooks (which carry no
/// user-data pointer) can wake `"tm-taskbar"` immediately without flooding its
/// message queue.
static LIVE_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static LIVE_FG_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static LIVE_FS_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static LIVE_TRAY_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static LIVE_OCCLUDER_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static IN_ELEVATED_BAND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static REFRESH_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAYOUT_RECHECK_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static FULLSCREEN_WAKE_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutUpdate {
    Recheck,
    Invalidate,
}

fn taskbar_event_layout_update(event: u32, id_object: i32, id_child: i32) -> Option<LayoutUpdate> {
    let window_self = id_object == OBJID_WINDOW.0 && id_child == 0;
    if event != EVENT_OBJECT_REORDER
        && !(window_self
            && matches!(
                event,
                EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE | EVENT_OBJECT_LOCATIONCHANGE
            ))
    {
        return None;
    }
    // Both native taskbar children and accessible controls report changes when
    // focus or unrelated app buttons change. Revalidate transactionally: keep
    // the last pixels, but pass clicks through until fresh occupancy arrives.
    // Refresh separately invalidates an actual taskbar identity/geometry change.
    Some(LayoutUpdate::Recheck)
}

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

struct MonitorVisibility {
    visible: std::sync::atomic::AtomicBool,
    notifier: Arc<dyn Fn() + Send + Sync>,
}

impl MonitorVisibility {
    fn set(&self, visible: bool) {
        if self
            .visible
            .swap(visible, std::sync::atomic::Ordering::AcqRel)
            != visible
        {
            (self.notifier)();
        }
    }
}

pub struct TaskbarMonitorHandle {
    shared: Arc<Mutex<SharedState>>,
    visibility: Arc<MonitorVisibility>,
    callback: Arc<dyn Fn(Action) + Send + Sync>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl TaskbarMonitorHandle {
    pub fn new(callback: Arc<dyn Fn(Action) + Send + Sync>) -> Self {
        Self::new_with_visibility_notifier(callback, Arc::new(|| {}))
    }

    pub fn new_with_visibility_notifier(
        callback: Arc<dyn Fn(Action) + Send + Sync>,
        visibility_notifier: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            shared: Arc::new(Mutex::new(SharedState {
                config: Config::default(),
                metrics: Metrics::default(),
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: visibility_notifier,
            }),
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
        }
        self.stop_worker();
        self.visibility.set(false);
    }

    pub fn is_visible(&self) -> bool {
        self.visibility
            .visible
            .load(std::sync::atomic::Ordering::Acquire)
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
        let visibility = self.visibility.clone();
        let callback = self.callback.clone();
        match std::thread::Builder::new()
            .name("tm-taskbar".into())
            .spawn(move || taskbar_thread_main(shared, visibility, callback))
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
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

    /// The overlapping area in window-local coordinates, if any.
    fn local_intersection(self, other: Self) -> Option<Self> {
        let left = self.left.max(other.left);
        let top = self.top.max(other.top);
        let right = self.right.min(other.right);
        let bottom = self.bottom.min(other.bottom);
        (right > left && bottom > top).then_some(Self {
            left: left - self.left,
            top: top - self.top,
            right: right - self.left,
            bottom: bottom - self.top,
        })
    }
}

/// How a partially occluded widget should react to a set of occluder rects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OcclusionOutcome {
    /// No occluder: any stale clip must be removed.
    Clear,
    /// Still partly visible: keep the window and cut out the covered strips.
    Clipped,
    /// Nothing left to present: fall back to hiding.
    Hidden,
}

/// Normalized, stable local clip rects used both as the region recipe and as
/// the change-detection key, so redundant `SetWindowRgn` calls are skipped.
#[derive(Clone, PartialEq, Eq, Debug)]
struct AppliedClip {
    size: (i32, i32),
    local_rects: Vec<PixelRect>,
}

/// Sub-pixel jitter in an animating occluder's frame would otherwise reinstall
/// an identical window region on every refresh, forcing needless DWM
/// recomposition. Skip the region update while the difference stays tiny.
const CLIP_TOLERANCE_PX: i32 = 2;

fn applied_clip_equivalent(previous: &AppliedClip, next: &AppliedClip) -> bool {
    previous.size == next.size
        && previous.local_rects.len() == next.local_rects.len()
        && previous
            .local_rects
            .iter()
            .zip(&next.local_rects)
            .all(|(a, b)| {
                (a.left - b.left).abs() <= CLIP_TOLERANCE_PX
                    && (a.top - b.top).abs() <= CLIP_TOLERANCE_PX
                    && (a.right - b.right).abs() <= CLIP_TOLERANCE_PX
                    && (a.bottom - b.bottom).abs() <= CLIP_TOLERANCE_PX
            })
}

fn local_clip_rects(target: PixelRect, occluder_rects: &[PixelRect]) -> Vec<PixelRect> {
    let mut rects: Vec<PixelRect> = occluder_rects
        .iter()
        .filter_map(|frame| target.local_intersection(*frame))
        .filter(|r| !r.is_empty())
        .collect();
    rects.sort_unstable_by_key(|r| (r.left, r.top, r.right, r.bottom));
    rects.dedup();
    rects
}

/// Exact rectangle subtraction: split each remaining piece around the clip.
fn subtract_local_rect(pieces: &mut Vec<PixelRect>, clip: PixelRect) {
    let mut next = Vec::with_capacity(pieces.len().saturating_add(3));
    for piece in pieces.drain(..) {
        let left = piece.left.max(clip.left);
        let top = piece.top.max(clip.top);
        let right = piece.right.min(clip.right);
        let bottom = piece.bottom.min(clip.bottom);
        if right <= left || bottom <= top {
            next.push(piece);
            continue;
        }
        if piece.left < left {
            next.push(PixelRect {
                left: piece.left,
                top: piece.top,
                right: left,
                bottom: piece.bottom,
            });
        }
        if right < piece.right {
            next.push(PixelRect {
                left: right,
                top: piece.top,
                right: piece.right,
                bottom: piece.bottom,
            });
        }
        if piece.top < top {
            next.push(PixelRect {
                left,
                top: piece.top,
                right,
                bottom: top,
            });
        }
        if bottom < piece.bottom {
            next.push(PixelRect {
                left,
                top: bottom,
                right,
                bottom: piece.bottom,
            });
        }
    }
    *pieces = next;
}

fn occlusion_outcome(size: (i32, i32), clips: &[PixelRect]) -> OcclusionOutcome {
    if clips.is_empty() || size.0 <= 0 || size.1 <= 0 {
        return OcclusionOutcome::Clear;
    }
    let mut remaining = vec![PixelRect {
        left: 0,
        top: 0,
        right: size.0,
        bottom: size.1,
    }];
    for clip in clips {
        subtract_local_rect(&mut remaining, *clip);
        if remaining.is_empty() {
            return OcclusionOutcome::Hidden;
        }
    }
    OcclusionOutcome::Clipped
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

struct CachedFormatLayout {
    family: String,
    dip_size_bits: u32,
    fmt: IDWriteTextFormat,
    up: (f32, f32, Option<IDWriteTextLayout>),
    down: (f32, f32, Option<IDWriteTextLayout>),
    cpu_label: (f32, f32, Option<IDWriteTextLayout>),
    c_label: (f32, f32, Option<IDWriteTextLayout>),
    ram_label: (f32, f32, Option<IDWriteTextLayout>),
    r_label: (f32, f32, Option<IDWriteTextLayout>),
    net_sample: Option<(String, f32)>,
    sys_val_sample: Option<(String, f32)>,
}

struct Renderer {
    d2d_factory: ID2D1Factory,
    dwrite_factory: IDWriteFactory,
    dc_rt: ID2D1DCRenderTarget,
    surface: Option<DibSurface>,
    format_cache: Vec<CachedFormatLayout>,
    rendering_params: Option<(isize, bool, IDWriteRenderingParams)>,
    text_brush: Option<([u8; 3], ID2D1SolidColorBrush)>,
    hover_brush: Option<(bool, ID2D1SolidColorBrush)>,
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
                format_cache: Vec::new(),
                rendering_params: None,
                text_brush: None,
                hover_brush: None,
            })
        }
    }

    fn invalidate_rendering_params(&mut self) {
        self.rendering_params = None;
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
        let dip_size = (font_size_pt.clamp(MIN_ADAPTIVE_FONT_PT, 18.0)) * (96.0 / 72.0);
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

    fn cached_format_index(&mut self, font_family: &str, font_size_pt: f32) -> Option<usize> {
        let family = if font_family.trim().is_empty() {
            "Segoe UI"
        } else {
            font_family.trim()
        };
        let dip_size = (font_size_pt.clamp(MIN_ADAPTIVE_FONT_PT, 18.0)) * (96.0 / 72.0);
        let dip_size_bits = dip_size.to_bits();
        if let Some(idx) = self
            .format_cache
            .iter()
            .position(|c| c.dip_size_bits == dip_size_bits && c.family == family)
        {
            return Some(idx);
        }

        let fmt = self
            .create_text_format(family, font_size_pt)
            .or_else(|| self.create_text_format("Segoe UI", font_size_pt))?;
        let up = self.measure_text(&fmt, "↑");
        let down = self.measure_text(&fmt, "↓");
        let cpu_label = self.measure_text(&fmt, "CPU");
        let c_label = self.measure_text(&fmt, "C");
        let ram_label = self.measure_text(&fmt, "RAM");
        let r_label = self.measure_text(&fmt, "R");
        if self.format_cache.len() >= 6 {
            self.format_cache.remove(0);
        }
        self.format_cache.push(CachedFormatLayout {
            family: family.to_string(),
            dip_size_bits,
            fmt,
            up,
            down,
            cpu_label,
            c_label,
            ram_label,
            r_label,
            net_sample: None,
            sys_val_sample: None,
        });
        Some(self.format_cache.len() - 1)
    }

    fn apply_text_rendering_params(&mut self, hwnd: Option<isize>, dpi: u32) {
        let monitor_raw = unsafe {
            MonitorFromWindow(
                HWND(hwnd.unwrap_or(0) as *mut std::ffi::c_void),
                MONITOR_DEFAULTTOPRIMARY,
            )
        }
        .0 as isize;
        let classic = dpi <= 120;
        if let Some((cached_mon, cached_classic, _)) = &self.rendering_params
            && *cached_mon == monitor_raw
            && *cached_classic == classic
        {
            return;
        }
        let params = crate::win::text_rendering::query(hwnd);
        let geom = if params.bgr {
            DWRITE_PIXEL_GEOMETRY_BGR
        } else {
            DWRITE_PIXEL_GEOMETRY_RGB
        };
        let mode = if classic {
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
                self.rendering_params = Some((monitor_raw, classic, rp_base));
            }
        }
    }

    fn measure_layout(
        &mut self,
        display: &RenderedDisplay,
        bounds: LayoutBounds,
    ) -> Option<MeasuredLayout> {
        let mut best = self.measure_layout_at_size(display, display.font_size_pt, bounds)?;
        if layout_fits_cross_axis(&best, bounds) || display.font_size_pt <= MIN_ADAPTIVE_FONT_PT {
            return Some(best);
        }

        // Step down gently in 0.5 pt increments when a short horizontal bar or narrow
        // vertical bar cannot fit the user's configured font size without clipping.
        let mut size =
            ((display.font_size_pt * 2.0).floor() / 2.0 - 0.5).min(display.font_size_pt - 0.25);
        while size >= MIN_ADAPTIVE_FONT_PT {
            if let Some(candidate) = self.measure_layout_at_size(display, size, bounds) {
                let fits = layout_fits_cross_axis(&candidate, bounds);
                best = candidate;
                if fits {
                    return Some(best);
                }
            }
            size -= 0.5;
        }
        if (size + 0.5 - MIN_ADAPTIVE_FONT_PT).abs() > 0.05
            && let Some(candidate) =
                self.measure_layout_at_size(display, MIN_ADAPTIVE_FONT_PT, bounds)
        {
            best = candidate;
        }
        Some(best)
    }

    fn measure_layout_at_size(
        &mut self,
        display: &RenderedDisplay,
        font_size_pt: f32,
        bounds: LayoutBounds,
    ) -> Option<MeasuredLayout> {
        let _ = &self.d2d_factory;
        let cache_idx = self.cached_format_index(&display.font_family, font_size_pt)?;
        let fmt = self.format_cache[cache_idx].fmt.clone();

        let has_net = display.show_net && display.net_sent.is_some() && display.net_recv.is_some();
        let has_cpu = display.cpu_label.is_some() && display.cpu_val.is_some();
        let has_ram = display.ram_label.is_some() && display.ram_val.is_some();
        let has_sys = has_cpu || has_ram;
        if !has_net && !has_sys {
            return None;
        }

        let (up_w, arrow_h1, up_layout) = if has_net {
            self.format_cache[cache_idx].up.clone()
        } else {
            (0.0, 0.0, None)
        };
        let (down_w, arrow_h2, down_layout) = if has_net {
            self.format_cache[cache_idx].down.clone()
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
        let net_sample_w = if has_net {
            if let Some((cached_s, cached_w)) = &self.format_cache[cache_idx].net_sample
                && cached_s == &display.net_sample
            {
                *cached_w
            } else {
                let (w, _, _) = self.measure_text(&fmt, &display.net_sample);
                self.format_cache[cache_idx].net_sample = Some((display.net_sample.clone(), w));
                w
            }
        } else {
            0.0
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
            Some("CPU") => self.format_cache[cache_idx].cpu_label.clone(),
            Some("C") => self.format_cache[cache_idx].c_label.clone(),
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (cpu_vw, line_h3, cpu_val_layout) = match display.cpu_val.as_deref() {
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (ram_lw, ram_lh, ram_label_layout) = match display.ram_label.as_deref() {
            Some("RAM") => self.format_cache[cache_idx].ram_label.clone(),
            Some("R") => self.format_cache[cache_idx].r_label.clone(),
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (ram_vw, line_h4, ram_val_layout) = match display.ram_val.as_deref() {
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let sys_val_sample_w = if has_sys {
            if let Some((cached_s, cached_w)) = &self.format_cache[cache_idx].sys_val_sample
                && cached_s == &display.sys_val_sample
            {
                *cached_w
            } else {
                let (w, _, _) = self.measure_text(&fmt, &display.sys_val_sample);
                self.format_cache[cache_idx].sys_val_sample =
                    Some((display.sys_val_sample.clone(), w));
                w
            }
        } else {
            0.0
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
            .max(10.0);
        let avail_h = bounds.avail_dip_h();
        let pad_x = if display.compact { 3.0 } else { 6.0 };
        let col_gap = if has_net && has_sys {
            if display.compact { 4.0 } else { 7.0 }
        } else {
            0.0
        };
        let side_by_side_w = pad_x * 2.0 + net_col_w + col_gap + sys_col_w;
        // On a vertical taskbar, stack Network above CPU/RAM when side-by-side
        // columns exceed the bar's cross-axis width.
        let stacked = !bounds.horizontal
            && has_net
            && has_sys
            && side_by_side_w > (bounds.max_dip_w - 2.0).max(1.0);

        let two_rows = has_net || (has_cpu && has_ram);
        let mut row_gap = if two_rows { 1.0 } else { 0.0 };
        if !stacked && two_rows && row_h * 2.0 + row_gap + 4.0 > avail_h {
            row_gap = 0.0;
        }
        let section_gap = if stacked {
            if display.compact { 2.0 } else { 3.0 }
        } else {
            0.0
        };
        let net_rows_h = if has_net { row_h * 2.0 + row_gap } else { 0.0 };
        let sys_rows_h = if has_cpu && has_ram {
            row_h * 2.0 + row_gap
        } else if has_sys {
            row_h
        } else {
            0.0
        };
        let content_h = if stacked {
            net_rows_h + section_gap + sys_rows_h
        } else if two_rows {
            row_h * 2.0 + row_gap
        } else {
            row_h
        };
        let content_w = if stacked {
            net_col_w.max(sys_col_w)
        } else {
            net_col_w + col_gap + sys_col_w
        };
        let pad_y = ((avail_h - content_h) * 0.5).clamp(1.0, 4.0);
        let dip_w = (pad_x * 2.0 + content_w).ceil();
        let min_h = 22.0_f32.min(avail_h);
        let dip_h = (content_h + pad_y * 2.0).clamp(min_h, avail_h).ceil();

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
            section_gap,
            content_h,
            col_gap,
            stacked,
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
        let surface_hdc = self.surface.as_ref()?.hdc;

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
            self.dc_rt.BindDC(surface_hdc, &bind_rect).ok()?;
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
                let hover_brush = match &self.hover_brush {
                    Some((light, b)) if *light == display.light_taskbar => Some(b.clone()),
                    _ => {
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
                        self.dc_rt
                            .CreateSolidColorBrush(&hover_color, None)
                            .ok()
                            .inspect(|b| {
                                self.hover_brush = Some((display.light_taskbar, b.clone()));
                            })
                    }
                };
                if let Some(hover_brush) = hover_brush {
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

            let brush: ID2D1SolidColorBrush = match &self.text_brush {
                Some((rgb, b)) if *rgb == display.text_rgb => b.clone(),
                _ => {
                    let text_color = D2D1_COLOR_F {
                        r: f32::from(display.text_rgb[0]) / 255.0,
                        g: f32::from(display.text_rgb[1]) / 255.0,
                        b: f32::from(display.text_rgb[2]) / 255.0,
                        a: 1.0,
                    };
                    match self.dc_rt.CreateSolidColorBrush(&text_color, None) {
                        Ok(b) => {
                            self.text_brush = Some((display.text_rgb, b.clone()));
                            b
                        }
                        Err(_) => {
                            let _ = self.dc_rt.EndDraw(None, None);
                            self.text_brush = None;
                            self.hover_brush = None;
                            return None;
                        }
                    }
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
                let label_x = snap_dip(if layout.stacked {
                    layout.pad_x
                } else {
                    layout.pad_x
                        + layout.net_col_w
                        + if layout.has_net { layout.col_gap } else { 0.0 }
                });
                let val_x = snap_dip(label_x + layout.sys_label_w + layout.sys_label_gap);
                let draw_sys_row =
                    |lbl: Option<IDWriteTextLayout>, val: Option<IDWriteTextLayout>, y: f32| {
                        draw_text(lbl, label_x, y, layout.sys_label_w);
                        draw_text(val, val_x, y, layout.sys_val_w);
                    };

                let (sys_y1, sys_y2, sys_single_y) = if layout.stacked {
                    let net_block_h = layout.row_h * 2.0 + layout.row_gap;
                    let sys_top = snap_dip(top_y + net_block_h + layout.section_gap);
                    (
                        sys_top,
                        snap_dip(sys_top + layout.row_h + layout.row_gap),
                        sys_top,
                    )
                } else {
                    (top_y, row2_y, single_y)
                };

                if layout.has_cpu && layout.has_ram {
                    draw_sys_row(layout.cpu_label_layout, layout.cpu_val_layout, sys_y1);
                    draw_sys_row(layout.ram_label_layout, layout.ram_val_layout, sys_y2);
                } else if layout.has_cpu {
                    draw_sys_row(layout.cpu_label_layout, layout.cpu_val_layout, sys_single_y);
                } else if layout.has_ram {
                    draw_sys_row(layout.ram_label_layout, layout.ram_val_layout, sys_single_y);
                }
            }

            if self.dc_rt.EndDraw(None, None).is_err() {
                self.text_brush = None;
                self.hover_brush = None;
                self.rendering_params = None;
                return None;
            }
            Some((px_w, px_h, surface_hdc))
        }
    }

    #[cfg(test)]
    fn measure_and_render(
        &mut self,
        display: &RenderedDisplay,
        max_taskbar_dip_h: f32,
        hwnd: Option<isize>,
    ) -> Option<(i32, i32, HDC)> {
        self.measure_and_render_in(display, LayoutBounds::horizontal(max_taskbar_dip_h), hwnd)
    }

    #[cfg(test)]
    fn measure_and_render_in(
        &mut self,
        display: &RenderedDisplay,
        bounds: LayoutBounds,
        hwnd: Option<isize>,
    ) -> Option<(i32, i32, HDC)> {
        let layout = self.measure_layout(display, bounds)?;
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
    section_gap: f32,
    content_h: f32,
    col_gap: f32,
    stacked: bool,
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
    in_elevated_band: bool,
    user_closing: bool,
    shared: Arc<Mutex<SharedState>>,
    visibility: Arc<MonitorVisibility>,
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
    last_present_error: Option<Instant>,
    last_anchor_slot: Option<AnchorSlot>,
    last_compact: bool,
    last_stacked: bool,
    last_env_geom: Option<(isize, PixelRect, Option<PixelRect>, u32)>,
    pending_uia_scan: bool,
    invalidate_uia_layout: bool,
    restore_from_cached_layout: bool,
    tracked_fg_tid: u32,
    fg_loc_hook: HWINEVENTHOOK,
    tracked_fs_tid: u32,
    fs_loc_hook: HWINEVENTHOOK,
    fs_lifecycle_hook: HWINEVENTHOOK,
    fs_cloak_hook: HWINEVENTHOOK,
    tracked_tray_tid: u32,
    tray_hook: HWINEVENTHOOK,
    topmost_hooks: Vec<(u32, HWINEVENTHOOK)>,
    last_fg_hwnd: isize,
    fullscreen_recheck_remaining: u8,
    fullscreen_recheck_armed: bool,
    hidden_by_occlusion: bool,
    hidden_by_tray_missing: bool,
    last_occluded_rect: Option<PixelRect>,
    applied_clip: Option<AppliedClip>,
    /// Kept separately from the global so a later `set_visible(true)` can
    /// restore the occluder we are still clipping around.
    clip_occluder: HWND,
    topmost_recheck_armed: bool,
}

impl Drop for WindowCtx {
    fn drop(&mut self) {
        self.visibility.set(false);
        self.unhook_dynamic_hooks();
    }
}

impl WindowCtx {
    fn request_layout_scan(&mut self, update: LayoutUpdate, arm_settle: bool) {
        self.pending_uia_scan = true;
        if update == LayoutUpdate::Invalidate {
            self.invalidate_uia_layout = true;
            self.restore_from_cached_layout = false;
            self.last_display = None;
            self.last_anchor_slot = None;
            self.last_compact = false;
        }
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
        LIVE_FS_HWND.store(0, std::sync::atomic::Ordering::Release);
        LIVE_TRAY_HWND.store(0, std::sync::atomic::Ordering::Release);
        LIVE_OCCLUDER_HWND.store(0, std::sync::atomic::Ordering::Release);
        if !self.fg_loc_hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.fg_loc_hook);
            }
            self.fg_loc_hook = HWINEVENTHOOK::default();
        }
        for hook in [
            &mut self.fs_loc_hook,
            &mut self.fs_lifecycle_hook,
            &mut self.fs_cloak_hook,
        ] {
            if !hook.is_invalid() {
                unsafe {
                    let _ = UnhookWinEvent(*hook);
                }
                *hook = HWINEVENTHOOK::default();
            }
        }
        if !self.tray_hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.tray_hook);
            }
            self.tray_hook = HWINEVENTHOOK::default();
        }
        for (_, hook) in self.topmost_hooks.drain(..) {
            if !hook.is_invalid() {
                unsafe {
                    let _ = UnhookWinEvent(hook);
                }
            }
        }
        self.tracked_fg_tid = 0;
        self.tracked_fs_tid = 0;
        self.tracked_tray_tid = 0;
    }

    fn sync_fullscreen_hook(&mut self, fs_hwnd: HWND) {
        LIVE_FS_HWND.store(fs_hwnd.0 as isize, std::sync::atomic::Ordering::Release);
        let tid = if fs_hwnd.0.is_null() {
            0
        } else {
            unsafe { GetWindowThreadProcessId(fs_hwnd, None) }
        };
        let self_tid = unsafe { GetCurrentThreadId() };
        let target_tid = if tid != 0 && tid != self_tid { tid } else { 0 };
        if target_tid == self.tracked_fs_tid {
            return;
        }
        for hook in [
            &mut self.fs_loc_hook,
            &mut self.fs_lifecycle_hook,
            &mut self.fs_cloak_hook,
        ] {
            if !hook.is_invalid() {
                unsafe {
                    let _ = UnhookWinEvent(*hook);
                }
                *hook = HWINEVENTHOOK::default();
            }
        }
        self.tracked_fs_tid = target_tid;
        if target_tid != 0 {
            let install_fs_hook = |min_ev: u32, max_ev: u32| unsafe {
                SetWinEventHook(
                    min_ev,
                    max_ev,
                    None,
                    Some(fs_location_event_proc),
                    0,
                    target_tid,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            self.fs_loc_hook =
                install_fs_hook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE);
            self.fs_lifecycle_hook = install_fs_hook(EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE);
            self.fs_cloak_hook = install_fs_hook(EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED);
        }
    }

    fn disarm_fullscreen_exit_recheck(&mut self) {
        if self.fullscreen_recheck_armed && !self.hwnd.0.is_null() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), FULLSCREEN_EXIT_TIMER_ID);
            }
        }
        self.fullscreen_recheck_armed = false;
        self.fullscreen_recheck_remaining = 0;
    }

    fn disarm_topmost_recheck(&mut self) {
        if self.topmost_recheck_armed && !self.hwnd.0.is_null() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TOPMOST_RECHECK_TIMER_ID);
            }
        }
        self.topmost_recheck_armed = false;
    }

    fn sync_topmost_recheck(&mut self) {
        let should_arm = self.in_elevated_band
            && (self.last_visible || self.hidden_by_occlusion || self.hidden_by_tray_missing);
        if should_arm {
            if !self.topmost_recheck_armed && !self.hwnd.0.is_null() {
                unsafe {
                    let _ = SetTimer(
                        Some(self.hwnd),
                        TOPMOST_RECHECK_TIMER_ID,
                        TOPMOST_RECHECK_MS,
                        None,
                    );
                }
                self.topmost_recheck_armed = true;
            }
        } else {
            self.disarm_topmost_recheck();
        }
    }

    fn excluded_topmost_tids(&self) -> [u32; 3] {
        [
            unsafe { GetCurrentThreadId() },
            self.tracked_fg_tid,
            self.tracked_tray_tid,
        ]
    }

    fn recheck_topmost_occlusion_with_tray(&mut self, tray_hwnd: HWND) -> bool {
        if !self.in_elevated_band
            || (!self.last_visible && !self.hidden_by_occlusion && !self.hidden_by_tray_missing)
        {
            self.disarm_topmost_recheck();
            return false;
        }
        if self.hidden_by_tray_missing {
            self.hidden_by_tray_missing = false;
            return true;
        }
        let probe_rect = if self.last_visible {
            self.last_rect()
        } else if self.hidden_by_occlusion {
            match self.last_occluded_rect.or_else(|| self.last_rect()) {
                Some(r) => Some(r),
                None => {
                    self.hidden_by_occlusion = false;
                    self.disarm_topmost_recheck();
                    return true;
                }
            }
        } else {
            None
        };
        let excluded = self.excluded_topmost_tids();
        let summary = scan_above_tray(tray_hwnd, self.hwnd, None, probe_rect, Some(&excluded));
        self.sync_topmost_hooks_with_tids(&summary.topmost_tids);
        let occlusion = summary.occlusion;
        let occluder_rects = Arc::new(summary.occluder_rects);
        let prechecked = probe_rect
            .zip(occlusion)
            .map(|(rect, check)| (rect, check, occluder_rects));
        if self.last_visible {
            let _ = self.hide_occluded_if_needed_cached(tray_hwnd, probe_rect, prechecked);
            return false;
        }
        self.hidden_by_occlusion && summary.occlusion == Some(OcclusionCheck::Clear)
    }

    fn recheck_topmost_occlusion(&mut self) {
        if !self.in_elevated_band
            || (!self.last_visible && !self.hidden_by_occlusion && !self.hidden_by_tray_missing)
        {
            self.disarm_topmost_recheck();
            return;
        }
        let tray_hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }.unwrap_or_default();
        if tray_hwnd.0.is_null()
            || unsafe { !IsWindowVisible(tray_hwnd).as_bool() }
            || is_window_cloaked(tray_hwnd)
        {
            self.refresh(false);
            return;
        }
        if self.recheck_topmost_occlusion_with_tray(tray_hwnd) {
            self.refresh(false);
        }
    }

    fn sync_foreground_hook(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
        LIVE_FG_HWND.store(fg.0 as isize, std::sync::atomic::Ordering::Release);
        let tid = if fg.0.is_null() {
            0
        } else {
            unsafe { GetWindowThreadProcessId(fg, None) }
        };
        let self_tid = unsafe { GetCurrentThreadId() };
        let target_tid = if tid != 0 && tid != self_tid { tid } else { 0 };
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
                    WINEVENT_OUTOFCONTEXT,
                )
            };
        }
    }

    fn sync_tray_hook(&mut self, tray_hwnd: HWND) {
        LIVE_TRAY_HWND.store(tray_hwnd.0 as isize, std::sync::atomic::Ordering::Release);
        let tid = if tray_hwnd.0.is_null() {
            0
        } else {
            unsafe { GetWindowThreadProcessId(tray_hwnd, None) }
        };
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

    #[cfg(test)]
    fn sync_topmost_hooks(&mut self, tray_hwnd: HWND) {
        let target_tids = if self.in_elevated_band && !tray_hwnd.0.is_null() {
            let excluded = self.excluded_topmost_tids();
            collect_topmost_occluder_tids(tray_hwnd, self.hwnd, &excluded)
        } else {
            Vec::new()
        };
        self.sync_topmost_hooks_with_tids(&target_tids);
    }

    fn sync_topmost_hooks_with_tids(&mut self, target_tids: &[u32]) {
        self.topmost_hooks.retain(|&(tid, hook)| {
            if target_tids.contains(&tid) {
                true
            } else {
                if !hook.is_invalid() {
                    unsafe {
                        let _ = UnhookWinEvent(hook);
                    }
                }
                false
            }
        });

        for &tid in target_tids {
            if self
                .topmost_hooks
                .iter()
                .any(|&(existing, _)| existing == tid)
            {
                continue;
            }
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    None,
                    Some(topmost_location_event_proc),
                    0,
                    tid,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if !hook.is_invalid() {
                self.topmost_hooks.push((tid, hook));
            }
        }
    }

    fn refresh(&mut self, force_render: bool) {
        let wake_recheck = FULLSCREEN_WAKE_PENDING.swap(false, std::sync::atomic::Ordering::AcqRel);
        let (config, metrics) = {
            let guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (guard.config.clone(), guard.metrics)
        };

        if !config.enabled || (!config.show_cpu && !config.show_ram && !config.show_network) {
            self.sync_fullscreen_hook(HWND::default());
            self.disarm_fullscreen_exit_recheck();
            self.sync_topmost_hooks_with_tids(&[]);
            self.restore_from_cached_layout = false;
            self.hide("disabled", None);
            return;
        }

        // Track the active foreground window's thread even when hidden during
        // fullscreen so exiting fullscreen (F11 / Esc / resize) wakes immediately.
        self.sync_foreground_hook();
        let tray_hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }.unwrap_or_default();
        self.sync_tray_hook(tray_hwnd);

        let fg_raw = unsafe { GetForegroundWindow() }.0 as isize;
        let fg_changed = fg_raw != self.last_fg_hwnd;
        self.last_fg_hwnd = fg_raw;

        let occlusion_probe = if !self.in_elevated_band {
            None
        } else if self.last_visible {
            self.last_rect()
        } else if self.hidden_by_occlusion {
            self.last_occluded_rect.or_else(|| self.last_rect())
        } else {
            None
        };
        let excluded_tids = self.excluded_topmost_tids();

        let (env, prechecked_occlusion, topmost_tids) = match probe_taskbar_env(
            tray_hwnd,
            self.hwnd,
            occlusion_probe,
            self.in_elevated_band.then_some(&excluded_tids[..]),
        ) {
            TaskbarEnvProbe::Ready {
                env,
                prechecked_occlusion,
                topmost_tids,
            } => {
                self.sync_fullscreen_hook(HWND::default());
                self.disarm_fullscreen_exit_recheck();
                (env, prechecked_occlusion, topmost_tids)
            }
            TaskbarEnvProbe::Fullscreen {
                suppressor,
                fg_covers_monitor,
            } => {
                self.sync_topmost_hooks_with_tids(&[]);
                self.sync_fullscreen_hook(suppressor.unwrap_or_default());
                if fg_covers_monitor {
                    self.disarm_fullscreen_exit_recheck();
                } else {
                    if wake_recheck || fg_changed {
                        self.fullscreen_recheck_remaining = FULLSCREEN_EXIT_MAX_RETRIES;
                    }
                    if self.fullscreen_recheck_remaining > 0 && !self.hwnd.0.is_null() {
                        unsafe {
                            let _ = SetTimer(
                                Some(self.hwnd),
                                FULLSCREEN_EXIT_TIMER_ID,
                                FULLSCREEN_EXIT_RECHECK_MS,
                                None,
                            );
                        }
                        self.fullscreen_recheck_armed = true;
                    } else {
                        self.disarm_fullscreen_exit_recheck();
                    }
                }
                // Preserve `last_env_geom` and `restore_from_cached_layout` across
                // transient fullscreen suppression so returning to an unchanged
                // taskbar can reappear immediately while UIA revalidates.
                self.pending_uia_scan = true;
                self.hide("fullscreen", suppressor);
                return;
            }
            TaskbarEnvProbe::Unavailable(reason) => {
                self.sync_topmost_hooks_with_tids(&[]);
                self.sync_fullscreen_hook(HWND::default());
                self.disarm_fullscreen_exit_recheck();
                self.pending_uia_scan = true;
                self.hide(reason, None);
                return;
            }
        };

        self.sync_topmost_hooks_with_tids(&topmost_tids);
        self.refresh_taskbar_env_with_occlusion(
            &config,
            &metrics,
            env,
            force_render,
            prechecked_occlusion,
        );
    }

    fn last_rect(&self) -> Option<PixelRect> {
        let (x, y) = self.last_pos?;
        let (w, h) = self.last_size;
        let r = PixelRect {
            left: x,
            top: y,
            right: x.saturating_add(w),
            bottom: y.saturating_add(h),
        };
        (!r.is_empty()).then_some(r)
    }

    /// Drop any window region we installed and forget its occluder. Called when
    /// the occluder disappears, when we change hide reason, and on teardown.
    fn reset_clip(&mut self) {
        if self.applied_clip.is_some() && !self.hwnd.0.is_null() {
            unsafe {
                // A null region removes the current one; the system releases it.
                let _ = SetWindowRgn(self.hwnd, None, true);
            }
        }
        self.applied_clip = None;
        self.clip_occluder = HWND::default();
        LIVE_OCCLUDER_HWND.store(0, std::sync::atomic::Ordering::Release);
    }

    /// Subtract the occluder strips from the layered window's region. A null
    /// result or a GDI failure means clipping is unusable and the caller must
    /// fall back to hiding, exactly as before this policy existed.
    fn apply_window_clip(&self, key: &AppliedClip) -> bool {
        if self.hwnd.0.is_null() || key.size.0 <= 0 || key.size.1 <= 0 {
            return false;
        }
        unsafe {
            let visible = CreateRectRgn(0, 0, key.size.0, key.size.1);
            if visible.0.is_null() {
                return false;
            }
            for rect in &key.local_rects {
                let cut = CreateRectRgn(rect.left, rect.top, rect.right, rect.bottom);
                if cut.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(visible.0));
                    return false;
                }
                let combined = CombineRgn(Some(visible), Some(visible), Some(cut), RGN_DIFF);
                let _ = DeleteObject(HGDIOBJ(cut.0));
                if combined.0 == 0 {
                    // ERROR: the system region is untouched, release our copy.
                    let _ = DeleteObject(HGDIOBJ(visible.0));
                    return false;
                }
            }
            // On success the window owns the region; on failure it stays ours.
            if SetWindowRgn(self.hwnd, Some(visible), true) == 0 {
                let _ = DeleteObject(HGDIOBJ(visible.0));
                return false;
            }
        }
        true
    }

    /// Single decision point for all elevated-band occlusion checks. Partial
    /// overlap keeps the widget visible with a window region cutting out the
    /// covered strips; only a fully covered widget (or a GDI failure) hides.
    /// Returns `true` when the caller must stop refreshing because we hid.
    fn apply_occlusion_policy(
        &mut self,
        target_rect: PixelRect,
        check: OcclusionCheck,
        occluder_rects: Vec<PixelRect>,
    ) -> bool {
        match check {
            OcclusionCheck::Clear => {
                self.reset_clip();
                false
            }
            OcclusionCheck::TrayMissing => {
                self.reset_clip();
                self.hide("tray_missing_in_enum", None);
                true
            }
            OcclusionCheck::Occluded(hwnd) => {
                let local_rects = local_clip_rects(target_rect, &occluder_rects);
                let key = AppliedClip {
                    size: (target_rect.width(), target_rect.height()),
                    local_rects,
                };
                match occlusion_outcome(key.size, &key.local_rects) {
                    OcclusionOutcome::Hidden => {
                        self.reset_clip();
                        self.last_occluded_rect = Some(target_rect);
                        self.hide("occluded_above_tray", Some(hwnd));
                        return true;
                    }
                    OcclusionOutcome::Clear => {
                        self.reset_clip();
                        return false;
                    }
                    OcclusionOutcome::Clipped => {}
                }
                let unchanged = self
                    .applied_clip
                    .as_ref()
                    .is_some_and(|previous| applied_clip_equivalent(previous, &key));
                if !unchanged && !self.apply_window_clip(&key) {
                    self.reset_clip();
                    self.last_occluded_rect = Some(target_rect);
                    self.hide("occluded_above_tray", Some(hwnd));
                    return true;
                }
                tracing::debug!(
                    occluder = ?hwnd,
                    strips = key.local_rects.len(),
                    "clipping taskbar monitor around occluder"
                );
                self.applied_clip = Some(key);
                self.clip_occluder = hwnd;
                LIVE_OCCLUDER_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::Release);
                false
            }
        }
    }

    #[cfg(test)]
    fn ensure_visible_order(&self, tray_hwnd: HWND, target_rect: Option<PixelRect>) -> bool {
        if self.in_elevated_band {
            let Some(rect) = target_rect else {
                return false;
            };
            !is_rect_occluded_above_taskbar(tray_hwnd, self.hwnd, rect)
        } else {
            ensure_just_above_taskbar(self.hwnd, tray_hwnd)
        }
    }

    fn hide_occluded_if_needed_cached(
        &mut self,
        tray_hwnd: HWND,
        target_rect: Option<PixelRect>,
        prechecked: Option<PrecheckedOcclusion>,
    ) -> bool {
        if self.in_elevated_band {
            let Some(rect) = target_rect else {
                self.restore_from_cached_layout = false;
                self.hide("missing_target_rect", None);
                return true;
            };
            let (check, rects) = match prechecked.as_ref() {
                Some((checked_rect, res, rects)) if *checked_rect == rect => {
                    (*res, rects.as_ref().clone())
                }
                _ => occlusion_check_with_rects(tray_hwnd, self.hwnd, rect),
            };
            self.apply_occlusion_policy(rect, check, rects)
        } else {
            self.reset_clip();
            if !ensure_just_above_taskbar(self.hwnd, tray_hwnd) {
                self.hide("z_order_failed", None);
                true
            } else {
                false
            }
        }
    }

    fn finish_show(&mut self, tray_hwnd: HWND, allow_input: bool) {
        let positioned = self.in_elevated_band || ensure_just_above_taskbar(self.hwnd, tray_hwnd);
        if !positioned {
            self.hide("z_order_failed", None);
            return;
        }
        let hit_test_ok = self.set_interactive(allow_input);
        if !hit_test_ok {
            self.restore_from_cached_layout = false;
            self.hide("hit_test_failed", None);
            return;
        }
        self.set_visible(true);
        if allow_input && self.last_visible {
            self.restore_from_cached_layout = true;
        }
    }

    #[cfg(test)]
    fn refresh_taskbar_env(
        &mut self,
        config: &Config,
        metrics: &Metrics,
        env: TaskbarEnv,
        force_render: bool,
    ) {
        self.refresh_taskbar_env_with_occlusion(config, metrics, env, force_render, None);
    }

    fn refresh_taskbar_env_with_occlusion(
        &mut self,
        config: &Config,
        metrics: &Metrics,
        env: TaskbarEnv,
        force_render: bool,
        prechecked_occlusion: Option<PrecheckedOcclusion>,
    ) {
        let tray_raw = env.tray_hwnd.0 as isize;
        let prev_geom = self.last_env_geom;
        let geom = (tray_raw, env.taskbar_rect, env.tray_notify_rect, env.dpi);
        let core_geom_changed = prev_geom
            .is_none_or(|(t, bar, _, dpi)| (t, bar, dpi) != (tray_raw, env.taskbar_rect, env.dpi));
        let notify_geom_changed = !core_geom_changed
            && prev_geom.is_some_and(|(_, _, notify, _)| notify != env.tray_notify_rect);
        if core_geom_changed {
            self.last_env_geom = Some(geom);
            self.pending_uia_scan = true;
            self.invalidate_uia_layout = true;
            self.restore_from_cached_layout = false;
            self.last_display = None;
            self.last_anchor_slot = None;
            self.last_compact = false;
        } else if notify_geom_changed {
            self.last_env_geom = Some(geom);
            self.request_layout_scan(LayoutUpdate::Recheck, true);
        }

        if self.pending_uia_scan {
            self.pending_uia_scan = false;
            self.uia_scanner.request_scan(
                tray_raw,
                env.taskbar_rect,
                std::mem::take(&mut self.invalidate_uia_layout),
            );
        }
        let Some(uia_occupied) = self.uia_scanner.snapshot_for(tray_raw, env.taskbar_rect) else {
            self.uia_scanner
                .request_scan(tray_raw, env.taskbar_rect, false);
            self.restore_from_cached_layout = false;
            self.hide("uia_unavailable", None);
            return;
        };

        if !uia_occupied.current {
            if self.last_visible && !notify_geom_changed {
                // A layout hint is not evidence that our pixels collide. Keep
                // the already presented footprint stable while scanning, but
                // never intercept a control that may have moved underneath it.
                if !self.set_interactive(false) {
                    self.restore_from_cached_layout = false;
                    self.hide("hit_test_failed", None);
                    return;
                }
                let _ = self.hide_occluded_if_needed_cached(
                    env.tray_hwnd,
                    self.last_rect(),
                    prechecked_occlusion,
                );
                return;
            }
            if !self.last_visible && !self.restore_from_cached_layout {
                // Hidden windows without a previously confirmed, geometry-matched
                // layout (e.g. startup or after `no_placement_slot`) must wait
                // for fresh UIA validation before appearing.
                if !self.set_interactive(false) {
                    self.hide("hit_test_failed", None);
                }
                return;
            }
        }

        // Repair a taskbar raise before DirectWrite measurement/rasterization.
        // Only reuse our position while fresh geometry and known controls still
        // leave that rectangle clear; otherwise the placement solver runs first.
        if self.last_visible
            && self.last_display.is_some()
            && !core_geom_changed
            && !notify_geom_changed
            && self.last_pos.is_some_and(|(x, y)| {
                let current = PixelRect {
                    left: x,
                    top: y,
                    right: x.saturating_add(self.last_size.0),
                    bottom: y.saturating_add(self.last_size.1),
                };
                uia_occupied.rects.iter().all(|r| !r.intersects(current))
            })
            && self.hide_occluded_if_needed_cached(
                env.tray_hwnd,
                self.last_rect(),
                prechecked_occlusion.clone(),
            )
        {
            return;
        }

        if self.hovered && uia_occupied.current && !cursor_inside_window(self.hwnd) {
            self.hovered = false;
            self.tracking_mouse = false;
        }

        let bounds = LayoutBounds::from_taskbar(env.taskbar_rect, env.dpi);
        let normal_display = build_display(
            config,
            metrics,
            self.light_taskbar,
            self.hovered,
            env.dpi,
            false,
        );
        let Some(renderer) = self.renderer.as_mut() else {
            self.restore_from_cached_layout = false;
            self.hide("no_renderer", None);
            return;
        };
        if force_render {
            renderer.invalidate_rendering_params();
        }

        // Fast path: when display content, DPI, and taskbar bounds are unchanged
        // in normal (non-compact) mode, reuse the known pixel footprint without
        // recreating DirectWrite layouts.
        let cached_normal_placement = if !self.last_compact
            && !force_render
            && !core_geom_changed
            && self.last_display.as_ref() == Some(&normal_display)
            && self.last_size.0 > 0
            && self.last_size.1 > 0
        {
            solve_taskbar_placement(
                env.taskbar_rect,
                env.tray_notify_rect,
                Some(&uia_occupied.rects),
                self.last_size,
                env.dpi,
                self.last_anchor_slot,
                false,
            )
        } else {
            None
        };

        if let Some(placement) = cached_normal_placement {
            self.last_anchor_slot = Some(placement.slot);
            self.last_compact = false;
            let (w, h) = self.last_size;
            let (x, y) = placement.pos;
            let target_rect = PixelRect {
                left: x,
                top: y,
                right: x.saturating_add(w),
                bottom: y.saturating_add(h),
            };
            if self.in_elevated_band {
                let (check, rects) = match prechecked_occlusion.as_ref() {
                    Some((checked_rect, res, rects)) if *checked_rect == target_rect => {
                        (*res, rects.as_ref().clone())
                    }
                    _ => occlusion_check_with_rects(env.tray_hwnd, self.hwnd, target_rect),
                };
                if self.apply_occlusion_policy(target_rect, check, rects) {
                    return;
                }
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
            self.finish_show(env.tray_hwnd, uia_occupied.current);
            return;
        }

        let Some(normal_layout) = renderer.measure_layout(&normal_display, bounds) else {
            self.restore_from_cached_layout = false;
            self.hide("measure_failed", None);
            return;
        };
        let normal_size = (normal_layout.px_w, normal_layout.px_h);

        let same_footprint = self.last_display.as_ref().is_some_and(|d| {
            !core_geom_changed
                && self.last_stacked == normal_layout.stacked
                && d.show_net == normal_display.show_net
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
            Some(&uia_occupied.rects),
            normal_size,
            env.dpi,
            self.last_anchor_slot,
            self.last_compact && same_footprint && !force_render,
        );

        let (display, layout, placement) = if let Some(placement) = normal_placement {
            (normal_display, normal_layout, placement)
        } else {
            let compact_display = build_display(
                config,
                metrics,
                self.light_taskbar,
                self.hovered,
                env.dpi,
                true,
            );
            let Some(compact_layout) = renderer.measure_layout(&compact_display, bounds) else {
                self.restore_from_cached_layout = false;
                self.hide("measure_failed", None);
                return;
            };
            let compact_size = (compact_layout.px_w, compact_layout.px_h);
            let Some(placement) = solve_taskbar_placement(
                env.taskbar_rect,
                env.tray_notify_rect,
                Some(&uia_occupied.rects),
                compact_size,
                env.dpi,
                self.last_anchor_slot,
                false,
            ) else {
                // Taskbar is completely full even in compact form: step out of the way
                // instead of overlapping taskbar buttons or stealing their clicks.
                self.restore_from_cached_layout = false;
                self.hide("no_placement_slot", None);
                return;
            };
            (compact_display, compact_layout, placement)
        };

        self.last_anchor_slot = Some(placement.slot);
        self.last_compact = display.compact;
        let layout_stacked = layout.stacked;
        let (w, h) = (layout.px_w, layout.px_h);
        let (x, y) = placement.pos;
        let target_rect = PixelRect {
            left: x,
            top: y,
            right: x.saturating_add(w),
            bottom: y.saturating_add(h),
        };
        // Resolve the occlusion check before touching renderer state, but apply
        // the policy only after the renderer borrow ends: clipping mutates the
        // window and its tracked occluder (and must run before `finish_show`
        // restores that handle on show).
        let occlusion_check = self
            .in_elevated_band
            .then(|| match prechecked_occlusion.as_ref() {
                Some((checked_rect, res, rects)) if *checked_rect == target_rect => {
                    (*res, rects.as_ref().clone())
                }
                _ => occlusion_check_with_rects(env.tray_hwnd, self.hwnd, target_rect),
            });
        let needs_render = force_render
            || self.last_display.as_ref() != Some(&display)
            || self.last_size != (w, h)
            || self.last_stacked != layout_stacked;

        if needs_render {
            if let Some((rw, rh, src_dc)) =
                renderer.render_layout(&display, layout, Some(self.hwnd.0 as isize))
            {
                if !self.present_layered(x, y, rw, rh, src_dc) {
                    self.restore_from_cached_layout = false;
                    self.last_display = None;
                    self.hide("present_failed", None);
                    return;
                }
                self.last_display = Some(display);
                self.last_size = (rw, rh);
                self.last_stacked = layout_stacked;
                self.last_pos = Some((x, y));
            } else {
                self.restore_from_cached_layout = false;
                self.last_display = None;
                self.hide("render_failed", None);
                return;
            }
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
        if let Some((check, rects)) = occlusion_check
            && self.apply_occlusion_policy(target_rect, check, rects)
        {
            return;
        }
        self.finish_show(env.tray_hwnd, uia_occupied.current);
    }

    fn set_interactive(&self, interactive: bool) -> bool {
        let style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) };
        let next = if interactive {
            style & !(WS_EX_TRANSPARENT.0 as isize)
        } else {
            style | WS_EX_TRANSPARENT.0 as isize
        };
        if style != next {
            unsafe { SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, next) };
        }
        let applied = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } == next;
        if !applied && self.last_visible {
            tracing::warn!("cannot update taskbar monitor hit testing; keeping it hidden");
        }
        applied
    }

    fn accepts_input(&self) -> bool {
        self.last_visible
            && unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TRANSPARENT.0
                == 0
    }

    fn present_layered(&mut self, x: i32, y: i32, w: i32, h: i32, src_dc: HDC) -> bool {
        let pt_dst = POINT { x, y };
        let size = SIZE { cx: w, cy: h };
        let pt_src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let result = unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pt_dst),
                Some(&size),
                Some(src_dc),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        };
        match result {
            Ok(()) => {
                self.last_present_error = None;
                true
            }
            Err(error) => {
                if self
                    .last_present_error
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(30))
                {
                    tracing::warn!(%error, "cannot present taskbar monitor; keeping it hidden");
                    self.last_present_error = Some(Instant::now());
                }
                false
            }
        }
    }

    fn hide(&mut self, reason: &'static str, detail: Option<HWND>) {
        if self.last_visible {
            let class = detail.and_then(read_window_class);
            let band = detail.and_then(window_band);
            let frame = detail.and_then(visible_frame_rect);
            tracing::debug!(reason, ?class, ?band, ?frame, "hiding taskbar monitor");
        }
        // Hiding is mutually exclusive with presenting a clipped region: the
        // system owns the old HRGN and must get it back before SW_HIDE.
        self.reset_clip();
        self.hidden_by_occlusion = reason == "occluded_above_tray";
        self.hidden_by_tray_missing = reason == "tray_missing_in_enum";
        let occluder_raw = if self.hidden_by_occlusion {
            detail.map_or(0, |h| h.0 as isize)
        } else {
            0
        };
        LIVE_OCCLUDER_HWND.store(occluder_raw, std::sync::atomic::Ordering::Release);
        if !self.hidden_by_occlusion {
            self.last_occluded_rect = None;
        } else if self.last_occluded_rect.is_none() {
            self.last_occluded_rect = self.last_rect();
        }
        self.set_visible(false);
    }

    fn set_visible(&mut self, visible: bool) -> bool {
        if visible {
            // A clipped window still tracks the occluder it is clipped around;
            // `set_visible(true)` must not lose that global tracking handle.
            let clip_occluder = self.clip_occluder;
            self.hidden_by_occlusion = false;
            self.hidden_by_tray_missing = false;
            self.last_occluded_rect = None;
            LIVE_OCCLUDER_HWND.store(
                if clip_occluder.0.is_null() {
                    0
                } else {
                    clip_occluder.0 as isize
                },
                std::sync::atomic::Ordering::Release,
            );
        } else {
            self.hovered = false;
            self.tracking_mouse = false;
            if !self.hidden_by_occlusion {
                self.last_occluded_rect = None;
                LIVE_OCCLUDER_HWND.store(0, std::sync::atomic::Ordering::Release);
            }
        }
        // The stacking-only WinEvent path can hide a popup whose safe ordering
        // failed. Reconcile native state instead of trusting our last refresh.
        let native_visible = unsafe { IsWindowVisible(self.hwnd).as_bool() };
        if self.last_visible == visible && native_visible == visible {
            self.sync_topmost_recheck();
            return false;
        }
        if native_visible != visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
        let previous = self.last_visible;
        self.last_visible = unsafe { IsWindowVisible(self.hwnd).as_bool() };
        if self.last_visible != previous {
            tracing::debug!(
                visible = self.last_visible,
                "taskbar monitor visibility changed"
            );
        }
        self.visibility.set(self.last_visible);
        self.sync_topmost_recheck();
        self.last_visible
    }
}

fn context_menu_anchor(
    cursor: POINT,
    taskbar: Option<PixelRect>,
    monitor: Option<PixelRect>,
) -> (POINT, u32) {
    let Some(bar) = taskbar.filter(|b| !b.is_empty()) else {
        return (cursor, TPM_BOTTOMALIGN.0);
    };
    if bar.width() >= bar.height() {
        let is_top = monitor
            .filter(|m| !m.is_empty())
            .map_or(bar.top < 200, |m| bar.top - m.top < m.bottom - bar.bottom);
        if is_top {
            (
                POINT {
                    x: cursor.x,
                    y: bar.bottom,
                },
                TPM_TOPALIGN.0,
            )
        } else {
            (
                POINT {
                    x: cursor.x,
                    y: bar.top,
                },
                TPM_BOTTOMALIGN.0,
            )
        }
    } else {
        let is_left = monitor
            .filter(|m| !m.is_empty())
            .map_or(bar.left < 200, |m| bar.left - m.left < m.right - bar.right);
        if is_left {
            (
                POINT {
                    x: bar.right,
                    y: cursor.y,
                },
                TPM_LEFTALIGN.0,
            )
        } else {
            (
                POINT {
                    x: bar.left,
                    y: cursor.y,
                },
                TPM_RIGHTALIGN.0,
            )
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

fn cursor_inside_window(hwnd: HWND) -> bool {
    let mut cur = POINT::default();
    let mut wr = RECT::default();
    let ok = unsafe { GetCursorPos(&mut cur).is_ok() && GetWindowRect(hwnd, &mut wr).is_ok() };
    ok && cur.x >= wr.left && cur.x < wr.right && cur.y >= wr.top && cur.y < wr.bottom
}

#[derive(Clone, Copy)]
struct TaskbarEnv {
    tray_hwnd: HWND,
    taskbar_rect: PixelRect,
    tray_notify_rect: Option<PixelRect>,
    dpi: u32,
}

enum TaskbarEnvProbe {
    Ready {
        env: TaskbarEnv,
        prechecked_occlusion: Option<PrecheckedOcclusion>,
        topmost_tids: Vec<u32>,
    },
    Fullscreen {
        suppressor: Option<HWND>,
        fg_covers_monitor: bool,
    },
    Unavailable(&'static str),
}

fn probe_taskbar_env(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    occlusion_probe: Option<PixelRect>,
    excluded_tids: Option<&[u32]>,
) -> TaskbarEnvProbe {
    unsafe {
        if tray_hwnd.0.is_null()
            || !IsWindowVisible(tray_hwnd).as_bool()
            || is_window_cloaked(tray_hwnd)
        {
            return TaskbarEnvProbe::Unavailable("tray_unavailable");
        }
        let mut tray_r = RECT::default();
        if GetWindowRect(tray_hwnd, &mut tray_r).is_err() {
            return TaskbarEnvProbe::Unavailable("tray_unavailable");
        }
        let taskbar_rect = PixelRect::from_win32(tray_r);
        if taskbar_rect.is_empty() {
            return TaskbarEnvProbe::Unavailable("tray_unavailable");
        }

        let monitor: HMONITOR = MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut mi).as_bool() {
            return TaskbarEnvProbe::Unavailable("monitor_unavailable");
        }
        let monitor_rect = PixelRect::from_win32(mi.rcMonitor);

        if is_taskbar_autohidden(taskbar_rect, monitor_rect) {
            return TaskbarEnvProbe::Unavailable("tray_autohidden");
        }

        let (fg_fs, fg_covers_monitor, fg_suppressor) =
            fullscreen_covering_monitor_state(monitor, monitor_rect, tray_hwnd, own_hwnd);
        if fg_covers_monitor {
            return TaskbarEnvProbe::Fullscreen {
                suppressor: fg_suppressor,
                fg_covers_monitor: true,
            };
        }

        let summary = scan_above_tray(
            tray_hwnd,
            own_hwnd,
            Some(monitor_rect),
            occlusion_probe,
            excluded_tids,
        );
        if let Some(above_suppressor) = summary.fullscreen_coverer {
            return TaskbarEnvProbe::Fullscreen {
                suppressor: Some(above_suppressor),
                fg_covers_monitor: false,
            };
        }
        if fg_fs {
            return TaskbarEnvProbe::Fullscreen {
                suppressor: fg_suppressor,
                fg_covers_monitor: false,
            };
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
        let occlusion = summary.occlusion;
        let occluder_rects = Arc::new(summary.occluder_rects);
        TaskbarEnvProbe::Ready {
            env: TaskbarEnv {
                tray_hwnd,
                taskbar_rect,
                tray_notify_rect,
                dpi: if dpi == 0 { 96 } else { dpi },
            },
            prechecked_occlusion: occlusion_probe
                .zip(occlusion)
                .map(|(rect, check)| (rect, check, occluder_rects)),
            topmost_tids: summary.topmost_tids,
        }
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

unsafe fn fullscreen_covering_monitor_state(
    taskbar_monitor: HMONITOR,
    monitor_rect: PixelRect,
    tray_hwnd: HWND,
    own_hwnd: HWND,
) -> (bool, bool, Option<HWND>) {
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() || fg == tray_hwnd || fg == own_hwnd {
        return (false, false, None);
    }
    let desktop = unsafe { GetDesktopWindow() };
    let shell = unsafe { GetShellWindow() };
    if fg == desktop || fg == shell {
        return (false, false, None);
    }
    if unsafe { !IsWindowVisible(fg).as_bool() || IsIconic(fg).as_bool() } {
        return (false, false, None);
    }
    if is_window_cloaked(fg) || is_transparent_overlay(fg) {
        return (false, false, None);
    }
    let fg_mon = unsafe { MonitorFromWindow(fg, MONITOR_DEFAULTTONULL) };
    if fg_mon != taskbar_monitor {
        return (false, false, None);
    }
    if is_desktop_shell_class(fg) || is_non_occluding_shell_host_window(fg) {
        return (false, false, None);
    }
    let fg_covers = {
        let mut fg_rect = RECT::default();
        unsafe { GetWindowRect(fg, &mut fg_rect) }.is_ok()
            && PixelRect::from_win32(fg_rect).covers(monitor_rect)
    };
    if fg_covers {
        return (true, true, Some(fg));
    }
    // `SHQueryUserNotificationState` is session-global; only suppress when `fg`
    // is actually ordered above `tray_hwnd` on this monitor (avoiding false
    // hides from secondary-monitor D3D/presentations or post-exit QUNS lag).
    if let Ok(state) = unsafe { SHQueryUserNotificationState() }
        && (state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE)
        && is_window_above(fg, tray_hwnd)
    {
        return (true, false, Some(fg));
    }
    (false, false, None)
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

fn is_below_band(hwnd: HWND, tray_band: Option<u32>) -> bool {
    tray_band.is_some_and(|tb| window_band(hwnd).is_some_and(|wb| wb < tb))
}

fn fullscreen_above_result(saw_tray: bool, tray_band: Option<u32>, covered: bool) -> bool {
    // Windows 11 promotes `Shell_TrayWnd` into band 6 (`ZBID_IMMERSIVE_MOGO`)
    // when Start/Search opens, and `EnumWindows` omits immersive bands from
    // non-immersive callers; windows in bands above `tray_band` still cover it.
    covered && (saw_tray || tray_band.is_some_and(|band| band > 1))
}

struct ClassNameBuf {
    buf: [u16; 64],
    len: usize,
}

impl ClassNameBuf {
    fn read(hwnd: HWND) -> Option<Self> {
        let mut buf = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut buf) };
        (len > 0).then_some(Self {
            buf,
            len: len as usize,
        })
    }

    fn slice(&self) -> &[u16] {
        &self.buf[..self.len]
    }

    fn eq_ascii(&self, s: &str) -> bool {
        self.len == s.len()
            && self
                .slice()
                .iter()
                .zip(s.bytes())
                .all(|(&w, b)| w == u16::from(b))
    }

    fn eq_ascii_ignore_case(&self, s: &str) -> bool {
        self.len == s.len()
            && self
                .slice()
                .iter()
                .zip(s.bytes())
                .all(|(&w, b)| u8::try_from(w).is_ok_and(|c| c.eq_ignore_ascii_case(&b)))
    }

    fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(self.slice())
    }

    #[cfg(test)]
    fn from_ascii(s: &str) -> Self {
        let mut buf = [0u16; 64];
        let len = s.len().min(buf.len());
        for (slot, byte) in buf.iter_mut().zip(s.bytes().take(len)) {
            *slot = u16::from(byte);
        }
        Self { buf, len }
    }
}

fn read_window_class(hwnd: HWND) -> Option<String> {
    ClassNameBuf::read(hwnd).map(|c| c.to_string_lossy())
}

fn is_desktop_shell_class_buf(cls: &ClassNameBuf) -> bool {
    cls.eq_ascii("Progman")
        || cls.eq_ascii("WorkerW")
        || cls.eq_ascii("Shell_TrayWnd")
        || cls.eq_ascii("Shell_SecondaryTrayWnd")
}

fn is_desktop_shell_class(hwnd: HWND) -> bool {
    ClassNameBuf::read(hwnd).is_some_and(|c| is_desktop_shell_class_buf(&c))
}

fn is_tooltip_class_buf(cls: &ClassNameBuf) -> bool {
    cls.eq_ascii_ignore_case("tooltips_class32")
}

fn is_shell_host_overlay_class_buf(cls: &ClassNameBuf) -> bool {
    cls.eq_ascii("Windows.UI.Core.CoreWindow")
        || cls.eq_ascii("XamlExplorerHostIslandWindow")
        || cls.eq_ascii("MultitaskingViewFrame")
}

fn windows_root_dir() -> &'static str {
    static ROOT: OnceLock<String> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut buf = [0u16; 260];
        let len = unsafe { GetSystemWindowsDirectoryW(Some(&mut buf)) } as usize;
        let raw = if len > 0 && len <= buf.len() {
            String::from_utf16_lossy(&buf[..len])
        } else {
            r"C:\Windows".to_string()
        };
        raw.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    })
}

fn process_image_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    unsafe {
        let _ = CloseHandle(handle);
    }
    if !ok || len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

fn is_shell_host_overlay_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "Windows.UI.Core.CoreWindow" | "XamlExplorerHostIslandWindow" | "MultitaskingViewFrame"
    )
}

fn is_shell_host_overlay_identity(class_name: &str, image_path: &str, windows_root: &str) -> bool {
    // Windows 11 Start, Search, tray flyouts, and Task View (`Win+Tab`) use
    // `Windows.UI.Core.CoreWindow`, `XamlExplorerHostIslandWindow`, or
    // `MultitaskingViewFrame` hosts backed by DirectComposition canvases whose
    // HWND bounds can span the screen or overlap the taskbar top edge even though
    // the taskbar remains visible and unobscured.
    if !is_shell_host_overlay_class(class_name) {
        return false;
    }
    let root = windows_root
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    let path = image_path.replace('/', "\\").to_ascii_lowercase();
    let Some(rel) = path.strip_prefix(&root) else {
        return false;
    };
    if rel == r"\explorer.exe" || rel == r"\system32\shellhost.exe" {
        return true;
    }
    let Some(app_rel) = rel.strip_prefix(r"\systemapps\") else {
        return false;
    };
    let Some(file) = app_rel.rsplit('\\').next() else {
        return false;
    };
    matches!(
        file,
        "startmenuexperiencehost.exe"
            | "searchhost.exe"
            | "searchapp.exe"
            | "shellexperiencehost.exe"
    )
}

fn is_non_occluding_shell_host_with_class(hwnd: HWND, cls: &ClassNameBuf) -> bool {
    if !is_shell_host_overlay_class_buf(cls) {
        return false;
    }
    let mut pid = 0u32;
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    let Some(image_path) = process_image_path(pid) else {
        return false;
    };
    let class_name = cls.to_string_lossy();
    is_shell_host_overlay_identity(&class_name, &image_path, windows_root_dir())
}

fn is_non_occluding_shell_host_window(hwnd: HWND) -> bool {
    ClassNameBuf::read(hwnd).is_some_and(|c| is_non_occluding_shell_host_with_class(hwnd, &c))
}

fn is_taskbar_flyout_class(cls: Option<&ClassNameBuf>) -> bool {
    cls.is_some_and(|class| {
        class.eq_ascii_ignore_case("Xaml_WindowedPopupClass")
            || class.eq_ascii_ignore_case("TopLevelWindowForOverflowXamlIsland")
    })
}

/// Windows 11 taskbar flyouts (the right-click menu and tray overflow) render an
/// invisible XAML drop-shadow *inside* their HWND frame. Their frame can extend
/// below the visible menu into the taskbar even though no menu pixels are there.
/// Our widget is placed inside the taskbar, so only the part of such a popup
/// above the taskbar can actually cover it; the rest is shadow and must not be
/// subtracted from the widget region.
fn occluder_frame_for_target(
    frame: PixelRect,
    is_taskbar_flyout: bool,
    taskbar_rect: Option<PixelRect>,
) -> PixelRect {
    if is_taskbar_flyout
        && let Some(taskbar) = taskbar_rect
        && !taskbar.is_empty()
    {
        PixelRect {
            bottom: frame.bottom.min(taskbar.top),
            ..frame
        }
    } else {
        frame
    }
}

fn visible_frame_rect(hwnd: HWND) -> Option<PixelRect> {
    let mut r = RECT::default();
    if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut r as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .is_ok()
    {
        let pr = PixelRect::from_win32(r);
        if !pr.is_empty() {
            return Some(pr);
        }
    }
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok() {
        let pr = PixelRect::from_win32(r);
        if !pr.is_empty() {
            return Some(pr);
        }
    }
    None
}

fn is_potential_occluder_window(
    hwnd: HWND,
    own_hwnd: HWND,
    desktop_hwnd: HWND,
    shell_hwnd: HWND,
) -> bool {
    if hwnd == own_hwnd || hwnd == desktop_hwnd || hwnd == shell_hwnd {
        return false;
    }
    if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
        return false;
    }
    if is_window_cloaked(hwnd) {
        return false;
    }
    let Some(cls) = ClassNameBuf::read(hwnd) else {
        return !is_transparent_overlay(hwnd);
    };
    if is_desktop_shell_class_buf(&cls) || is_non_occluding_shell_host_with_class(hwnd, &cls) {
        return false;
    }
    is_tooltip_class_buf(&cls) || !is_transparent_overlay(hwnd)
}

fn is_topmost_or_elevated_window(hwnd: HWND) -> bool {
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    (ex_style & WS_EX_TOPMOST.0) != 0 || window_band(hwnd).is_some_and(|band| band > 1)
}

fn is_topmost_location_candidate(hwnd: HWND, id_object: i32, id_child: i32) -> bool {
    if id_object != OBJID_WINDOW.0 || id_child != 0 || hwnd.0.is_null() {
        return false;
    }
    let raw = hwnd.0 as isize;
    if raw == LIVE_HWND.load(std::sync::atomic::Ordering::Acquire)
        || raw == LIVE_TRAY_HWND.load(std::sync::atomic::Ordering::Acquire)
    {
        return false;
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    if (style & WS_CHILD.0) != 0 {
        return false;
    }
    if !is_potential_occluder_window(
        hwnd,
        HWND::default(),
        unsafe { GetDesktopWindow() },
        unsafe { GetShellWindow() },
    ) {
        return false;
    }
    is_topmost_or_elevated_window(hwnd)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OcclusionCheck {
    Clear,
    TrayMissing,
    Occluded(HWND),
}

/// Occlusion result together with the occluder frames collected by the *same*
/// enumeration. Reusing them avoids a second `EnumWindows` pass that could
/// observe a different window set and clip against a vanished occluder.
type PrecheckedOcclusion = (PixelRect, OcclusionCheck, Arc<Vec<PixelRect>>);

fn occlusion_check_with_rects(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    target_rect: PixelRect,
) -> (OcclusionCheck, Vec<PixelRect>) {
    let summary =
        scan_above_tray_with_collect(tray_hwnd, own_hwnd, None, Some(target_rect), None, true);
    (
        summary.occlusion.unwrap_or(OcclusionCheck::TrayMissing),
        summary.occluder_rects,
    )
}

fn occlusion_result_after_enum(
    saw_tray: bool,
    tray_band: Option<u32>,
    occluder: Option<HWND>,
) -> OcclusionCheck {
    let tray_accounted = saw_tray || tray_band.is_some_and(|band| band > 1);
    if !tray_accounted {
        OcclusionCheck::TrayMissing
    } else if let Some(hwnd) = occluder {
        OcclusionCheck::Occluded(hwnd)
    } else {
        OcclusionCheck::Clear
    }
}

struct AboveTraySummary {
    topmost_tids: Vec<u32>,
    fullscreen_coverer: Option<HWND>,
    occlusion: Option<OcclusionCheck>,
    occluder_rects: Vec<PixelRect>,
}

struct AboveTrayScan<'a> {
    tray_hwnd: HWND,
    own_hwnd: HWND,
    desktop_hwnd: HWND,
    shell_hwnd: HWND,
    tray_band: Option<u32>,
    monitor_rect: Option<PixelRect>,
    taskbar_rect: Option<PixelRect>,
    probe_rect: Option<PixelRect>,
    excluded_tids: Option<&'a [u32]>,
    collect_rects: bool,
    topmost_tids: Vec<u32>,
    fullscreen_coverer: Option<HWND>,
    occluder: Option<HWND>,
    occluder_rects: Vec<PixelRect>,
    saw_tray: bool,
}

unsafe extern "system" fn above_tray_enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = unsafe { &mut *(lparam.0 as *mut AboveTrayScan<'_>) };
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
    if is_below_band(hwnd, scan.tray_band) || is_window_cloaked(hwnd) {
        return BOOL(1);
    }
    let cls = ClassNameBuf::read(hwnd);
    if let Some(ref c) = cls
        && (is_desktop_shell_class_buf(c) || is_non_occluding_shell_host_with_class(hwnd, c))
    {
        return BOOL(1);
    }
    let is_tooltip = cls.as_ref().is_some_and(is_tooltip_class_buf);
    let transparent = is_transparent_overlay(hwnd);

    if let Some(mon_rect) = scan.monitor_rect
        && scan.fullscreen_coverer.is_none()
        && !transparent
    {
        let mut r = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok()
            && PixelRect::from_win32(r).covers(mon_rect)
        {
            scan.fullscreen_coverer = Some(hwnd);
        }
    }

    if is_tooltip || !transparent {
        if let Some(excluded) = scan.excluded_tids
            && is_topmost_or_elevated_window(hwnd)
        {
            let tid = unsafe { GetWindowThreadProcessId(hwnd, None) };
            if tid != 0 && !excluded.contains(&tid) && !scan.topmost_tids.contains(&tid) {
                scan.topmost_tids.push(tid);
            }
        }
        if let Some(target_rect) = scan.probe_rect
            && !target_rect.is_empty()
            && (scan.collect_rects || scan.occluder.is_none())
            && let Some(frame) = visible_frame_rect(hwnd)
        {
            let frame = occluder_frame_for_target(
                frame,
                is_taskbar_flyout_class(cls.as_ref()),
                scan.taskbar_rect,
            );
            if !frame.is_empty() && frame.intersects(target_rect) {
                if scan.occluder.is_none() {
                    scan.occluder = Some(hwnd);
                }
                if scan.collect_rects && !scan.occluder_rects.contains(&frame) {
                    scan.occluder_rects.push(frame);
                }
            }
        }
    }
    BOOL(1)
}

fn scan_above_tray_in_band(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    tray_band: Option<u32>,
    monitor_rect: Option<PixelRect>,
    probe_rect: Option<PixelRect>,
    excluded_tids: Option<&[u32]>,
    collect_rects: bool,
) -> (AboveTraySummary, bool) {
    if !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tray_hwnd)).as_bool() } {
        return (
            AboveTraySummary {
                topmost_tids: Vec::new(),
                fullscreen_coverer: None,
                occlusion: probe_rect.map(|_| OcclusionCheck::TrayMissing),
                occluder_rects: Vec::new(),
            },
            false,
        );
    }
    let taskbar_rect = {
        let mut r = RECT::default();
        unsafe { GetWindowRect(tray_hwnd, &mut r) }
            .ok()
            .map(|_| PixelRect::from_win32(r))
            .filter(|rect| !rect.is_empty())
    };
    let mut scan = AboveTrayScan {
        tray_hwnd,
        own_hwnd,
        desktop_hwnd: unsafe { GetDesktopWindow() },
        shell_hwnd: unsafe { GetShellWindow() },
        tray_band,
        monitor_rect,
        taskbar_rect,
        probe_rect,
        excluded_tids,
        collect_rects,
        topmost_tids: Vec::new(),
        fullscreen_coverer: None,
        occluder: None,
        occluder_rects: Vec::new(),
        saw_tray: false,
    };
    unsafe {
        let _ = EnumWindows(
            Some(above_tray_enum_cb),
            LPARAM(&mut scan as *mut AboveTrayScan<'_> as isize),
        );
    }
    let fullscreen_coverer = fullscreen_above_result(
        scan.saw_tray,
        scan.tray_band,
        scan.fullscreen_coverer.is_some(),
    )
    .then_some(scan.fullscreen_coverer)
    .flatten();
    let occlusion = probe_rect.map(|r| {
        if r.is_empty() {
            OcclusionCheck::TrayMissing
        } else {
            occlusion_result_after_enum(scan.saw_tray, scan.tray_band, scan.occluder)
        }
    });
    (
        AboveTraySummary {
            topmost_tids: scan.topmost_tids,
            fullscreen_coverer,
            occlusion,
            occluder_rects: scan.occluder_rects,
        },
        scan.saw_tray,
    )
}

fn scan_above_tray(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    monitor_rect: Option<PixelRect>,
    probe_rect: Option<PixelRect>,
    excluded_tids: Option<&[u32]>,
) -> AboveTraySummary {
    // Any scan with a probe rect feeds the occlusion policy, which needs the
    // occluder frames from that exact enumeration to clip deterministically.
    scan_above_tray_with_collect(
        tray_hwnd,
        own_hwnd,
        monitor_rect,
        probe_rect,
        excluded_tids,
        probe_rect.is_some(),
    )
}

/// Same enumeration as [`scan_above_tray`], optionally gathering every
/// intersecting occluder frame so a partially covered widget can be clipped
/// instead of hidden. Rect collection is only enabled on that rare path.
fn scan_above_tray_with_collect(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    monitor_rect: Option<PixelRect>,
    probe_rect: Option<PixelRect>,
    excluded_tids: Option<&[u32]>,
    collect_rects: bool,
) -> AboveTraySummary {
    let initial_band = window_band(tray_hwnd);
    let (summary, saw_tray) = scan_above_tray_in_band(
        tray_hwnd,
        own_hwnd,
        initial_band,
        monitor_rect,
        probe_rect,
        excluded_tids,
        collect_rects,
    );
    if !saw_tray {
        let current_band = window_band(tray_hwnd);
        if current_band != initial_band {
            return scan_above_tray_in_band(
                tray_hwnd,
                own_hwnd,
                current_band,
                monitor_rect,
                probe_rect,
                excluded_tids,
                collect_rects,
            )
            .0;
        }
    }
    summary
}

#[cfg(test)]
fn fullscreen_window_above_taskbar_in_band(
    monitor_rect: PixelRect,
    tray_hwnd: HWND,
    own_hwnd: HWND,
    tray_band: Option<u32>,
) -> Option<HWND> {
    scan_above_tray_in_band(
        tray_hwnd,
        own_hwnd,
        tray_band,
        Some(monitor_rect),
        None,
        None,
        false,
    )
    .0
    .fullscreen_coverer
}

#[cfg(test)]
fn is_fullscreen_window_above_taskbar_in_band(
    monitor_rect: PixelRect,
    tray_hwnd: HWND,
    own_hwnd: HWND,
    tray_band: Option<u32>,
) -> bool {
    fullscreen_window_above_taskbar_in_band(monitor_rect, tray_hwnd, own_hwnd, tray_band).is_some()
}

#[cfg(test)]
fn is_fullscreen_window_above_taskbar(
    monitor_rect: PixelRect,
    tray_hwnd: HWND,
    own_hwnd: HWND,
) -> bool {
    scan_above_tray(tray_hwnd, own_hwnd, Some(monitor_rect), None, None)
        .fullscreen_coverer
        .is_some()
}

#[cfg(test)]
fn check_rect_occluded_above_taskbar_in_band(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    target_rect: PixelRect,
    tray_band: Option<u32>,
) -> OcclusionCheck {
    scan_above_tray_in_band(
        tray_hwnd,
        own_hwnd,
        tray_band,
        None,
        Some(target_rect),
        None,
        false,
    )
    .0
    .occlusion
    .unwrap_or(OcclusionCheck::TrayMissing)
}

#[cfg(test)]
fn check_rect_occluded_above_taskbar(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    target_rect: PixelRect,
) -> OcclusionCheck {
    scan_above_tray(tray_hwnd, own_hwnd, None, Some(target_rect), None)
        .occlusion
        .unwrap_or(OcclusionCheck::TrayMissing)
}

/// Detects visible, uncloaked, non-transparent windows above `tray_hwnd` (such as
/// popup menus, tooltips, or always-on-top apps) whose visible frame overlaps
/// `target_rect`. Used in elevated window band 16 so the widget never floats on
/// top of overlapping popups or always-on-top windows.
#[cfg(test)]
fn is_rect_occluded_above_taskbar(tray_hwnd: HWND, own_hwnd: HWND, target_rect: PixelRect) -> bool {
    check_rect_occluded_above_taskbar(tray_hwnd, own_hwnd, target_rect) != OcclusionCheck::Clear
}

#[cfg(test)]
fn collect_topmost_occluder_tids(tray_hwnd: HWND, own_hwnd: HWND, excluded: &[u32]) -> Vec<u32> {
    scan_above_tray(tray_hwnd, own_hwnd, None, None, Some(excluded)).topmost_tids
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
/// Unknown or empty UIA occupancy keeps the widget hidden.
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

    let uia_rects = uia_occupied.filter(|s| !s.is_empty())?;

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
    let (cross_pos, widget_len, cross_avail, widget_cross) = if horizontal {
        (
            taskbar_rect.top + (taskbar_rect.height() - h) / 2,
            w,
            taskbar_rect.height(),
            h,
        )
    } else {
        (
            taskbar_rect.left + (taskbar_rect.width() - w) / 2,
            h,
            taskbar_rect.width(),
            w,
        )
    };
    if widget_cross > cross_avail {
        return None;
    }
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
        return None;
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
                let _ = uia2.SetConnectionTimeout(150);
                let _ = uia2.SetTransactionTimeout(250);
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
        collect_cached_rects(len, |i| unsafe {
            let elem = found.GetElement(i)?;
            elem.CachedBoundingRectangle()
        })
    }
}

fn collect_cached_rects<I>(len: i32, mut read_rect: I) -> windows::core::Result<Vec<PixelRect>>
where
    I: FnMut(i32) -> windows::core::Result<RECT>,
{
    let mut rects = Vec::with_capacity(len.max(0) as usize);
    for i in 0..len {
        // Propagate any per-element cache/COM error so a failed rectangle read
        // triggers `uia_worker_main`'s retry/backoff path instead of publishing
        // a partial obstacle list that could place the widget over an omitted
        // taskbar button and intercept its clicks.
        let rc = read_rect(i)?;
        let px = PixelRect::from_win32(rc);
        if !px.is_empty() {
            rects.push(px);
        }
    }
    rects.sort_unstable_by_key(|r| (r.left, r.top, r.right, r.bottom));
    rects.dedup();
    Ok(rects)
}

#[derive(Default)]
struct UiaCache {
    tray_hwnd: isize,
    taskbar_rect: PixelRect,
    rects: Option<Vec<PixelRect>>,
    generation: u64,
}

struct UiaSnapshot {
    rects: Vec<PixelRect>,
    current: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct UiaTarget {
    tray_hwnd: isize,
    taskbar_rect: PixelRect,
    generation: u64,
}

#[derive(Default)]
struct UiaShared {
    pending: Option<UiaTarget>,
    cache: UiaCache,
    generation: u64,
    min_valid_generation: u64,
    shutdown: bool,
    consecutive_failures: u32,
    backoff_until: Option<Instant>,
    last_target: Option<(isize, PixelRect)>,
}

fn uia_failure_backoff(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    let idx =
        (consecutive_failures.saturating_sub(1) as usize).min(UIA_FAILURE_BACKOFF_STEPS.len() - 1);
    UIA_FAILURE_BACKOFF_STEPS[idx]
}

impl UiaShared {
    fn next_wait(&self, last_scan: Option<Instant>, now: Instant) -> Option<Duration> {
        let wait_until = last_scan
            .map(|prev| prev + UIA_MIN_SCAN_INTERVAL)
            .into_iter()
            .chain(self.backoff_until)
            .max()?;
        (now < wait_until).then(|| wait_until - now)
    }

    fn publish(&mut self, target: UiaTarget, rects: Option<Vec<PixelRect>>) -> bool {
        self.publish_at(target, rects, Instant::now())
    }

    fn publish_at(
        &mut self,
        target: UiaTarget,
        rects: Option<Vec<PixelRect>>,
        now: Instant,
    ) -> bool {
        if self.shutdown {
            return false;
        }
        let rects = rects.filter(|r| !r.is_empty());
        if target.generation != self.generation {
            // If an in-flight scan started after the latest invalidation/target
            // change finishes with valid rectangles while `cache.rects` is still
            // empty (e.g. startup or post-resize burst), populate `cache.rects`
            // with `target.generation` (`current == false`) instead of starving
            // the cache across back-to-back recheck requests.
            let valid_for_current_target = self.last_target
                == Some((target.tray_hwnd, target.taskbar_rect))
                && target.generation.wrapping_sub(self.min_valid_generation)
                    <= self.generation.wrapping_sub(self.min_valid_generation);
            if valid_for_current_target && self.cache.rects.is_none() && rects.is_some() {
                self.consecutive_failures = 0;
                self.backoff_until = None;
                self.cache = UiaCache {
                    tray_hwnd: target.tray_hwnd,
                    taskbar_rect: target.taskbar_rect,
                    rects,
                    generation: target.generation,
                };
                return true;
            }
            return false;
        }
        let same_target = self.cache.tray_hwnd == target.tray_hwnd
            && self.cache.taskbar_rect == target.taskbar_rect;
        let had_rects_for_target = same_target && self.cache.rects.is_some();
        let changed = if rects.is_some() {
            self.consecutive_failures = 0;
            self.backoff_until = None;
            if self
                .pending
                .is_some_and(|pending| pending.generation == target.generation)
            {
                self.pending = None;
            }
            !same_target || self.cache.rects != rects || self.cache.generation != target.generation
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            self.backoff_until = Some(now + uia_failure_backoff(self.consecutive_failures));
            // Keep retry ownership inside the UIA worker across failures so recovery
            // continues after backoff even when sampling is paused and `changed == false`.
            if self.pending.is_none() && target.tray_hwnd != 0 && !target.taskbar_rect.is_empty() {
                self.pending = Some(target);
            }
            // Only wake the window if previously cached rectangles for this target
            // must be cleared; repeated unavailable scans must not spin refresh.
            had_rects_for_target
        };
        self.cache = UiaCache {
            tray_hwnd: target.tray_hwnd,
            taskbar_rect: target.taskbar_rect,
            rects,
            generation: target.generation,
        };
        changed
    }
}

/// Scans `Shell_TrayWnd` UI Automation bounding rectangles on a dedicated MTA
/// thread so cross-process UIA COM calls never pump messages or block `"tm-taskbar"`.
struct UiaScanner {
    state: Arc<(Mutex<UiaShared>, Condvar)>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl UiaScanner {
    fn start() -> Self {
        let state = Arc::new((Mutex::new(UiaShared::default()), Condvar::new()));
        let worker_state = state.clone();
        let handle = match std::thread::Builder::new()
            .name("tm-taskbar-uia".to_string())
            .spawn(move || uia_worker_main(worker_state))
        {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::warn!(%error, "cannot start taskbar layout scanner; monitor hidden");
                None
            }
        };
        Self {
            state,
            worker: Mutex::new(handle),
        }
    }

    fn snapshot_for(&self, tray_raw: isize, taskbar_rect: PixelRect) -> Option<UiaSnapshot> {
        let guard = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
        if guard.cache.tray_hwnd == tray_raw && guard.cache.taskbar_rect == taskbar_rect {
            guard.cache.rects.clone().map(|rects| UiaSnapshot {
                rects,
                current: guard.cache.generation == guard.generation,
            })
        } else {
            None
        }
    }

    fn request_scan(&self, tray_raw: isize, taskbar_rect: PixelRect, invalidate: bool) {
        if tray_raw == 0 || taskbar_rect.is_empty() {
            return;
        }
        let mut guard = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
        if guard.shutdown {
            return;
        }
        let target_key = (tray_raw, taskbar_rect);
        let target_changed = guard.last_target != Some(target_key);
        if target_changed {
            guard.last_target = Some(target_key);
            guard.consecutive_failures = 0;
            guard.backoff_until = None;
        }
        if invalidate {
            guard.cache.rects = None;
        }
        // If occupancy is already unavailable and a retry is already pending for
        // the same target, avoid churning `generation` or waking the worker.
        if guard.cache.rects.is_none() && !target_changed && !invalidate && guard.pending.is_some()
        {
            return;
        }
        // Even a recheck supersedes an in-flight scan. Publishing unchanged
        // rectangles must wake the window so it can restore hit testing.
        guard.generation = guard.generation.wrapping_add(1);
        if target_changed || invalidate {
            guard.min_valid_generation = guard.generation;
        }
        guard.pending = Some(UiaTarget {
            tray_hwnd: tray_raw,
            taskbar_rect,
            generation: guard.generation,
        });
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
    let mut session: Option<UiaSession> = None;
    let mut last_scan: Option<Instant> = None;
    let mut last_failure_report: Option<Instant> = None;

    loop {
        let target = {
            let mut guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if guard.shutdown {
                    return;
                }
                if let Some(target) = guard.pending {
                    if let Some(wait) = guard.next_wait(last_scan, Instant::now()) {
                        let (g, _) = state
                            .1
                            .wait_timeout(guard, wait)
                            .unwrap_or_else(|e| e.into_inner());
                        guard = g;
                        continue;
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
        let mut scan_error: Option<windows::core::Error> = None;
        let mut result = match session.as_ref() {
            Some(s) => match s.scan_tray(target.tray_hwnd) {
                Ok(rects) => Ok(rects),
                Err(err) => {
                    session = None;
                    scan_error = Some(err);
                    Err(())
                }
            },
            None => Err(()),
        };

        // If the first attempt hit a transient COM/RPC error while previously
        // healthy, re-initialize the UIA session and retry once immediately
        // before clearing the layout cache and entering progressive backoff.
        if result.is_err() && scan_error.is_some() {
            let should_retry_immediately = {
                let guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
                !guard.shutdown && guard.consecutive_failures == 0 && guard.pending.is_none()
            };
            if should_retry_immediately {
                session = UiaSession::new().ok();
                if let Some(s) = session.as_ref() {
                    match s.scan_tray(target.tray_hwnd) {
                        Ok(rects) => {
                            scan_error = None;
                            result = Ok(rects);
                        }
                        Err(err) => {
                            session = None;
                            scan_error = Some(err);
                        }
                    }
                }
            }
        }

        let rects = result.ok().filter(|r| !r.is_empty());
        if rects.is_none() {
            if last_failure_report.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
                tracing::warn!(
                    error = ?scan_error,
                    "taskbar layout unavailable; monitor hidden until UI Automation recovers"
                );
                last_failure_report = Some(Instant::now());
            }
        } else {
            last_failure_report = None;
        }
        let changed = state
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish(target, rects);
        if changed {
            post_refresh();
        }
    }
}

struct ZOrderScan {
    target: HWND,
    anchor: HWND,
    saw_target: bool,
    saw_anchor: bool,
}

unsafe extern "system" fn z_order_enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = unsafe { &mut *(lparam.0 as *mut ZOrderScan) };
    if hwnd == scan.anchor {
        scan.saw_anchor = true;
        return BOOL(0);
    }
    if hwnd == scan.target {
        scan.saw_target = true;
    }
    BOOL(1)
}

fn is_window_above(target: HWND, anchor: HWND) -> bool {
    let mut scan = ZOrderScan {
        target,
        anchor,
        saw_target: false,
        saw_anchor: false,
    };
    unsafe {
        let _ = EnumWindows(
            Some(z_order_enum_cb),
            LPARAM(&mut scan as *mut ZOrderScan as isize),
        );
    }
    scan.saw_anchor && scan.saw_target
}

struct TaskbarZOrder {
    tray: HWND,
    widget: HWND,
    topmost: bool,
    tray_band: Option<u32>,
    predecessor: Option<HWND>,
    saw_tray: bool,
    saw_widget: bool,
    visible_after_widget: bool,
}

unsafe extern "system" fn taskbar_z_order_enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = unsafe { &mut *(lparam.0 as *mut TaskbarZOrder) };
    if hwnd == scan.tray {
        scan.saw_tray = true;
        return BOOL(0);
    }
    if hwnd == scan.widget {
        scan.saw_widget = true;
    } else if unsafe { IsWindowVisible(hwnd).as_bool() }
        && !is_window_cloaked(hwnd)
        && ((unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0) != 0)
            == scan.topmost
        && (scan.tray_band.is_none() || window_band(hwnd) == scan.tray_band)
    {
        scan.predecessor = Some(hwnd);
        scan.visible_after_widget |= scan.saw_widget;
    }
    BOOL(1)
}

fn hide_and_demote_widget(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOACTIVATE
                | SWP_NOOWNERZORDER
                | SWP_NOSENDCHANGING
                | SWP_HIDEWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// Keeps `widget_hwnd` above `tray_hwnd`, never climbing in front of visible
/// popup menus, tray flyouts, or other programs' topmost windows, and never
/// remaining pinned as `TOPMOST` if Explorer itself was demoted behind a
/// fullscreen window.
fn ensure_just_above_taskbar(widget_hwnd: HWND, tray_hwnd: HWND) -> bool {
    let tray_topmost =
        (unsafe { GetWindowLongPtrW(tray_hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0) != 0;
    let widget_topmost =
        (unsafe { GetWindowLongPtrW(widget_hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0) != 0;
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING;

    if !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tray_hwnd)).as_bool() } {
        hide_and_demote_widget(widget_hwnd);
        return false;
    }
    let prev = unsafe { GetWindow(tray_hwnd, GW_HWNDPREV) }.unwrap_or_default();
    if prev == widget_hwnd && widget_topmost == tray_topmost {
        return true;
    }
    // Hidden bookkeeping windows (e.g. ForegroundStaging) may be inaccessible
    // insertion anchors. They have no visible content to preserve. Enumerate
    // without a fixed cap, and retain ordering below every visible peer instead.
    let mut scan = TaskbarZOrder {
        tray: tray_hwnd,
        widget: widget_hwnd,
        topmost: tray_topmost,
        tray_band: window_band(tray_hwnd),
        predecessor: None,
        saw_tray: false,
        saw_widget: false,
        visible_after_widget: false,
    };
    unsafe {
        let _ = EnumWindows(
            Some(taskbar_z_order_enum_cb),
            LPARAM(&mut scan as *mut TaskbarZOrder as isize),
        );
    }
    if !scan.saw_tray {
        hide_and_demote_widget(widget_hwnd);
        return false;
    }
    if scan.saw_widget && !scan.visible_after_widget && widget_topmost == tray_topmost {
        return true;
    }
    // At the top of the selected band use its marker; never inherit another
    // band's predecessor. No operation changes Explorer's own window or owner.
    let insert_after = if let Some(predecessor) = scan.predecessor {
        predecessor
    } else if tray_topmost {
        HWND_TOPMOST
    } else if widget_topmost {
        HWND_NOTOPMOST
    } else {
        HWND_TOP
    };
    match unsafe { SetWindowPos(widget_hwnd, Some(insert_after), 0, 0, 0, 0, flags) } {
        Ok(()) => true,
        Err(error) => {
            // An inaccessible predecessor (e.g. a higher-integrity window) is not
            // permission to raise above it. Hide and demote only our own popup.
            let was_visible = unsafe { IsWindowVisible(widget_hwnd).as_bool() };
            hide_and_demote_widget(widget_hwnd);
            if was_visible {
                tracing::warn!(%error, "cannot position taskbar monitor; keeping it hidden");
            }
            false
        }
    }
}

/// Repair a taskbar raise in its WinEvent delivery, before posting any heavier
/// work. Only already visible popups participate; fullscreen/auto-hide checks
/// and all rendering remain in refresh. Keeping immediately above the tray also
/// preserves the order below windows that Explorer itself sits behind.
fn repair_visible_taskbar_order(widget_hwnd: HWND, tray_hwnd: HWND) {
    if !widget_hwnd.0.is_null()
        && !tray_hwnd.0.is_null()
        && unsafe { IsWindowVisible(widget_hwnd).as_bool() && IsWindowVisible(tray_hwnd).as_bool() }
    {
        ensure_just_above_taskbar(widget_hwnd, tray_hwnd);
    }
}

fn repair_live_taskbar_order() {
    use std::sync::atomic::Ordering;
    if IN_ELEVATED_BAND.load(Ordering::Acquire) {
        return;
    }
    let widget = LIVE_HWND.load(Ordering::Acquire);
    let tray = LIVE_TRAY_HWND.load(Ordering::Acquire);
    repair_visible_taskbar_order(
        HWND(widget as *mut std::ffi::c_void),
        HWND(tray as *mut std::ffi::c_void),
    );
}

fn read_hkcu_dword(subkey: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut std::ffi::c_void),
            Some(&mut size),
        )
    };
    rc.is_ok().then_some(data)
}

fn is_rgb_light([r, g, b]: [u8; 3]) -> bool {
    // Matches Windows Shell's `IsColorLight` luminance threshold.
    (5 * u32::from(g) + 2 * u32::from(r) + u32::from(b)) > 8 * 128
}

fn resolve_light_taskbar(
    system_uses_light: bool,
    color_prevalence: bool,
    accent_rgb: Option<[u8; 3]>,
) -> bool {
    if color_prevalence && let Some(rgb) = accent_rgb {
        return is_rgb_light(rgb);
    }
    system_uses_light
}

fn read_taskbar_is_light() -> bool {
    let personalize = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let dwm = w!("Software\\Microsoft\\Windows\\DWM");
    let system_uses_light =
        read_hkcu_dword(personalize, w!("SystemUsesLightTheme")).is_some_and(|v| v != 0);
    let color_prevalence =
        read_hkcu_dword(personalize, w!("ColorPrevalence")).is_some_and(|v| v != 0);
    // `DWM\AccentColor` is stored as 0xAABBGGRR; `ColorizationColor` is 0xAARRGGBB.
    let accent_rgb = read_hkcu_dword(dwm, w!("AccentColor"))
        .map(|raw| [raw as u8, (raw >> 8) as u8, (raw >> 16) as u8])
        .or_else(|| {
            read_hkcu_dword(dwm, w!("ColorizationColor"))
                .map(|raw| [(raw >> 16) as u8, (raw >> 8) as u8, raw as u8])
        });
    resolve_light_taskbar(system_uses_light, color_prevalence, accent_rgb)
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
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if event == EVENT_OBJECT_DESTROY {
        if id_object != OBJID_WINDOW.0 || id_child != 0 || hwnd.0.is_null() {
            return;
        }
        let raw = hwnd.0 as isize;
        let is_fs = raw == LIVE_FS_HWND.load(std::sync::atomic::Ordering::Acquire);
        let is_occluder = raw == LIVE_OCCLUDER_HWND.load(std::sync::atomic::Ordering::Acquire);
        if !is_fs && !is_occluder {
            return;
        }
        if is_fs {
            FULLSCREEN_WAKE_PENDING.store(true, std::sync::atomic::Ordering::Release);
        }
        post_refresh();
        return;
    }
    if matches!(
        event,
        EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED | EVENT_OBJECT_UNCLOAKED
    ) && (id_object != OBJID_WINDOW.0
        || id_child != 0
        || hwnd.0.is_null()
        || (unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32 & WS_CHILD.0) != 0)
    {
        return;
    }
    let tray_raw = LIVE_TRAY_HWND.load(std::sync::atomic::Ordering::Acquire);
    if event == EVENT_SYSTEM_FOREGROUND && hwnd.0 as isize == tray_raw {
        repair_live_taskbar_order();
    }
    if event == EVENT_SYSTEM_MOVESIZEEND && tray_raw != 0 && hwnd.0 as isize == tray_raw {
        LAYOUT_RECHECK_PENDING.store(true, std::sync::atomic::Ordering::Release);
    }
    if matches!(
        event,
        EVENT_SYSTEM_FOREGROUND
            | EVENT_SYSTEM_MINIMIZESTART
            | EVENT_SYSTEM_MINIMIZEEND
            | EVENT_OBJECT_HIDE
            | EVENT_OBJECT_CLOAKED
            | EVENT_OBJECT_UNCLOAKED
    ) {
        FULLSCREEN_WAKE_PENDING.store(true, std::sync::atomic::Ordering::Release);
    }
    post_refresh();
}

unsafe extern "system" fn fs_location_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if id_object == OBJID_WINDOW.0
        && id_child == 0
        && !hwnd.0.is_null()
        && hwnd.0 as isize == LIVE_FS_HWND.load(std::sync::atomic::Ordering::Acquire)
    {
        if event != EVENT_OBJECT_LOCATIONCHANGE {
            FULLSCREEN_WAKE_PENDING.store(true, std::sync::atomic::Ordering::Release);
        }
        post_refresh();
    }
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
        && (hwnd.0 as isize == LIVE_FG_HWND.load(std::sync::atomic::Ordering::Acquire)
            || (IN_ELEVATED_BAND.load(std::sync::atomic::Ordering::Acquire)
                && is_topmost_location_candidate(hwnd, id_object, id_child)))
    {
        post_refresh();
    }
}

unsafe extern "system" fn topmost_location_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if event == EVENT_OBJECT_LOCATIONCHANGE
        && IN_ELEVATED_BAND.load(std::sync::atomic::Ordering::Acquire)
        && is_topmost_location_candidate(hwnd, id_object, id_child)
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
    if is_tray_or_child && taskbar_event_layout_update(event, id_object, id_child).is_some() {
        if hwnd == tray_hwnd {
            repair_live_taskbar_order();
        }
        LAYOUT_RECHECK_PENDING.store(true, std::sync::atomic::Ordering::Release);
        post_refresh();
    } else if event == EVENT_OBJECT_LOCATIONCHANGE
        && !is_tray_or_child
        && IN_ELEVATED_BAND.load(std::sync::atomic::Ordering::Acquire)
        && is_topmost_location_candidate(hwnd, id_object, id_child)
    {
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

type CreateWindowInBandFn = unsafe extern "system" fn(
    u32,
    PCWSTR,
    PCWSTR,
    u32,
    i32,
    i32,
    i32,
    i32,
    HWND,
    HMENU,
    HINSTANCE,
    *const std::ffi::c_void,
    u32,
) -> HWND;
type GetWindowBandFn = unsafe extern "system" fn(HWND, *mut u32) -> BOOL;

#[derive(Clone, Copy)]
struct BandExports {
    create_in_band: Option<CreateWindowInBandFn>,
    get_band: Option<GetWindowBandFn>,
}

fn band_exports() -> BandExports {
    static EXPORTS: OnceLock<BandExports> = OnceLock::new();
    *EXPORTS.get_or_init(|| unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else {
            return BandExports {
                create_in_band: None,
                get_band: None,
            };
        };
        let create_in_band = GetProcAddress(user32, s!("CreateWindowInBand")).map(|p| {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateWindowInBandFn>(p)
        });
        let get_band = GetProcAddress(user32, s!("GetWindowBand")).map(|p| {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, GetWindowBandFn>(p)
        });
        BandExports {
            create_in_band,
            get_band,
        }
    })
}

fn window_band(hwnd: HWND) -> Option<u32> {
    let get_band = band_exports().get_band?;
    let mut band = 0u32;
    let ok = unsafe { get_band(hwnd, &mut band) };
    ok.as_bool().then_some(band)
}

fn create_monitor_window(
    hinstance: HINSTANCE,
    class_name: PCWSTR,
    state: *const WndProcState,
) -> windows::core::Result<HWND> {
    // Cross-thread ownership attaches input queues just like SetParent does.
    // Explorer must never be our owner, including while rebinding after restart.
    unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            w!("TaskMan Taskbar Monitor"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinstance),
            Some(state.cast()),
        )
    }
}

fn create_elevated_monitor_window(
    hinstance: HINSTANCE,
    class_name: PCWSTR,
    state: *const WndProcState,
) -> windows::core::Result<(HWND, bool)> {
    // Place the widget in `ZBID_SYSTEM_TOOLS` (band 16) so band-1 taskbar
    // activation cannot raise `Shell_TrayWnd` above it for even a single frame,
    // without touching Explorer ownership or input queues. Fall back cleanly
    // to standard band 1 when `CreateWindowInBand` is unavailable or denied.
    if let Some(create_in_band) = band_exports().create_in_band {
        let hwnd = unsafe {
            create_in_band(
                (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0,
                class_name,
                w!("TaskMan Taskbar Monitor"),
                WS_POPUP.0,
                0,
                0,
                1,
                1,
                HWND::default(),
                HMENU::default(),
                hinstance,
                state.cast(),
                TASKBAR_MONITOR_BAND,
            )
        };
        if !hwnd.0.is_null() {
            let in_elevated_band = window_band(hwnd) == Some(TASKBAR_MONITOR_BAND);
            return Ok((hwnd, in_elevated_band));
        }
        let error = windows::core::Error::from_thread();
        tracing::warn!(%error, "CreateWindowInBand failed; falling back to band 1");
    }
    create_monitor_window(hinstance, class_name, state).map(|hwnd| (hwnd, false))
}

struct WndProcState {
    msg_taskbar_created: u32,
    msg_shell_hook: u32,
    ctx: RefCell<WindowCtx>,
    menu_open: Cell<bool>,
    reentrant_refresh: Cell<bool>,
    reentrant_force_render: Cell<bool>,
    reentrant_layout_update: Cell<Option<LayoutUpdate>>,
    reentrant_theme_dirty: Cell<bool>,
    reentrant_close: Cell<bool>,
}

struct MenuOpenGuard<'a>(&'a Cell<bool>);

impl Drop for MenuOpenGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl WndProcState {
    fn new(msg_taskbar_created: u32, msg_shell_hook: u32, ctx: WindowCtx) -> Self {
        Self {
            msg_taskbar_created,
            msg_shell_hook,
            ctx: RefCell::new(ctx),
            menu_open: Cell::new(false),
            reentrant_refresh: Cell::new(false),
            reentrant_force_render: Cell::new(false),
            reentrant_layout_update: Cell::new(None),
            reentrant_theme_dirty: Cell::new(false),
            reentrant_close: Cell::new(false),
        }
    }

    fn queue_reentrant_layout(&self, update: LayoutUpdate) {
        let next = match (self.reentrant_layout_update.get(), update) {
            (Some(LayoutUpdate::Invalidate), _) | (_, LayoutUpdate::Invalidate) => {
                LayoutUpdate::Invalidate
            }
            _ => LayoutUpdate::Recheck,
        };
        self.reentrant_layout_update.set(Some(next));
    }

    fn with_ctx_mut(&self, hwnd: HWND, f: impl FnOnce(&mut WindowCtx)) -> bool {
        let Ok(mut ctx) = self.ctx.try_borrow_mut() else {
            return false;
        };
        f(&mut ctx);

        if self.reentrant_theme_dirty.take() {
            let light = read_taskbar_is_light();
            if light != ctx.light_taskbar {
                ctx.light_taskbar = light;
                self.reentrant_force_render.set(true);
            }
        }
        if let Some(update) = self.reentrant_layout_update.take() {
            ctx.request_layout_scan(update, true);
        }
        let need_close = self.reentrant_close.take();
        let need_refresh = !need_close
            && !ctx.user_closing
            && (self.reentrant_refresh.take() || self.reentrant_force_render.get());
        drop(ctx);

        if need_close {
            if !hwnd.0.is_null() {
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        } else if need_refresh {
            post_refresh();
        }
        true
    }

    fn with_ctx_mut_or(
        &self,
        hwnd: HWND,
        on_reentry: impl FnOnce(),
        f: impl FnOnce(&mut WindowCtx),
    ) {
        if !self.with_ctx_mut(hwnd, f) {
            on_reentry();
        }
    }
}

fn show_context_menu(state: &WndProcState, hwnd: HWND) {
    if state.menu_open.get() {
        return;
    }
    let (shared, callback, last_env_geom) = {
        let Ok(ctx) = state.ctx.try_borrow() else {
            return;
        };
        if !ctx.accepts_input() || ctx.user_closing {
            return;
        }
        (ctx.shared.clone(), ctx.callback.clone(), ctx.last_env_geom)
    };
    state.menu_open.set(true);
    let _menu_guard = MenuOpenGuard(&state.menu_open);

    let config = {
        let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
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

    let cmd = unsafe {
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

        let _ = SetForegroundWindow(hwnd);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let tray_hwnd = FindWindowW(w!("Shell_TrayWnd"), None).unwrap_or_default();
        let tray_rect = (!tray_hwnd.0.is_null())
            .then(|| {
                let mut r = RECT::default();
                GetWindowRect(tray_hwnd, &mut r)
                    .ok()
                    .map(|()| PixelRect::from_win32(r))
            })
            .flatten()
            .or_else(|| last_env_geom.map(|(_, bar, _, _)| bar));
        let mon_rect = tray_rect.and_then(|_| {
            let target = if !tray_hwnd.0.is_null() {
                tray_hwnd
            } else {
                hwnd
            };
            let mon = MonitorFromWindow(target, MONITOR_DEFAULTTOPRIMARY);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi)
                .as_bool()
                .then(|| PixelRect::from_win32(mi.rcMonitor))
        });
        let (menu_pt, align_flags) = context_menu_anchor(pt, tray_rect, mon_rect);
        let cmd = TrackPopupMenuEx(
            hmenu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0 | align_flags,
            menu_pt.x,
            menu_pt.y,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(hmenu);
        cmd
    };

    let mut should_dispatch_action = false;
    if unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)).as_bool() } {
        state.with_ctx_mut(hwnd, |ctx| {
            if ctx.user_closing {
                return;
            }
            should_dispatch_action = true;
            if ctx.hovered && !cursor_inside_window(hwnd) {
                ctx.hovered = false;
                ctx.tracking_mouse = false;
                ctx.refresh(true);
            }
        });
    }
    if should_dispatch_action {
        match cmd.0 {
            1 => callback(Action::OpenMain),
            2 => callback(Action::OpenSettings),
            3 => callback(Action::Exit),
            _ => {}
        }
    }
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

    if msg == WM_DESTROY {
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            PostQuitMessage(0);
        }
        return LRESULT(0);
    }

    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }

    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WndProcState;
    if ptr.is_null() {
        return unsafe {
            windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
        };
    }
    let state = unsafe { &*ptr };

    if state.msg_taskbar_created != 0 && msg == state.msg_taskbar_created {
        state.with_ctx_mut_or(
            hwnd,
            || {
                state.reentrant_theme_dirty.set(true);
                state.queue_reentrant_layout(LayoutUpdate::Invalidate);
                state.reentrant_force_render.set(true);
                state.reentrant_refresh.set(true);
            },
            |ctx| {
                ctx.light_taskbar = read_taskbar_is_light();
                ctx.request_layout_scan(LayoutUpdate::Invalidate, true);
                ctx.refresh(true);
            },
        );
        return LRESULT(0);
    }

    if state.msg_shell_hook != 0 && msg == state.msg_shell_hook {
        let event = wparam.0 as u32;
        match event {
            HSHELL_WINDOWCREATED | HSHELL_WINDOWDESTROYED | HSHELL_WINDOWREPLACED => {
                if event == HSHELL_WINDOWDESTROYED {
                    FULLSCREEN_WAKE_PENDING.store(true, std::sync::atomic::Ordering::Release);
                }
                state.with_ctx_mut_or(
                    hwnd,
                    || {
                        state.queue_reentrant_layout(LayoutUpdate::Recheck);
                        state.reentrant_refresh.set(true);
                    },
                    |ctx| {
                        ctx.request_layout_scan(LayoutUpdate::Recheck, true);
                        ctx.refresh(false);
                    },
                );
            }
            HSHELL_WINDOWACTIVATED | HSHELL_RUDEAPPACTIVATED_MSG => {
                FULLSCREEN_WAKE_PENDING.store(true, std::sync::atomic::Ordering::Release);
                state.with_ctx_mut_or(
                    hwnd,
                    || {
                        state.reentrant_refresh.set(true);
                    },
                    |ctx| {
                        ctx.refresh(false);
                    },
                );
            }
            _ => {}
        }
        return LRESULT(0);
    }

    match msg {
        WM_APP_REFRESH => {
            REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
            let recheck = LAYOUT_RECHECK_PENDING.swap(false, std::sync::atomic::Ordering::AcqRel);
            let force = state.reentrant_force_render.take();
            state.with_ctx_mut_or(
                hwnd,
                || {
                    if recheck {
                        state.queue_reentrant_layout(LayoutUpdate::Recheck);
                    }
                    if force {
                        state.reentrant_force_render.set(true);
                    }
                    state.reentrant_refresh.set(true);
                },
                |ctx| {
                    if recheck {
                        ctx.request_layout_scan(LayoutUpdate::Recheck, true);
                    }
                    ctx.refresh(force);
                },
            );
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == UIA_SETTLE_TIMER_ID {
                unsafe {
                    let _ = KillTimer(Some(hwnd), UIA_SETTLE_TIMER_ID);
                }
                state.with_ctx_mut_or(
                    hwnd,
                    || {
                        state.queue_reentrant_layout(LayoutUpdate::Recheck);
                        state.reentrant_refresh.set(true);
                    },
                    |ctx| {
                        ctx.request_layout_scan(LayoutUpdate::Recheck, false);
                        ctx.refresh(false);
                    },
                );
            } else if wparam.0 == FULLSCREEN_EXIT_TIMER_ID {
                unsafe {
                    let _ = KillTimer(Some(hwnd), FULLSCREEN_EXIT_TIMER_ID);
                }
                state.with_ctx_mut_or(
                    hwnd,
                    || {
                        state.reentrant_refresh.set(true);
                    },
                    |ctx| {
                        ctx.fullscreen_recheck_armed = false;
                        ctx.fullscreen_recheck_remaining =
                            ctx.fullscreen_recheck_remaining.saturating_sub(1);
                        ctx.refresh(false);
                    },
                );
            } else if wparam.0 == TOPMOST_RECHECK_TIMER_ID {
                state.with_ctx_mut_or(
                    hwnd,
                    || {
                        state.reentrant_refresh.set(true);
                    },
                    |ctx| {
                        ctx.recheck_topmost_occlusion();
                    },
                );
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            state.with_ctx_mut_or(
                hwnd,
                || {
                    state.queue_reentrant_layout(LayoutUpdate::Recheck);
                    state.reentrant_theme_dirty.set(true);
                    state.reentrant_refresh.set(true);
                },
                |ctx| {
                    if let Some(renderer) = ctx.renderer.as_mut() {
                        renderer.invalidate_rendering_params();
                    }
                    ctx.request_layout_scan(LayoutUpdate::Recheck, true);
                    let light = read_taskbar_is_light();
                    if light != ctx.light_taskbar {
                        ctx.light_taskbar = light;
                        ctx.refresh(true);
                    } else {
                        ctx.refresh(false);
                    }
                },
            );
            LRESULT(0)
        }
        WM_DWMCOLORIZATIONCOLORCHANGED => {
            state.with_ctx_mut_or(
                hwnd,
                || {
                    state.reentrant_theme_dirty.set(true);
                    state.reentrant_refresh.set(true);
                },
                |ctx| {
                    let light = read_taskbar_is_light();
                    if light != ctx.light_taskbar {
                        ctx.light_taskbar = light;
                        ctx.refresh(true);
                    }
                },
            );
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            state.with_ctx_mut_or(
                hwnd,
                || {
                    state.queue_reentrant_layout(LayoutUpdate::Invalidate);
                    state.reentrant_force_render.set(true);
                    state.reentrant_refresh.set(true);
                },
                |ctx| {
                    ctx.request_layout_scan(LayoutUpdate::Invalidate, true);
                    ctx.refresh(true);
                },
            );
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            state.with_ctx_mut_or(
                hwnd,
                || {
                    state.reentrant_refresh.set(true);
                },
                |ctx| {
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
                },
            );
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            state.with_ctx_mut_or(
                hwnd,
                || {
                    state.reentrant_refresh.set(true);
                },
                |ctx| {
                    ctx.tracking_mouse = false;
                    if ctx.hovered {
                        ctx.hovered = false;
                        ctx.refresh(true);
                    }
                },
            );
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let callback = state
                .ctx
                .try_borrow()
                .ok()
                .filter(|ctx| ctx.accepts_input())
                .map(|ctx| ctx.callback.clone());
            if let Some(cb) = callback {
                cb(Action::OpenMain);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(state, hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            let Ok(mut ctx) = state.ctx.try_borrow_mut() else {
                state.reentrant_close.set(true);
                return LRESULT(0);
            };
            ctx.user_closing = true;
            ctx.fullscreen_recheck_armed = false;
            ctx.fullscreen_recheck_remaining = 0;
            ctx.topmost_recheck_armed = false;
            drop(ctx);
            unsafe {
                let _ = KillTimer(Some(hwnd), UIA_SETTLE_TIMER_ID);
                let _ = KillTimer(Some(hwnd), FULLSCREEN_EXIT_TIMER_ID);
                let _ = KillTimer(Some(hwnd), TOPMOST_RECHECK_TIMER_ID);
                let _ = DestroyWindow(hwnd);
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
    visibility: Arc<MonitorVisibility>,
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
    LAYOUT_RECHECK_PENDING.store(false, std::sync::atomic::Ordering::Release);
    FULLSCREEN_WAKE_PENDING.store(false, std::sync::atomic::Ordering::Release);
    let uia_scanner = Arc::new(UiaScanner::start());

    loop {
        let renderer = Renderer::new();
        let state_raw: *mut WndProcState = Box::into_raw(Box::new(WndProcState::new(
            msg_taskbar_created,
            msg_shell_hook,
            WindowCtx {
                hwnd: HWND::default(),
                in_elevated_band: false,
                user_closing: false,
                shared: shared.clone(),
                visibility: visibility.clone(),
                callback: callback.clone(),
                renderer,
                uia_scanner: uia_scanner.clone(),
                light_taskbar: read_taskbar_is_light(),
                hovered: false,
                tracking_mouse: false,
                last_display: None,
                last_size: (0, 0),
                last_pos: None,
                last_visible: false,
                last_present_error: None,
                last_anchor_slot: None,
                last_compact: false,
                last_stacked: false,
                last_env_geom: None,
                pending_uia_scan: true,
                invalidate_uia_layout: true,
                restore_from_cached_layout: false,
                tracked_fg_tid: 0,
                fg_loc_hook: HWINEVENTHOOK::default(),
                tracked_fs_tid: 0,
                fs_loc_hook: HWINEVENTHOOK::default(),
                fs_lifecycle_hook: HWINEVENTHOOK::default(),
                fs_cloak_hook: HWINEVENTHOOK::default(),
                tracked_tray_tid: 0,
                tray_hook: HWINEVENTHOOK::default(),
                topmost_hooks: Vec::new(),
                last_fg_hwnd: 0,
                fullscreen_recheck_remaining: 0,
                fullscreen_recheck_armed: false,
                hidden_by_occlusion: false,
                hidden_by_tray_missing: false,
                last_occluded_rect: None,
                applied_clip: None,
                clip_occluder: HWND::default(),
                topmost_recheck_armed: false,
            },
        )));
        let state = unsafe { &*state_raw };

        let (hwnd, in_elevated_band) =
            match create_elevated_monitor_window(hmodule, class_name, state_raw) {
                Ok((h, band)) if !h.0.is_null() => (h, band),
                _ => {
                    unsafe { drop(Box::from_raw(state_raw)) };
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

        {
            let mut ctx = state.ctx.borrow_mut();
            ctx.hwnd = hwnd;
            ctx.in_elevated_band = in_elevated_band;
        }
        REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
        IN_ELEVATED_BAND.store(in_elevated_band, std::sync::atomic::Ordering::Release);
        LIVE_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::Release);

        // If `shutdown()` / `set_config(enabled = false)` raced before `LIVE_HWND`
        // was published, exit immediately instead of parking in `GetMessageW`.
        let still_enabled = {
            let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.config.enabled
        };
        if !still_enabled {
            LIVE_HWND.store(0, std::sync::atomic::Ordering::Release);
            IN_ELEVATED_BAND.store(false, std::sync::atomic::Ordering::Release);
            REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
            unsafe {
                let _ = DestroyWindow(hwnd);
                drop(Box::from_raw(state_raw));
            }
            break;
        }

        let shell_hook_registered = unsafe { RegisterShellHookWindow(hwnd).as_bool() };
        for allowed_msg in [
            msg_taskbar_created,
            msg_shell_hook,
            WM_SETTINGCHANGE,
            WM_DWMCOLORIZATIONCOLORCHANGED,
        ] {
            if allowed_msg != 0 {
                unsafe {
                    let _ = ChangeWindowMessageFilterEx(hwnd, allowed_msg, MSGFLT_ALLOW, None);
                }
            }
        }
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
        let menu_hook = install_sys_hook(EVENT_SYSTEM_MENUPOPUPSTART, EVENT_SYSTEM_MENUPOPUPEND);
        let obj_vis_hook = install_sys_hook(EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE);
        let cloak_hook = install_sys_hook(EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED);

        state.with_ctx_mut(hwnd, |ctx| ctx.refresh(true));

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
        IN_ELEVATED_BAND.store(false, std::sync::atomic::Ordering::Release);
        REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
        for hook in [
            fg_hook,
            movesize_hook,
            minimize_hook,
            menu_hook,
            obj_vis_hook,
            cloak_hook,
        ] {
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
        let user_closing = {
            let mut ctx = state.ctx.borrow_mut();
            ctx.unhook_dynamic_hooks();
            ctx.user_closing
        };
        if unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)).as_bool() } {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(state_raw));
        }

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

    // Desktop Z-order (`HWND_TOPMOST` / `EnumWindows`) is session-global state;
    // serialize tests that mutate or assert relative top-level Z-order.
    static ZORDER_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn z_order_test_lock() -> std::sync::MutexGuard<'static, ()> {
        ZORDER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct TestWindows(Vec<HWND>);

    impl Drop for TestWindows {
        fn drop(&mut self) {
            for hwnd in self.0.iter().rev() {
                unsafe {
                    let _ = DestroyWindow(*hwnd);
                }
            }
        }
    }

    fn create_topmost_test_window(hinstance: HINSTANCE, class_name: PCWSTR) -> HWND {
        unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                class_name,
                w!("Mock Topmost Test Window"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance),
                None,
            )
            .unwrap()
        }
    }

    #[test]
    fn monitor_window_is_unowned_and_survives_tray_destruction() {
        let _zorder = z_order_test_lock();
        use windows::Win32::UI::WindowsAndMessaging::{GW_OWNER, IsWindow};
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let widget = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let mut windows = TestWindows(vec![tray, widget]);
        assert!(ensure_just_above_taskbar(widget, tray));
        assert!(
            unsafe { GetWindow(widget, GW_OWNER) }
                .unwrap_or_default()
                .0
                .is_null(),
            "the taskbar must never own our window or attach its input queue"
        );
        unsafe {
            DestroyWindow(tray).unwrap();
        }
        windows.0.remove(0);
        assert!(
            unsafe { IsWindow(Some(widget)).as_bool() },
            "an Explorer restart must not destroy the independent widget"
        );
        assert!(!ensure_just_above_taskbar(widget, tray));
        assert!(!unsafe { IsWindowVisible(widget).as_bool() });
    }

    #[test]
    fn z_order_check_handles_more_than_128_intervening_windows() {
        let _zorder = z_order_test_lock();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let mut windows = TestWindows(Vec::new());
        for _ in 0..162 {
            let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
            windows.0.push(hwnd);
            unsafe {
                SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
                .unwrap();
            }
        }
        let bottom = windows.0[0];
        let top = *windows.0.last().unwrap();
        assert!(is_window_above(top, bottom));
        assert!(!is_window_above(bottom, top));
        assert!(!is_window_above(top, top));
    }

    #[test]
    fn visibility_notifier_wakes_only_on_transitions() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = notifications.clone();
        let handle = TaskbarMonitorHandle::new_with_visibility_notifier(
            Arc::new(|_| {}),
            Arc::new(move || {
                observed.fetch_add(1, Ordering::Relaxed);
            }),
        );
        handle.visibility.set(false);
        assert_eq!(notifications.load(Ordering::Relaxed), 0);
        handle.visibility.set(true);
        handle.visibility.set(true);
        assert!(handle.is_visible());
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        handle.shutdown();
        assert!(!handle.is_visible());
        assert_eq!(notifications.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn uia_cache_rejects_stale_scans_and_clears_failed_or_empty_results() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let bar = rect(0, 1032, 1920, 1080);
        let occupied = vec![rect(500, 1032, 1000, 1080)];
        scanner.request_scan(1, bar, true);
        let old_target = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(
            scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(old_target, Some(occupied.clone()))
        );
        assert_eq!(scanner.snapshot_for(1, bar).unwrap().rects, occupied);

        scanner.request_scan(1, bar, true);
        assert!(scanner.snapshot_for(1, bar).is_none());
        assert!(
            !scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(old_target, Some(occupied.clone()))
        );
        assert!(scanner.snapshot_for(1, bar).is_none());
        let target = scanner.state.0.lock().unwrap().pending.take().unwrap();
        for unavailable in [None, Some(Vec::new())] {
            scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(target, Some(occupied.clone()));
            assert!(scanner.state.0.lock().unwrap().publish(target, unavailable));
            assert!(
                scanner.snapshot_for(1, bar).is_none(),
                "failed layout must not reuse stale buttons"
            );
        }
        assert!(scanner.snapshot_for(2, bar).is_none());
        assert!(scanner.snapshot_for(1, rect(0, 0, 1920, 48)).is_none());
    }

    #[test]
    fn uia_revalidation_preserves_pixels_but_requires_latest_scan_for_input() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let bar = rect(0, 1032, 1920, 1080);
        let occupied = vec![rect(500, 1032, 1000, 1080)];
        scanner.request_scan(1, bar, true);
        let initial = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(
            scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(initial, Some(occupied.clone()))
        );
        assert!(scanner.snapshot_for(1, bar).unwrap().current);

        scanner.request_scan(1, bar, false);
        let superseded = scanner.state.0.lock().unwrap().pending.take().unwrap();
        scanner.request_scan(1, bar, false);
        let provisional = scanner.snapshot_for(1, bar).unwrap();
        assert_eq!(provisional.rects, occupied);
        assert!(!provisional.current);
        assert!(
            !scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(superseded, Some(occupied.clone()))
        );
        assert!(!scanner.snapshot_for(1, bar).unwrap().current);

        let latest = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(
            scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(latest, Some(occupied.clone())),
            "unchanged occupancy must still wake the window to restore input"
        );
        let validated = scanner.snapshot_for(1, bar).unwrap();
        assert!(validated.current);
        assert_eq!(validated.rects, occupied);
    }

    #[test]
    fn taskbar_focus_refresh_keeps_confirmed_layout_and_window_visible() {
        let _zorder = z_order_test_lock();
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _com = ComMtaGuard::enter();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd]);
        let bar = rect(-10_000, -10_000, -8_080, -9_952);
        let notify = rect(-8_300, -10_000, -8_080, -9_952);
        let env = TaskbarEnv {
            tray_hwnd: tray,
            taskbar_rect: bar,
            tray_notify_rect: Some(notify),
            dpi: 96,
        };
        unsafe {
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                bar.left,
                bar.top,
                bar.width(),
                bar.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
            .unwrap();
        }
        let config = Config {
            enabled: true,
            ..Config::default()
        };
        let metrics = Metrics::default();
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = notifications.clone();
        let scanner = Arc::new(UiaScanner {
            state: Arc::new((
                Mutex::new(UiaShared {
                    cache: UiaCache {
                        tray_hwnd: tray.0 as isize,
                        taskbar_rect: bar,
                        rects: Some(vec![notify]),
                        generation: 0,
                    },
                    ..Default::default()
                }),
                Condvar::new(),
            )),
            worker: Mutex::new(None),
        });
        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: false,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: config.clone(),
                metrics,
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(move || {
                    observed.fetch_add(1, Ordering::Relaxed);
                }),
            }),
            callback: Arc::new(|_| {}),
            renderer: Renderer::new(),
            uia_scanner: scanner,
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: Some((tray.0 as isize, bar, Some(notify), 96)),
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(
            ctx.last_visible,
            "initial display rendered={}, positioned={}, known_layout={}",
            ctx.last_display.is_some(),
            ctx.last_pos.is_some(),
            ctx.uia_scanner.snapshot_for(tray.0 as isize, bar).is_some()
        );
        assert_eq!(notifications.load(Ordering::Relaxed), 1);

        // Reproduce focusing the tray: its band/geometry and occupied buttons stay
        // unchanged, but a layout notification arrives before the next UIA scan.
        unsafe {
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
            .unwrap();
        }
        // Focus-time repair must not wait on the app state or UIA worker, nor
        // require processing the WM_APP_REFRESH posted after the hook returns.
        {
            let _app_state = ctx.shared.lock().unwrap();
            let _layout_state = ctx.uia_scanner.state.0.lock().unwrap();
            repair_visible_taskbar_order(hwnd, tray);
            assert!(is_window_above(hwnd, tray));
            assert!(unsafe { IsWindowVisible(hwnd).as_bool() });
        }
        for event in [
            EVENT_OBJECT_REORDER,
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_SHOW,
        ] {
            let update = taskbar_event_layout_update(event, OBJID_WINDOW.0, 0).unwrap();
            ctx.request_layout_scan(update, true);
            ctx.refresh_taskbar_env(&config, &metrics, env, false);
            assert!(
                ctx.last_visible,
                "focus-only taskbar notification must not hide the widget"
            );
            assert_eq!(
                notifications.load(Ordering::Relaxed),
                1,
                "focus must not publish hide/show transitions"
            );
            assert!(ctx.uia_scanner.snapshot_for(tray.0 as isize, bar).is_some());
            assert!(is_window_above(hwnd, tray));
        }

        // Captured opening/restoring TaskMan: shell creation plus ReBar client
        // reorder and native task-list location notifications, far from us.
        // Preserve both pixels and footprint until the entire burst is scanned.
        let previous_pos = ctx.last_pos;
        let previous_size = ctx.last_size;
        let previous_display = ctx.last_display.clone();
        use windows::Win32::UI::WindowsAndMessaging::OBJID_CLIENT;
        for (event, object, child) in [
            (EVENT_OBJECT_REORDER, OBJID_CLIENT.0, 0),
            (EVENT_OBJECT_LOCATIONCHANGE, OBJID_WINDOW.0, 0),
            (EVENT_OBJECT_SHOW, OBJID_WINDOW.0, 0),
            (EVENT_OBJECT_HIDE, OBJID_WINDOW.0, 0),
        ] {
            let update = taskbar_event_layout_update(event, object, child).unwrap();
            ctx.request_layout_scan(update, true);
            ctx.refresh_taskbar_env(&config, &metrics, env, true);
            assert!(ctx.last_visible);
            assert!(
                !ctx.accepts_input(),
                "pending occupancy must not steal clicks"
            );
            assert_eq!(ctx.last_pos, previous_pos);
            assert_eq!(ctx.last_size, previous_size);
            assert_eq!(ctx.last_display, previous_display);
            assert_eq!(notifications.load(Ordering::Relaxed), 1);
        }
        {
            let mut state = ctx.uia_scanner.state.0.lock().unwrap();
            let target = state.pending.take().unwrap();
            assert!(state.publish(target, Some(vec![notify])));
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);
        assert!(ctx.accepts_input());
        assert_eq!(notifications.load(Ordering::Relaxed), 1);

        // A completed scan that finds no free space still hides immediately.
        ctx.request_layout_scan(LayoutUpdate::Recheck, true);
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        {
            let mut state = ctx.uia_scanner.state.0.lock().unwrap();
            let target = state.pending.take().unwrap();
            let middle = bar.left + bar.width() / 2;
            assert!(state.publish(
                target,
                Some(vec![
                    rect(bar.left, bar.top, middle, bar.bottom),
                    rect(middle, bar.top, bar.right, bar.bottom),
                ])
            ));
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(!ctx.last_visible);
        assert_eq!(notifications.load(Ordering::Relaxed), 2);

        // A hidden widget must not reappear on provisional cached data.
        ctx.request_layout_scan(LayoutUpdate::Recheck, true);
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(!ctx.last_visible);
        {
            let mut state = ctx.uia_scanner.state.0.lock().unwrap();
            let target = state.pending.take().unwrap();
            assert!(state.publish(target, Some(vec![notify])));
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);
        assert!(ctx.accepts_input());
        assert_eq!(notifications.load(Ordering::Relaxed), 3);

        // A failed validation clears cached pixels; rechecks cannot revive them.
        ctx.request_layout_scan(LayoutUpdate::Recheck, true);
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        {
            let mut state = ctx.uia_scanner.state.0.lock().unwrap();
            let target = state.pending.take().unwrap();
            assert!(state.publish(target, None));
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(!ctx.last_visible);
        assert!(!ctx.accepts_input());
        assert_eq!(notifications.load(Ordering::Relaxed), 4);

        {
            let mut state = ctx.uia_scanner.state.0.lock().unwrap();
            let target = state.pending.take().unwrap();
            assert!(state.publish(target, Some(vec![notify])));
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);
        assert_eq!(notifications.load(Ordering::Relaxed), 5);

        // A failed hook-time positioning attempt may already have hidden the
        // native popup. A successful refresh must be able to show it again.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        assert!(ctx.last_visible);
        ctx.set_visible(true);
        assert!(unsafe { IsWindowVisible(hwnd).as_bool() });
        assert_eq!(notifications.load(Ordering::Relaxed), 5);

        // A root-window notification also invalidates if the queried geometry moved.
        let mut moved = env;
        moved.taskbar_rect.left -= 1;
        moved.taskbar_rect.right -= 1;
        ctx.request_layout_scan(LayoutUpdate::Recheck, true);
        ctx.refresh_taskbar_env(&config, &metrics, moved, false);
        assert!(!ctx.last_visible);
        assert_eq!(notifications.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn taskbar_layout_hints_recheck_without_discarding_pixels() {
        use windows::Win32::UI::WindowsAndMessaging::OBJID_CLIENT;
        for event in [
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_HIDE,
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_REORDER,
        ] {
            assert_eq!(
                taskbar_event_layout_update(event, OBJID_WINDOW.0, 0),
                Some(LayoutUpdate::Recheck)
            );
        }
        // Reordering accessible children can change occupancy without changing
        // Shell_TrayWnd's own rectangle (e.g. XAML taskbar button additions).
        assert_eq!(
            taskbar_event_layout_update(EVENT_OBJECT_REORDER, OBJID_CLIENT.0, 0),
            Some(LayoutUpdate::Recheck)
        );
        assert_eq!(
            taskbar_event_layout_update(EVENT_OBJECT_REORDER, OBJID_WINDOW.0, 1),
            Some(LayoutUpdate::Recheck)
        );
        assert_eq!(
            taskbar_event_layout_update(EVENT_OBJECT_LOCATIONCHANGE, OBJID_CLIENT.0, 0),
            None
        );
        assert_eq!(
            taskbar_event_layout_update(EVENT_SYSTEM_FOREGROUND, OBJID_WINDOW.0, 0),
            None
        );
    }

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
        let pos = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&[notify]),
            (160, 36),
            96,
            None,
            false,
        )
        .unwrap()
        .pos;
        assert!(monitor.covers(taskbar));
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
        let _zorder = z_order_test_lock();
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
            None,
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

        // Both initial placement and steady-state ticks must keep the unowned widget
        // above mock_tray while keeping mock_menu on top.
        ensure_just_above_taskbar(widget, mock_tray);
        assert!(is_window_above(widget, mock_tray));
        assert!(is_window_above(mock_menu, widget));

        ensure_just_above_taskbar(widget, mock_tray);
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
        let _zorder = z_order_test_lock();
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
        // Match production: no ownership or input-queue attachment to Explorer.
        let widget = make_win(WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, None);

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

        ensure_just_above_taskbar(widget, mock_tray);
        assert!(is_window_above(widget, mock_tray));
        assert!(is_window_above(mock_menu, widget));

        // Simulate Explorer demoting Shell_TrayWnd, then process its shell event.
        // Only our window may be repositioned; Explorer remains in the normal band.
        unsafe {
            let _ = SetWindowPos(
                mock_tray,
                Some(mock_fullscreen),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        ensure_just_above_taskbar(widget, mock_tray);
        let widget_ex = unsafe { GetWindowLongPtrW(widget, GWL_EXSTYLE) } as u32;
        assert_eq!(
            widget_ex & WS_EX_TOPMOST.0,
            0,
            "widget must not stay TOPMOST when taskbar itself was demoted"
        );
        assert_eq!(
            unsafe { GetWindowLongPtrW(mock_tray, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0,
            0
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
        ensure_just_above_taskbar(widget, mock_tray);
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
        let _zorder = z_order_test_lock();
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
    fn placement_adapts_compact_then_hides_when_packed_or_uia_is_unavailable() {
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

        // Unknown occupancy must not cover buttons, including before the first UIA scan.
        for empty_uia in [None, Some(&[][..])] {
            assert!(
                solve_taskbar_placement(
                    taskbar,
                    Some(notify),
                    empty_uia,
                    (150, 36),
                    96,
                    None,
                    false,
                )
                .is_none(),
                "unknown taskbar occupancy must keep the widget hidden"
            );
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

    #[test]
    fn vertical_taskbar_stacks_sections_to_fit_cross_axis_and_rejects_too_narrow() {
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

        let vert_bounds = LayoutBounds {
            horizontal: false,
            max_dip_w: 80.0,
            max_dip_h: 1080.0,
        };
        let layout = renderer
            .measure_layout(&display, vert_bounds)
            .expect("vertical layout");
        assert!(
            layout.stacked,
            "vertical 80px taskbar must stack Net above CPU/RAM"
        );
        assert!(
            layout.px_w <= 80,
            "stacked vertical width ({}) must fit inside 80px taskbar",
            layout.px_w
        );
        let (vw, vh, _) = renderer
            .render_layout(&display, layout, None)
            .expect("vertical render");
        let pixels = renderer.surface.as_ref().unwrap().pixels();
        let top_lit = (0..(vh / 2) as usize)
            .any(|y| (0..vw as usize).any(|x| pixels[(y * vw as usize + x) * 4 + 3] >= 64));
        let bottom_lit = ((vh / 2) as usize..vh as usize)
            .any(|y| (0..vw as usize).any(|x| pixels[(y * vw as usize + x) * 4 + 3] >= 64));
        assert!(
            top_lit && bottom_lit,
            "stacked layout must render both top and bottom sections"
        );

        // Placement must reject a widget that still exceeds cross-axis taskbar width.
        let narrow_bar = rect(0, 0, 50, 1080);
        let narrow_notify = rect(0, 900, 50, 1080);
        let narrow_uia = [rect(0, 0, 50, 300)];
        assert_eq!(
            solve_taskbar_placement(
                narrow_bar,
                Some(narrow_notify),
                Some(&narrow_uia),
                (vw, vh),
                96,
                None,
                false,
            ),
            None,
            "oversized widget must hide instead of overflowing left-pinned taskbar"
        );
        unsafe { CoUninitialize() };
    }

    #[test]
    fn small_horizontal_taskbar_clamps_safely_without_panicking() {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut renderer = Renderer::new().expect("Direct2D/DirectWrite renderer");
        let cfg = Config {
            enabled: true,
            show_cpu: true,
            show_ram: true,
            show_network: true,
            font_size: 12.0,
            ..Config::default()
        };
        let metrics = Metrics {
            cpu_pct: Some(15.0),
            ram_pct: Some(50.0),
            net_sent_bps: Some(1_024.0),
            net_recv_bps: Some(2_048.0),
        };
        let display = build_display(&cfg, &metrics, false, false, 96, false);

        // Previously panicked when max_dip_h in (16.0, 26.0) made max_h < 22.0 in clamp(22.0, max_h).
        let tiny_bounds = LayoutBounds::horizontal(20.0);
        let (tw, th, _) = renderer
            .measure_and_render_in(&display, tiny_bounds, None)
            .expect("tiny horizontal taskbar must not panic");
        assert!(tw > 0 && th <= 20, "tiny bar height {th} must stay <= 20px");

        // Windows 10 small taskbar (~30 DIPs): larger configured font steps down to fit cleanly.
        let small_bounds = LayoutBounds::horizontal(30.0);
        let small_layout = renderer
            .measure_layout(&display, small_bounds)
            .expect("small taskbar layout");
        assert!(
            small_layout.px_h <= 30 && small_layout.content_h <= small_layout.dip_h,
            "small taskbar layout {}x{} (content_h={}) must fit 30px height",
            small_layout.px_w,
            small_layout.px_h,
            small_layout.content_h
        );
        unsafe { CoUninitialize() };
    }

    #[test]
    fn light_taskbar_contrast_respects_color_prevalence_and_accent_luminance() {
        // Without ColorPrevalence, SystemUsesLightTheme drives light/dark contrast.
        assert!(resolve_light_taskbar(true, false, Some([0, 120, 212])));
        assert!(!resolve_light_taskbar(false, false, Some([255, 210, 0])));

        // With ColorPrevalence enabled, accent color luminance overrides SystemUsesLightTheme.
        assert!(
            !resolve_light_taskbar(true, true, Some([0, 120, 212])),
            "dark blue accent taskbar needs white text even when SystemUsesLightTheme is true"
        );
        assert!(
            resolve_light_taskbar(false, true, Some([255, 185, 0])),
            "bright yellow accent taskbar needs dark text even when SystemUsesLightTheme is false"
        );

        // Missing accent value falls back safely to SystemUsesLightTheme.
        assert!(resolve_light_taskbar(true, true, None));
        assert!(!resolve_light_taskbar(false, true, None));
    }

    #[test]
    fn elevated_band_window_stays_above_activated_tray_without_z_order_repair() {
        let _zorder = z_order_test_lock();
        let _com = ComMtaGuard::enter();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_topmost_test_window(hinstance, class_name);
        let (hwnd, in_elevated_band) =
            create_elevated_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd]);

        assert!(
            unsafe { GetWindow(hwnd, windows::Win32::UI::WindowsAndMessaging::GW_OWNER) }
                .unwrap_or_default()
                .0
                .is_null(),
            "elevated-band widget must remain unowned"
        );
        if in_elevated_band {
            assert_eq!(window_band(hwnd), Some(TASKBAR_MONITOR_BAND));
        } else {
            assert_ne!(window_band(hwnd), Some(TASKBAR_MONITOR_BAND));
        }

        let bar = rect(-20_000, -20_000, -18_080, -19_952);
        unsafe {
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                bar.left,
                bar.top,
                bar.width(),
                bar.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                bar.left + 100,
                bar.top + 4,
                160,
                40,
                SWP_NOACTIVATE,
            )
            .unwrap();
        }
        assert!(is_window_above(hwnd, tray));

        // Simulate Band-1 taskbar activation moving `tray` to the top of Band 1.
        // Without calling `repair_visible_taskbar_order`, a Band-16 widget must
        // remain above `tray` natively; on hosts that fall back to Band 1,
        // `ensure_just_above_taskbar` must recover the order cleanly.
        if in_elevated_band {
            for _ in 0..3 {
                unsafe {
                    SetWindowPos(
                        tray,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    )
                    .unwrap();
                }
                assert!(
                    is_window_above(hwnd, tray),
                    "Band-{TASKBAR_MONITOR_BAND} widget must never fall below activated Band-1 tray"
                );
                assert!(unsafe { IsWindowVisible(hwnd).as_bool() });
            }
        } else {
            unsafe {
                SetWindowPos(
                    tray,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
                .unwrap();
            }
            assert!(ensure_just_above_taskbar(hwnd, tray));
            assert!(is_window_above(hwnd, tray));
        }
    }

    struct DpiContextGuard(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);
    impl DpiContextGuard {
        fn per_monitor_v2() -> Self {
            Self(unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            })
        }
    }
    impl Drop for DpiContextGuard {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = SetThreadDpiAwarenessContext(self.0);
                }
            }
        }
    }

    #[test]
    fn elevated_band_hides_on_overlapping_topmost_popup_and_restores() {
        let _zorder = z_order_test_lock();
        let _dpi = DpiContextGuard::per_monitor_v2();
        let _com = ComMtaGuard::enter();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_topmost_test_window(hinstance, class_name);
        let (hwnd, _) =
            create_elevated_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let popup = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd, popup]);

        let bar = rect(-30_000, -30_000, -28_080, -29_952);
        let notify = rect(-28_300, -30_000, -28_080, -29_952);
        let env = TaskbarEnv {
            tray_hwnd: tray,
            taskbar_rect: bar,
            tray_notify_rect: Some(notify),
            dpi: 96,
        };
        unsafe {
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                bar.left,
                bar.top,
                bar.width(),
                bar.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
        }

        let config = Config {
            enabled: true,
            ..Config::default()
        };
        let metrics = Metrics::default();
        let scanner = Arc::new(UiaScanner {
            state: Arc::new((
                Mutex::new(UiaShared {
                    cache: UiaCache {
                        tray_hwnd: tray.0 as isize,
                        taskbar_rect: bar,
                        rects: Some(vec![notify]),
                        generation: 0,
                    },
                    ..Default::default()
                }),
                Condvar::new(),
            )),
            worker: Mutex::new(None),
        });
        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: true,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: config.clone(),
                metrics,
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: Renderer::new(),
            uia_scanner: scanner,
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: Some((tray.0 as isize, bar, Some(notify), 96)),
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);
        let widget_rect = ctx.last_rect().expect("visible widget rect");

        // Non-overlapping topmost popup leaves the widget visible.
        unsafe {
            let _ = ShowWindow(popup, SW_SHOWNOACTIVATE);
            SetWindowPos(
                popup,
                Some(HWND_TOPMOST),
                bar.left,
                bar.bottom + 200,
                200,
                100,
                SWP_NOACTIVATE,
            )
            .unwrap();
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);

        // Overlapping topmost popup (menu, flyout, or always-on-top window) hides the widget.
        unsafe {
            SetWindowPos(
                popup,
                Some(HWND_TOPMOST),
                widget_rect.left,
                widget_rect.top,
                widget_rect.width().max(40),
                widget_rect.height().max(24),
                SWP_NOACTIVATE,
            )
            .unwrap();
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(
            !ctx.last_visible && unsafe { !IsWindowVisible(hwnd).as_bool() },
            "Band-16 widget must hide when a topmost popup overlaps its footprint"
        );

        // Click-through transparent overlay does not occlude the widget.
        let orig_ex = unsafe { GetWindowLongPtrW(popup, GWL_EXSTYLE) };
        unsafe {
            SetWindowLongPtrW(popup, GWL_EXSTYLE, orig_ex | WS_EX_TRANSPARENT.0 as isize);
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);

        // Non-topmost overlapping window behind the tray also does not hide the widget.
        unsafe {
            SetWindowLongPtrW(popup, GWL_EXSTYLE, orig_ex);
            SetWindowPos(
                popup,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
            .unwrap();
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(
            ctx.last_visible,
            "non-topmost window behind tray must not occlude Band-16 widget \
             (popup_above_tray={}, tray_ex={:#x}, popup_ex={:#x}, occluded={})",
            is_window_above(popup, tray),
            unsafe { GetWindowLongPtrW(tray, GWL_EXSTYLE) },
            unsafe { GetWindowLongPtrW(popup, GWL_EXSTYLE) },
            is_rect_occluded_above_taskbar(tray, hwnd, widget_rect),
        );

        // Restoring the overlapping topmost popup hides again, and hiding it restores visibility.
        unsafe {
            SetWindowPos(
                popup,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
            .unwrap();
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(!ctx.last_visible);

        unsafe {
            let _ = ShowWindow(popup, SW_HIDE);
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);
    }

    #[test]
    fn context_menu_anchor_places_menu_outside_taskbar_edges() {
        let monitor = Some(rect(0, 0, 1920, 1080));

        let (pt, align) = context_menu_anchor(
            POINT { x: 1600, y: 1050 },
            Some(rect(0, 1032, 1920, 1080)),
            monitor,
        );
        assert_eq!((pt.x, pt.y, align), (1600, 1032, TPM_BOTTOMALIGN.0));

        let (pt, align) = context_menu_anchor(
            POINT { x: 1600, y: 20 },
            Some(rect(0, 0, 1920, 48)),
            monitor,
        );
        assert_eq!((pt.x, pt.y, align), (1600, 48, TPM_TOPALIGN.0));

        let (pt, align) =
            context_menu_anchor(POINT { x: 30, y: 900 }, Some(rect(0, 0, 64, 1080)), monitor);
        assert_eq!((pt.x, pt.y, align), (64, 900, TPM_LEFTALIGN.0));

        let (pt, align) = context_menu_anchor(
            POINT { x: 1880, y: 900 },
            Some(rect(1856, 0, 1920, 1080)),
            monitor,
        );
        assert_eq!((pt.x, pt.y, align), (1856, 900, TPM_RIGHTALIGN.0));
    }

    #[test]
    fn band1_fallback_restores_order_when_elevated_band_disabled() {
        let _zorder = z_order_test_lock();
        let _com = ComMtaGuard::enter();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_topmost_test_window(hinstance, class_name);
        let hwnd = create_topmost_test_window(hinstance, class_name);
        let _windows = TestWindows(vec![tray, hwnd]);

        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                -25_000,
                -25_000,
                160,
                40,
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                -25_100,
                -25_000,
                1920,
                48,
                SWP_NOACTIVATE,
            )
            .unwrap();
        }
        assert!(
            is_window_above(tray, hwnd),
            "tray must start above hwnd (tray_ex={:#x}, hwnd_ex={:#x}, hwnd_above_tray={}, tray_visible={}, hwnd_visible={})",
            unsafe { GetWindowLongPtrW(tray, GWL_EXSTYLE) },
            unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) },
            is_window_above(hwnd, tray),
            unsafe { IsWindowVisible(tray).as_bool() },
            unsafe { IsWindowVisible(hwnd).as_bool() },
        );

        let ctx = WindowCtx {
            hwnd,
            in_elevated_band: false,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: Config::default(),
                metrics: Metrics::default(),
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: None,
            uia_scanner: Arc::new(UiaScanner {
                state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
                worker: Mutex::new(None),
            }),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: None,
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };
        assert!(ctx.ensure_visible_order(tray, None));
        assert!(is_window_above(hwnd, tray));
    }

    #[test]
    fn sync_topmost_hooks_tracks_background_topmost_thread_moves() {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;
        use windows::Win32::UI::WindowsAndMessaging::{
            MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
        };

        struct LiveGlobalsGuard {
            prev_elevated: bool,
            prev_hwnd: isize,
            prev_tray: isize,
            prev_pending: bool,
        }
        impl Drop for LiveGlobalsGuard {
            fn drop(&mut self) {
                IN_ELEVATED_BAND.store(self.prev_elevated, Ordering::Release);
                LIVE_HWND.store(self.prev_hwnd, Ordering::Release);
                LIVE_TRAY_HWND.store(self.prev_tray, Ordering::Release);
                REFRESH_PENDING.store(self.prev_pending, Ordering::Release);
            }
        }

        let _zorder = z_order_test_lock();
        let _com = ComMtaGuard::enter();
        let _globals = LiveGlobalsGuard {
            prev_elevated: IN_ELEVATED_BAND.load(Ordering::Acquire),
            prev_hwnd: LIVE_HWND.load(Ordering::Acquire),
            prev_tray: LIVE_TRAY_HWND.load(Ordering::Acquire),
            prev_pending: REFRESH_PENDING.load(Ordering::Acquire),
        };

        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let make_topmost_win = |x: i32, y: i32, w: i32, h: i32| unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                class_name,
                w!("sync_topmost_test"),
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(hinstance),
                None,
            )
            .unwrap()
        };
        let bar = rect(-26_000, -26_000, -24_080, -25_952);
        let tray = make_topmost_win(bar.left, bar.top, bar.width(), bar.height());
        let (hwnd, _) =
            create_elevated_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd]);

        unsafe {
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                bar.left,
                bar.top,
                bar.width(),
                bar.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
        }

        #[derive(Clone, Copy)]
        enum BgCmd {
            Move,
            Hide,
            Exit,
        }
        let (cmd_tx, cmd_rx) = mpsc::channel::<BgCmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<(isize, u32)>();
        let (ack_tx, ack_rx) = mpsc::channel::<()>();
        let hinst_raw = hinstance.0 as usize;
        let cls_raw = class_name.0 as usize;
        let bg_join = std::thread::spawn(move || {
            let hinst = HINSTANCE(hinst_raw as *mut std::ffi::c_void);
            let cls = PCWSTR(cls_raw as *const u16);
            let bg = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                    cls,
                    w!("sync_topmost_bg"),
                    WS_POPUP,
                    -26_000,
                    -25_500,
                    120,
                    40,
                    None,
                    None,
                    Some(hinst),
                    None,
                )
                .unwrap()
            };
            unsafe {
                let _ = ShowWindow(bg, SW_SHOWNOACTIVATE);
                SetWindowPos(
                    bg,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
                .unwrap();
            }
            let _ = ready_tx.send((bg.0 as isize, unsafe { GetCurrentThreadId() }));
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    BgCmd::Move => {
                        unsafe {
                            SetWindowPos(
                                bg,
                                None,
                                -25_900,
                                -25_500,
                                120,
                                40,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            )
                            .unwrap();
                        }
                        let _ = ack_tx.send(());
                    }
                    BgCmd::Hide => {
                        unsafe {
                            let _ = ShowWindow(bg, SW_HIDE);
                        }
                        let _ = ack_tx.send(());
                    }
                    BgCmd::Exit => break,
                }
            }
            unsafe {
                let _ = DestroyWindow(bg);
            }
        });

        let (bg_raw, bg_tid) = ready_rx.recv().expect("background topmost thread ready");
        let bg_hwnd = HWND(bg_raw as *mut std::ffi::c_void);
        IN_ELEVATED_BAND.store(true, Ordering::Release);
        LIVE_HWND.store(hwnd.0 as isize, Ordering::Release);
        LIVE_TRAY_HWND.store(tray.0 as isize, Ordering::Release);

        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: true,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: Config::default(),
                metrics: Metrics::default(),
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: None,
            uia_scanner: Arc::new(UiaScanner {
                state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
                worker: Mutex::new(None),
            }),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: None,
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        ctx.sync_topmost_hooks(tray);
        assert!(
            ctx.topmost_hooks.iter().any(|&(tid, _)| tid == bg_tid),
            "background topmost thread {bg_tid} must receive a location hook (hooks={:?}, bg_above_tray={}, tray_above_bg={}, potential={}, topmost_or_elev={}, bg_ex={:#x}, tray_ex={:#x}, bg_band={:?}, tray_band={:?})",
            ctx.topmost_hooks
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>(),
            is_window_above(bg_hwnd, tray),
            is_window_above(tray, bg_hwnd),
            is_potential_occluder_window(bg_hwnd, hwnd, unsafe { GetDesktopWindow() }, unsafe {
                GetShellWindow()
            }),
            is_topmost_or_elevated_window(bg_hwnd),
            unsafe { GetWindowLongPtrW(bg_hwnd, GWL_EXSTYLE) },
            unsafe { GetWindowLongPtrW(tray, GWL_EXSTYLE) },
            window_band(bg_hwnd),
            window_band(tray),
        );

        REFRESH_PENDING.store(false, Ordering::Release);
        cmd_tx.send(BgCmd::Move).unwrap();
        ack_rx.recv().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut msg = MSG::default();
        while !REFRESH_PENDING.load(Ordering::Acquire) && Instant::now() < deadline {
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                let _ = unsafe { TranslateMessage(&msg) };
                unsafe { DispatchMessageW(&msg) };
            }
            if REFRESH_PENDING.load(Ordering::Acquire) {
                break;
            }
            let remaining_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(50) as u32;
            unsafe {
                let _ = MsgWaitForMultipleObjects(None, false, remaining_ms, QS_ALLINPUT);
            }
        }
        assert!(
            REFRESH_PENDING.load(Ordering::Acquire),
            "moving an already-visible background topmost window must wake overlap refresh \
             (candidate={}, rect={:?}, bg_ex={:#x})",
            is_topmost_location_candidate(bg_hwnd, OBJID_WINDOW.0, 0),
            visible_frame_rect(bg_hwnd),
            unsafe { GetWindowLongPtrW(bg_hwnd, GWL_EXSTYLE) }
        );

        cmd_tx.send(BgCmd::Hide).unwrap();
        ack_rx.recv().unwrap();
        ctx.sync_topmost_hooks(tray);
        assert!(
            !ctx.topmost_hooks.iter().any(|&(tid, _)| tid == bg_tid),
            "hiding background topmost window must remove its thread hook"
        );

        let _ = cmd_tx.send(BgCmd::Exit);
        let _ = bg_join.join();
    }

    #[test]
    fn shell_host_overlay_identity_matches_only_trusted_shell_core_windows() {
        let root = r"C:\Windows";
        for path in [
            r"C:\WINDOWS\SystemApps\Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy\StartMenuExperienceHost.exe",
            r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\SearchHost.exe",
            r"C:\Windows\SystemApps\Microsoft.Windows.Search_cw5n1h2txyewy\SearchApp.exe",
            r"C:\Windows\SystemApps\ShellExperienceHost_cw5n1h2txyewy\ShellExperienceHost.exe",
            r"C:\Windows\System32\ShellHost.exe",
            r"C:\Windows\explorer.exe",
        ] {
            for overlay_class in [
                "Windows.UI.Core.CoreWindow",
                "XamlExplorerHostIslandWindow",
                "MultitaskingViewFrame",
            ] {
                assert!(
                    is_shell_host_overlay_identity(overlay_class, path, root),
                    "expected {overlay_class} overlay exemption for {path}"
                );
            }
            for non_exempt_class in [
                "CabinetWClass",
                "TopLevelWindowForOverflowXamlIsland",
                "Xaml_WindowedPopupClass",
                "#32768",
                "tooltips_class32",
            ] {
                assert!(
                    !is_shell_host_overlay_identity(non_exempt_class, path, root),
                    "{non_exempt_class} inside {path} must still participate in occlusion/fullscreen checks"
                );
            }
        }

        for untrusted in [
            r"C:\Users\Julian\StartMenuExperienceHost.exe",
            r"C:\Temp\explorer.exe",
            r"D:\Windows\explorer.exe",
            r"D:\Windows\SystemApps\Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy\StartMenuExperienceHost.exe",
            r"C:\Windows\System32\notepad.exe",
            r"C:\Windows\SystemApps\Evil_pkg\App.exe",
            r"C:\Windows\SystemApps\Evil_pkg\explorer.exe",
            r"C:\Program Files\WindowsApps\App\Game.exe",
        ] {
            for overlay_class in [
                "Windows.UI.Core.CoreWindow",
                "XamlExplorerHostIslandWindow",
                "MultitaskingViewFrame",
            ] {
                assert!(
                    !is_shell_host_overlay_identity(overlay_class, untrusted, root),
                    "untrusted or non-shell image {untrusted} ({overlay_class}) must not be exempted"
                );
            }
        }
    }

    #[test]
    fn band1_ensure_just_above_taskbar_ignores_cross_band_topmost_windows() {
        let _zorder = z_order_test_lock();
        let _dpi = DpiContextGuard::per_monitor_v2();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_topmost_test_window(hinstance, class_name);
        let widget = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let (higher_band_win, elevated) =
            create_elevated_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![tray, widget, higher_band_win]);
        if !elevated {
            return;
        }

        unsafe {
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                -25_000,
                -25_000,
                400,
                40,
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(higher_band_win, SW_SHOWNOACTIVATE);
            SetWindowPos(
                higher_band_win,
                Some(HWND_TOPMOST),
                -25_000,
                -25_100,
                400,
                140,
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(widget, SW_SHOWNOACTIVATE);
            SetWindowPos(
                widget,
                Some(HWND_TOPMOST),
                -24_900,
                -25_000,
                120,
                40,
                SWP_NOACTIVATE,
            )
            .unwrap();
            // Put Band-1 tray above Band-1 widget so repair must choose a Band-1 anchor
            // without picking `higher_band_win` (band 16) as `insert_after`.
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            )
            .unwrap();
        }

        assert!(
            ensure_just_above_taskbar(widget, tray),
            "cross-band higher window above Band-1 tray must not break Band-1 repair"
        );
        assert!(
            unsafe { IsWindowVisible(widget).as_bool() },
            "Band-1 widget must stay visible when higher-band window sits above tray"
        );
        assert!(
            is_window_above(widget, tray),
            "Band-1 widget must remain immediately above Band-1 tray"
        );
    }

    #[test]
    fn tray_in_elevated_band_ignores_lower_band_occluders_and_omission_from_enum() {
        assert_eq!(
            occlusion_result_after_enum(false, Some(6), None),
            OcclusionCheck::Clear
        );
        assert_eq!(
            occlusion_result_after_enum(false, Some(1), None),
            OcclusionCheck::TrayMissing
        );
        assert_eq!(
            occlusion_result_after_enum(false, None, None),
            OcclusionCheck::TrayMissing
        );
        assert!(fullscreen_above_result(false, Some(6), true));
        assert!(!fullscreen_above_result(false, Some(1), true));

        let _zorder = z_order_test_lock();
        let _dpi = DpiContextGuard::per_monitor_v2();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_topmost_test_window(hinstance, class_name);
        let lower_band_popup = create_topmost_test_window(hinstance, class_name);
        let _windows = TestWindows(vec![tray, lower_band_popup]);

        let mon = rect(-25_000, -25_000, -23_080, -23_920);
        let widget_rect = rect(-23_500, -23_968, -23_300, -23_920);
        unsafe {
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                -25_000,
                -23_968,
                1920,
                48,
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(lower_band_popup, SW_SHOWNOACTIVATE);
            SetWindowPos(
                lower_band_popup,
                Some(HWND_TOPMOST),
                mon.left,
                mon.top,
                mon.width(),
                mon.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
        }

        // With a Band-1 tray beneath it, the Band-1 fullscreen/overlapping window
        // acts as an occluder and a fullscreen coverer.
        assert_eq!(
            check_rect_occluded_above_taskbar_in_band(tray, HWND::default(), widget_rect, Some(1)),
            OcclusionCheck::Occluded(lower_band_popup)
        );
        assert!(is_fullscreen_window_above_taskbar_in_band(
            mon,
            tray,
            HWND::default(),
            Some(1),
        ));

        // When Windows promotes `Shell_TrayWnd` into an immersive band (band 6 > 1),
        // Band-1 windows sit beneath it and must never count as above-tray occluders.
        assert_eq!(
            check_rect_occluded_above_taskbar_in_band(tray, HWND::default(), widget_rect, Some(6)),
            OcclusionCheck::Clear
        );
        assert!(!is_fullscreen_window_above_taskbar_in_band(
            mon,
            tray,
            HWND::default(),
            Some(6),
        ));
    }

    #[test]
    fn fullscreen_suppression_restores_immediately_on_cached_layout_without_stealing_clicks() {
        let _zorder = z_order_test_lock();
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _com = ComMtaGuard::enter();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd]);
        let bar = rect(-10_000, -10_000, -8_080, -9_952);
        let notify = rect(-8_300, -10_000, -8_080, -9_952);
        let env = TaskbarEnv {
            tray_hwnd: tray,
            taskbar_rect: bar,
            tray_notify_rect: Some(notify),
            dpi: 96,
        };
        unsafe {
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                bar.left,
                bar.top,
                bar.width(),
                bar.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
            .unwrap();
        }
        let config = Config {
            enabled: true,
            ..Config::default()
        };
        let mut metrics = Metrics::default();
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = notifications.clone();
        let scanner = Arc::new(UiaScanner {
            state: Arc::new((
                Mutex::new(UiaShared {
                    cache: UiaCache {
                        tray_hwnd: tray.0 as isize,
                        taskbar_rect: bar,
                        rects: Some(vec![notify]),
                        generation: 0,
                    },
                    ..Default::default()
                }),
                Condvar::new(),
            )),
            worker: Mutex::new(None),
        });
        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: false,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: config.clone(),
                metrics,
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(move || {
                    observed.fetch_add(1, Ordering::Relaxed);
                }),
            }),
            callback: Arc::new(|_| {}),
            renderer: Renderer::new(),
            uia_scanner: scanner,
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: Some((tray.0 as isize, bar, Some(notify), 96)),
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        // Initial confirmed visibility.
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);
        assert!(ctx.accepts_input());
        assert!(ctx.restore_from_cached_layout);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);

        // Simulate entering fullscreen: geometry is preserved, background UIA
        // revalidation is marked pending, and the widget hides.
        ctx.pending_uia_scan = true;
        ctx.hide("env_unavailable_or_fullscreen", None);
        assert!(!ctx.last_visible);
        assert!(!unsafe { IsWindowVisible(hwnd).as_bool() });
        assert_eq!(notifications.load(Ordering::Relaxed), 2);

        // Simulate Alt+Tab out of fullscreen with updated metrics before the
        // background UIA scan completes: widget must reappear immediately using
        // its cached layout, while passing clicks through (`WS_EX_TRANSPARENT`).
        metrics.cpu_pct = Some(42.0);
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(
            ctx.last_visible && unsafe { IsWindowVisible(hwnd).as_bool() },
            "widget must reappear immediately on cached layout when returning from fullscreen"
        );
        assert!(
            !ctx.accepts_input(),
            "widget must remain click-through until UIA verification completes"
        );
        assert_eq!(notifications.load(Ordering::Relaxed), 3);
        assert!(
            ctx.uia_scanner
                .snapshot_for(tray.0 as isize, bar)
                .is_some_and(|s| !s.current),
            "cached occupancy must remain available while background verification runs"
        );

        // Once the background UIA scan finishes, hit testing is restored.
        {
            let mut state = ctx.uia_scanner.state.0.lock().unwrap();
            let target = state.pending.take().unwrap();
            assert!(state.publish(target, Some(vec![notify])));
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);
        assert!(ctx.accepts_input());
        assert_eq!(notifications.load(Ordering::Relaxed), 3);

        // If geometry actually changed while in fullscreen (e.g. resolution/DPI
        // change), returning must invalidate stale cache and wait for UIA.
        ctx.pending_uia_scan = true;
        ctx.hide("env_unavailable_or_fullscreen", None);
        let mut changed_env = env;
        changed_env.taskbar_rect.top -= 4;
        ctx.refresh_taskbar_env(&config, &metrics, changed_env, false);
        assert!(
            !ctx.last_visible && unsafe { !IsWindowVisible(hwnd).as_bool() },
            "changed taskbar geometry after fullscreen must wait for fresh UIA scan"
        );
    }

    #[test]
    fn fullscreen_hook_tracks_suppressing_window_moves_after_foreground_leaves() {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;
        use windows::Win32::UI::WindowsAndMessaging::{
            MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
        };

        let _zorder = z_order_test_lock();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![hwnd]);

        enum FsCmd {
            Move,
            Exit,
        }
        let (cmd_tx, cmd_rx) = mpsc::channel::<FsCmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<(isize, u32)>();
        let (ack_tx, ack_rx) = mpsc::channel::<()>();

        let fs_join = std::thread::spawn(move || {
            let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
            let class_name = register_window_class(hinstance).unwrap();
            let fs_hwnd =
                create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
            unsafe {
                SetWindowPos(
                    fs_hwnd,
                    None,
                    -22_000,
                    -22_000,
                    1920,
                    1080,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .unwrap();
                let _ = ShowWindow(fs_hwnd, SW_SHOWNOACTIVATE);
            }
            let tid = unsafe { GetCurrentThreadId() };
            ready_tx.send((fs_hwnd.0 as isize, tid)).unwrap();
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    FsCmd::Move => {
                        unsafe {
                            SetWindowPos(
                                fs_hwnd,
                                None,
                                -22_000,
                                -22_000,
                                800,
                                600,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            )
                            .unwrap();
                        }
                        ack_tx.send(()).unwrap();
                    }
                    FsCmd::Exit => {
                        unsafe {
                            let _ = DestroyWindow(fs_hwnd);
                        }
                        break;
                    }
                }
            }
        });

        let (fs_raw, fs_tid) = ready_rx.recv().expect("fullscreen thread ready");
        let fs_hwnd = HWND(fs_raw as *mut std::ffi::c_void);
        LIVE_HWND.store(hwnd.0 as isize, Ordering::Release);
        LIVE_FG_HWND.store(0, Ordering::Release);

        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: true,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: Config::default(),
                metrics: Metrics::default(),
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: None,
            uia_scanner: Arc::new(UiaScanner {
                state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
                worker: Mutex::new(None),
            }),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: None,
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        ctx.sync_fullscreen_hook(fs_hwnd);
        assert_eq!(ctx.tracked_fs_tid, fs_tid);
        assert!(!ctx.fs_loc_hook.is_invalid());

        REFRESH_PENDING.store(false, Ordering::Release);
        cmd_tx.send(FsCmd::Move).unwrap();
        ack_rx.recv().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut msg = MSG::default();
        while !REFRESH_PENDING.load(Ordering::Acquire) && Instant::now() < deadline {
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                let _ = unsafe { TranslateMessage(&msg) };
                unsafe { DispatchMessageW(&msg) };
            }
            if REFRESH_PENDING.load(Ordering::Acquire) {
                break;
            }
            let remaining_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(50) as u32;
            unsafe {
                let _ = MsgWaitForMultipleObjects(None, false, remaining_ms, QS_ALLINPUT);
            }
        }
        assert!(
            REFRESH_PENDING.load(Ordering::Acquire),
            "resizing a non-topmost fullscreen suppressor after foreground left must wake refresh"
        );

        ctx.sync_fullscreen_hook(HWND::default());
        assert_eq!(ctx.tracked_fs_tid, 0);
        assert_eq!(LIVE_FS_HWND.load(Ordering::Acquire), 0);
        LIVE_HWND.store(0, Ordering::Release);
        REFRESH_PENDING.store(false, Ordering::Release);

        let _ = cmd_tx.send(FsCmd::Exit);
        let _ = fs_join.join();
    }

    #[test]
    fn fullscreen_hook_wakes_immediately_when_suppressing_window_closes() {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;
        use windows::Win32::UI::WindowsAndMessaging::{
            MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
        };

        let _zorder = z_order_test_lock();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![hwnd]);

        let (close_tx, close_rx) = mpsc::channel::<()>();
        let (exit_tx, exit_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<(isize, u32)>();

        let fs_join = std::thread::spawn(move || {
            let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
            let class_name = register_window_class(hinstance).unwrap();
            let fs_hwnd =
                create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
            unsafe {
                SetWindowPos(
                    fs_hwnd,
                    None,
                    -22_000,
                    -22_000,
                    1920,
                    1080,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .unwrap();
                let _ = ShowWindow(fs_hwnd, SW_SHOWNOACTIVATE);
            }
            let tid = unsafe { GetCurrentThreadId() };
            ready_tx.send((fs_hwnd.0 as isize, tid)).unwrap();
            let _ = close_rx.recv();
            unsafe {
                let _ = DestroyWindow(fs_hwnd);
            }
            let _ = exit_rx.recv();
        });

        let (fs_raw, fs_tid) = ready_rx.recv().expect("fullscreen thread ready");
        let fs_hwnd = HWND(fs_raw as *mut std::ffi::c_void);
        LIVE_HWND.store(hwnd.0 as isize, Ordering::Release);
        LIVE_FG_HWND.store(0, Ordering::Release);

        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: true,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: Config::default(),
                metrics: Metrics::default(),
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: None,
            uia_scanner: Arc::new(UiaScanner {
                state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
                worker: Mutex::new(None),
            }),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: None,
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        ctx.sync_fullscreen_hook(fs_hwnd);
        assert_eq!(ctx.tracked_fs_tid, fs_tid);
        assert!(!ctx.fs_loc_hook.is_invalid());
        assert!(!ctx.fs_lifecycle_hook.is_invalid());
        assert!(!ctx.fs_cloak_hook.is_invalid());

        REFRESH_PENDING.store(false, Ordering::Release);
        FULLSCREEN_WAKE_PENDING.store(false, Ordering::Release);
        close_tx.send(()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut msg = MSG::default();
        while (!REFRESH_PENDING.load(Ordering::Acquire)
            || !FULLSCREEN_WAKE_PENDING.load(Ordering::Acquire))
            && Instant::now() < deadline
        {
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                let _ = unsafe { TranslateMessage(&msg) };
                unsafe { DispatchMessageW(&msg) };
            }
            if REFRESH_PENDING.load(Ordering::Acquire)
                && FULLSCREEN_WAKE_PENDING.load(Ordering::Acquire)
            {
                break;
            }
            let remaining_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(50) as u32;
            unsafe {
                let _ = MsgWaitForMultipleObjects(None, false, remaining_ms, QS_ALLINPUT);
            }
        }
        assert!(
            REFRESH_PENDING.load(Ordering::Acquire)
                && FULLSCREEN_WAKE_PENDING.load(Ordering::Acquire),
            "closing a suppressing fullscreen window must wake refresh and re-arm exit recheck"
        );

        // Top-level hide and cloak events via `fg_event_proc` must also arm fullscreen wake.
        for ev in [
            EVENT_OBJECT_HIDE,
            EVENT_OBJECT_CLOAKED,
            EVENT_OBJECT_UNCLOAKED,
        ] {
            REFRESH_PENDING.store(false, Ordering::Release);
            FULLSCREEN_WAKE_PENDING.store(false, Ordering::Release);
            unsafe {
                fg_event_proc(HWINEVENTHOOK::default(), ev, hwnd, OBJID_WINDOW.0, 0, 0, 0);
            }
            assert!(
                REFRESH_PENDING.load(Ordering::Acquire)
                    && FULLSCREEN_WAKE_PENDING.load(Ordering::Acquire),
                "top-level event {ev:#x} must set FULLSCREEN_WAKE_PENDING and wake refresh"
            );
        }

        ctx.sync_fullscreen_hook(HWND::default());
        assert_eq!(ctx.tracked_fs_tid, 0);
        assert!(ctx.fs_loc_hook.is_invalid());
        assert!(ctx.fs_lifecycle_hook.is_invalid());
        assert!(ctx.fs_cloak_hook.is_invalid());
        assert_eq!(LIVE_FS_HWND.load(Ordering::Acquire), 0);
        let _ = exit_tx.send(());
        let _ = fs_join.join();
        LIVE_HWND.store(0, Ordering::Release);
        REFRESH_PENDING.store(false, Ordering::Release);
        FULLSCREEN_WAKE_PENDING.store(false, Ordering::Release);
    }

    #[test]
    fn wnd_proc_handles_reentrant_show_and_messages_without_aliasing_or_panic() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

        struct LiveGuard(isize, bool);
        impl Drop for LiveGuard {
            fn drop(&mut self) {
                LIVE_HWND.store(self.0, Ordering::Release);
                REFRESH_PENDING.store(self.1, Ordering::Release);
            }
        }

        let _zorder = z_order_test_lock();
        let _live = LiveGuard(
            LIVE_HWND.load(Ordering::Acquire),
            REFRESH_PENDING.load(Ordering::Acquire),
        );
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();

        let callback_saw_unlocked_ctx = Arc::new(AtomicBool::new(false));
        let callback_flag = callback_saw_unlocked_ctx.clone();
        let state_slot = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state_slot_cb = state_slot.clone();

        let state_raw = Box::into_raw(Box::new(WndProcState::new(
            0,
            0,
            WindowCtx {
                hwnd: HWND::default(),
                in_elevated_band: false,
                user_closing: false,
                shared: Arc::new(Mutex::new(SharedState {
                    config: Config::default(),
                    metrics: Metrics::default(),
                    generation: 0,
                })),
                visibility: Arc::new(MonitorVisibility {
                    visible: AtomicBool::new(false),
                    notifier: Arc::new(|| {}),
                }),
                callback: Arc::new(move |_| {
                    let raw = state_slot_cb.load(Ordering::Acquire) as *const WndProcState;
                    if !raw.is_null() {
                        let st = unsafe { &*raw };
                        callback_flag.store(st.ctx.try_borrow_mut().is_ok(), Ordering::Release);
                    }
                }),
                renderer: None,
                uia_scanner: Arc::new(UiaScanner {
                    state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
                    worker: Mutex::new(None),
                }),
                light_taskbar: false,
                hovered: false,
                tracking_mouse: false,
                last_display: None,
                last_size: (0, 0),
                last_pos: None,
                last_visible: false,
                last_present_error: None,
                last_anchor_slot: None,
                last_compact: false,
                last_stacked: false,
                last_env_geom: None,
                pending_uia_scan: false,
                invalidate_uia_layout: false,
                restore_from_cached_layout: false,
                tracked_fg_tid: 0,
                fg_loc_hook: HWINEVENTHOOK::default(),
                tracked_fs_tid: 0,
                fs_loc_hook: HWINEVENTHOOK::default(),
                fs_lifecycle_hook: HWINEVENTHOOK::default(),
                fs_cloak_hook: HWINEVENTHOOK::default(),
                tracked_tray_tid: 0,
                tray_hook: HWINEVENTHOOK::default(),
                topmost_hooks: Vec::new(),
                last_fg_hwnd: 0,
                fullscreen_recheck_remaining: 0,
                fullscreen_recheck_armed: false,
                hidden_by_occlusion: false,
                hidden_by_tray_missing: false,
                last_occluded_rect: None,
                applied_clip: None,
                clip_occluder: HWND::default(),
                topmost_recheck_armed: false,
            },
        )));
        state_slot.store(state_raw as usize, Ordering::Release);
        let state = unsafe { &*state_raw };
        let hwnd = create_monitor_window(hinstance, class_name, state_raw).unwrap();
        LIVE_HWND.store(hwnd.0 as isize, Ordering::Release);
        REFRESH_PENDING.store(false, Ordering::Release);

        // Synchronous ShowWindow (fires WM_SHOWWINDOW/WM_WINDOWPOSCHANGING/CHANGED)
        // plus synchronous SendMessageW of handled messages while `ctx` is mutably
        // borrowed must not create aliasing `&mut` or panic `RefCell`.
        state.with_ctx_mut(hwnd, |ctx| {
            ctx.hwnd = hwnd;
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    -27_000,
                    -27_000,
                    20,
                    20,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            assert!(ctx.set_visible(true));
            assert!(ctx.set_interactive(true));
            unsafe {
                let _ = SendMessageW(hwnd, WM_SETTINGCHANGE, Some(WPARAM(0)), Some(LPARAM(0)));
                let _ = SendMessageW(hwnd, WM_APP_REFRESH, Some(WPARAM(0)), Some(LPARAM(0)));
            }
        });

        assert!(
            state.ctx.borrow().pending_uia_scan,
            "reentrant WM_SETTINGCHANGE layout scan must be drained after outer borrow ends"
        );
        assert!(
            REFRESH_PENDING.load(Ordering::Acquire),
            "reentrant WM_APP_REFRESH must re-queue refresh after outer borrow ends"
        );

        unsafe {
            let _ = SendMessageW(hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        assert!(
            callback_saw_unlocked_ctx.load(Ordering::Acquire),
            "action callback must execute without holding a WindowCtx borrow"
        );

        // Drain WM_QUIT posted by WM_CLOSE -> WM_DESTROY so the test thread's queue stays clean.
        unsafe {
            let _ = SendMessageW(hwnd, WM_CLOSE, Some(WPARAM(0)), Some(LPARAM(0)));
            assert_eq!(GetWindowLongPtrW(hwnd, GWLP_USERDATA), 0);
            let mut msg = MSG::default();
            while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut msg,
                None,
                windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
            )
            .as_bool()
            {}
            drop(Box::from_raw(state_raw));
        }
    }

    #[test]
    fn elevated_band_recheck_detects_background_topmost_promotion_and_demotion() {
        use std::sync::mpsc;

        let _zorder = z_order_test_lock();
        let _dpi = DpiContextGuard::per_monitor_v2();
        let _com = ComMtaGuard::enter();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();

        let tray = create_topmost_test_window(hinstance, class_name);
        let (hwnd, _) =
            create_elevated_monitor_window(hinstance, class_name, std::ptr::null()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd]);
        let bar = rect(-26_500, -26_500, -24_580, -26_452);
        let widget_rect = rect(-25_100, -26_500, -24_980, -26_452);
        unsafe {
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                bar.left,
                bar.top,
                bar.width(),
                bar.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                widget_rect.left,
                widget_rect.top,
                widget_rect.width(),
                widget_rect.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
        }

        #[derive(Clone, Copy)]
        enum BgZCmd {
            PromoteTopmost,
            DemoteNoTopmost,
            Exit,
        }
        let (cmd_tx, cmd_rx) = mpsc::channel::<BgZCmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<u32>();
        let (ack_tx, ack_rx) = mpsc::channel::<()>();
        let bg_join = std::thread::spawn(move || {
            let _dpi = DpiContextGuard::per_monitor_v2();
            let hinst = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
            let cls = register_window_class(hinst).unwrap();
            let bg = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                    cls,
                    w!("bg_promote_test"),
                    WS_POPUP,
                    widget_rect.left,
                    widget_rect.top,
                    widget_rect.width(),
                    widget_rect.height(),
                    None,
                    None,
                    Some(hinst),
                    None,
                )
                .unwrap()
            };
            unsafe {
                let _ = ShowWindow(bg, SW_SHOWNOACTIVATE);
                // Start visible and demoted behind `tray` without stealing desktop focus;
                // Win32 then allows subsequent SWP_NOACTIVATE topmost promotion from this thread.
                SetWindowPos(
                    bg,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
                .unwrap();
            }
            ready_tx.send(unsafe { GetCurrentThreadId() }).unwrap();
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    BgZCmd::PromoteTopmost => {
                        unsafe {
                            SetWindowPos(
                                bg,
                                Some(HWND_TOPMOST),
                                0,
                                0,
                                0,
                                0,
                                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                            )
                            .unwrap();
                        }
                        ack_tx.send(()).unwrap();
                    }
                    BgZCmd::DemoteNoTopmost => {
                        unsafe {
                            SetWindowPos(
                                bg,
                                Some(HWND_NOTOPMOST),
                                0,
                                0,
                                0,
                                0,
                                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                            )
                            .unwrap();
                        }
                        ack_tx.send(()).unwrap();
                    }
                    BgZCmd::Exit => break,
                }
            }
            unsafe {
                let _ = DestroyWindow(bg);
            }
        });

        let bg_tid = ready_rx.recv().unwrap();
        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: true,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: Config::default(),
                metrics: Metrics::default(),
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: None,
            uia_scanner: Arc::new(UiaScanner {
                state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
                worker: Mutex::new(None),
            }),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (widget_rect.width(), widget_rect.height()),
            last_pos: Some((widget_rect.left, widget_rect.top)),
            last_visible: true,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: None,
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        assert!(!ctx.recheck_topmost_occlusion_with_tray(tray));
        assert!(ctx.last_visible);
        assert!(!ctx.topmost_hooks.iter().any(|&(t, _)| t == bg_tid));

        // Promote already-visible background window to TOPMOST with no move/size/activate.
        cmd_tx.send(BgZCmd::PromoteTopmost).unwrap();
        ack_rx.recv().unwrap();
        assert!(!ctx.recheck_topmost_occlusion_with_tray(tray));
        assert!(
            !ctx.last_visible && unsafe { !IsWindowVisible(hwnd).as_bool() },
            "pure Z-order promotion of an overlapping background window must hide elevated widget"
        );
        assert!(ctx.hidden_by_occlusion);
        assert!(ctx.topmost_recheck_armed);
        assert!(ctx.topmost_hooks.iter().any(|&(t, _)| t == bg_tid));

        // Demote it back behind the taskbar with no move/size/activate.
        cmd_tx.send(BgZCmd::DemoteNoTopmost).unwrap();
        ack_rx.recv().unwrap();
        assert!(
            ctx.recheck_topmost_occlusion_with_tray(tray),
            "demoting the overlapping background window must request refresh to restore"
        );
        assert!(!ctx.topmost_hooks.iter().any(|&(t, _)| t == bg_tid));

        ctx.hide("env_unavailable_or_fullscreen", None);
        assert!(!ctx.hidden_by_occlusion);
        assert!(!ctx.topmost_recheck_armed);

        let _ = cmd_tx.send(BgZCmd::Exit);
        let _ = bg_join.join();
    }

    #[test]
    fn recheck_topmost_occlusion_prefers_blocked_candidate_rect_over_stale_last_rect() {
        let _zorder = z_order_test_lock();
        let _dpi = DpiContextGuard::per_monitor_v2();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_topmost_test_window(hinstance, class_name);
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null()).unwrap();
        let popup_old = create_topmost_test_window(hinstance, class_name);
        let popup_new = create_topmost_test_window(hinstance, class_name);
        let _windows = TestWindows(vec![hwnd, tray, popup_old, popup_new]);

        let rect_old = rect(-22_500, -24_000, -22_400, -23_960);
        let rect_new = rect(-22_900, -24_000, -22_800, -23_960);
        unsafe {
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                -24_000,
                -24_000,
                1920,
                48,
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            for (popup, r) in [(popup_old, rect_old), (popup_new, rect_new)] {
                SetWindowPos(
                    popup,
                    Some(HWND_TOP),
                    r.left,
                    r.top,
                    r.width(),
                    r.height(),
                    SWP_NOACTIVATE,
                )
                .unwrap();
                let _ = ShowWindow(popup, SW_SHOWNOACTIVATE);
            }
        }

        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: true,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: Config::default(),
                metrics: Metrics::default(),
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: None,
            uia_scanner: Arc::new(UiaScanner {
                state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
                worker: Mutex::new(None),
            }),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (rect_old.width(), rect_old.height()),
            last_pos: Some((rect_old.left, rect_old.top)),
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: None,
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        // Simulate a relocation to `rect_new` blocked by `popup_new` before `last_pos` updates.
        ctx.last_occluded_rect = Some(rect_new);
        ctx.hide("occluded_above_tray", Some(popup_new));
        assert!(ctx.hidden_by_occlusion);
        assert_eq!(ctx.last_occluded_rect, Some(rect_new));
        assert_eq!(ctx.last_rect(), Some(rect_old));
        assert!(!ctx.recheck_topmost_occlusion_with_tray(tray));

        // Clear the blocked candidate slot while `rect_old` stays occluded by `popup_old`.
        unsafe {
            let _ = ShowWindow(popup_new, SW_HIDE);
        }
        assert!(
            ctx.recheck_topmost_occlusion_with_tray(tray),
            "recheck must inspect `last_occluded_rect` before stale `last_rect()`"
        );
    }

    #[test]
    fn uia_unavailable_scans_do_not_spin_and_back_off_progressively() {
        let _zorder = z_order_test_lock();
        let scanner = Arc::new(UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        });
        let bar = rect(-24_000, -24_000, -22_080, -23_952);
        let t0 = Instant::now();

        scanner.request_scan(1, bar, true);
        let t1_target = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(
            !scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish_at(t1_target, None, t0),
            "unavailable -> unavailable must not wake refresh in a self-perpetuating loop"
        );

        // Repeated failures escalate worker wait: 500ms -> 1s -> 2s -> 5s cap.
        let mut cursor = t0;
        let mut last_scan = Some(t0);
        for expected in [
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(5),
            Duration::from_secs(5),
        ] {
            scanner.request_scan(1, bar, false);
            let gen_after_first = scanner.state.0.lock().unwrap().generation;
            // Duplicate requests while a retry is already queued do not churn generation.
            scanner.request_scan(1, bar, false);
            assert_eq!(scanner.state.0.lock().unwrap().generation, gen_after_first);

            let wait = scanner
                .state
                .0
                .lock()
                .unwrap()
                .next_wait(last_scan, cursor)
                .unwrap();
            assert_eq!(wait, expected);
            assert!(
                scanner
                    .state
                    .0
                    .lock()
                    .unwrap()
                    .next_wait(last_scan, cursor + expected)
                    .is_none()
            );
            cursor += expected;
            last_scan = Some(cursor);
            let target = scanner.state.0.lock().unwrap().pending.take().unwrap();
            assert!(
                !scanner
                    .state
                    .0
                    .lock()
                    .unwrap()
                    .publish_at(target, None, cursor)
            );
        }

        // A valid scan clears the failure backoff immediately.
        scanner.request_scan(1, bar, true);
        let recovered = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(scanner.state.0.lock().unwrap().publish_at(
            recovered,
            Some(vec![rect(-22_300, -24_000, -22_080, -23_952)]),
            cursor,
        ));
        {
            let guard = scanner.state.0.lock().unwrap();
            assert_eq!(guard.consecutive_failures, 0);
            assert!(guard.backoff_until.is_none());
            assert_eq!(
                guard.next_wait(last_scan, cursor),
                Some(UIA_MIN_SCAN_INTERVAL)
            );
        }

        // Verify WindowCtx::refresh_taskbar_env does not wake refresh in a loop after failure.
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_monitor_window(hinstance, class_name, std::ptr::null()).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd]);
        let env = TaskbarEnv {
            tray_hwnd: tray,
            taskbar_rect: bar,
            tray_notify_rect: None,
            dpi: 96,
        };
        let config = Config {
            enabled: true,
            ..Config::default()
        };
        let metrics = Metrics::default();
        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: false,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: config.clone(),
                metrics,
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: None,
            uia_scanner: scanner.clone(),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: Some((tray.0 as isize, bar, None, 96)),
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        let first_scan = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(
            !scanner.state.0.lock().unwrap().publish(first_scan, None),
            "unavailable scan for new target without cached rects must not post_refresh"
        );
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        let retry_gen = scanner.state.0.lock().unwrap().generation;
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert_eq!(
            scanner.state.0.lock().unwrap().generation,
            retry_gen,
            "repeated refresh calls while retry is queued must not churn UIA generation"
        );
        assert!(!ctx.last_visible);
    }

    #[test]
    fn uia_failed_scan_requeues_retry_in_worker_without_caller_refresh() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let bar = rect(-24_000, -24_000, -22_080, -23_952);
        let t0 = Instant::now();

        scanner.request_scan(1, bar, true);
        let t1 = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(!scanner.state.0.lock().unwrap().publish_at(t1, None, t0));

        // Even with no further `request_scan` (e.g. paused sampling), the worker
        // re-queues the failed target and honors progressive backoff before retrying.
        let t2 = {
            let mut guard = scanner.state.0.lock().unwrap();
            assert_eq!(
                guard.next_wait(Some(t0), t0),
                Some(Duration::from_millis(500))
            );
            guard
                .pending
                .take()
                .expect("failed scan must leave retry queued in UiaShared::pending")
        };

        let t1_step = t0 + Duration::from_millis(500);
        assert!(
            !scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish_at(t2, None, t1_step)
        );
        let t3 = {
            let mut guard = scanner.state.0.lock().unwrap();
            assert_eq!(
                guard.next_wait(Some(t1_step), t1_step),
                Some(Duration::from_secs(1))
            );
            guard
                .pending
                .take()
                .expect("repeated failure must stay queued for next backoff")
        };

        assert!(scanner.state.0.lock().unwrap().publish_at(
            t3,
            Some(vec![rect(-22_300, -24_000, -22_080, -23_952)]),
            t1_step + Duration::from_secs(1),
        ));
        assert!(scanner.state.0.lock().unwrap().pending.is_none());
    }

    #[test]
    fn tray_notify_resize_repositions_on_cached_layout_without_hiding_or_discarding_uia() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _zorder = z_order_test_lock();
        let _com = ComMtaGuard::enter();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd]);
        let bar = rect(-10_000, -10_000, -8_080, -9_952);
        let notify_initial = rect(-8_300, -10_000, -8_080, -9_952);
        let env = TaskbarEnv {
            tray_hwnd: tray,
            taskbar_rect: bar,
            tray_notify_rect: Some(notify_initial),
            dpi: 96,
        };
        unsafe {
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                bar.left,
                bar.top,
                bar.width(),
                bar.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
            .unwrap();
        }

        let config = Config {
            enabled: true,
            ..Config::default()
        };
        let metrics = Metrics {
            cpu_pct: Some(25.0),
            ram_pct: Some(50.0),
            ..Metrics::default()
        };
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = notifications.clone();
        let scanner = Arc::new(UiaScanner {
            state: Arc::new((
                Mutex::new(UiaShared {
                    cache: UiaCache {
                        tray_hwnd: tray.0 as isize,
                        taskbar_rect: bar,
                        rects: Some(vec![notify_initial]),
                        generation: 0,
                    },
                    ..Default::default()
                }),
                Condvar::new(),
            )),
            worker: Mutex::new(None),
        });
        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: false,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: config.clone(),
                metrics,
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(move || {
                    observed.fetch_add(1, Ordering::Relaxed);
                }),
            }),
            callback: Arc::new(|_| {}),
            renderer: Renderer::new(),
            uia_scanner: scanner.clone(),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: Some((tray.0 as isize, bar, Some(notify_initial), 96)),
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);
        assert!(ctx.accepts_input());
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        let initial_pos = ctx.last_pos.expect("widget must have initial placement");

        // Simulate `TrayNotifyWnd` growing leftward (e.g. IME/privacy indicator appearing
        // on foreground window switch). The widget must stay visible without a hide/show
        // transition, slide left immediately using the updated `tray_notify_rect` + cached
        // UIA rects, and become click-through while the soft UIA recheck is in flight.
        let notify_expanded = rect(-8_360, -10_000, -8_080, -9_952);
        let expanded_env = TaskbarEnv {
            tray_notify_rect: Some(notify_expanded),
            ..env
        };
        ctx.refresh_taskbar_env(&config, &metrics, expanded_env, false);
        assert!(
            ctx.last_visible && unsafe { IsWindowVisible(hwnd).as_bool() },
            "TrayNotifyWnd resize must not hide the taskbar monitor"
        );
        assert_eq!(
            notifications.load(Ordering::Relaxed),
            1,
            "TrayNotifyWnd resize must not emit a visibility transition"
        );
        let shifted_pos = ctx.last_pos.expect("widget must remain placed");
        assert!(
            shifted_pos.0 < initial_pos.0,
            "expanded TrayNotifyWnd must immediately shift widget left (initial_x={}, shifted_x={})",
            initial_pos.0,
            shifted_pos.0
        );
        assert!(
            !ctx.accepts_input(),
            "widget must be click-through while soft UIA recheck is pending"
        );
        assert!(
            scanner
                .snapshot_for(tray.0 as isize, bar)
                .is_some_and(|s| !s.current),
            "TrayNotifyWnd resize must preserve cached UIA rects during background recheck"
        );

        // Once the background UIA scan completes, publish wakes refresh and hit-testing is restored.
        {
            let mut state = scanner.state.0.lock().unwrap();
            let target = state.pending.take().unwrap();
            assert!(state.publish(target, Some(vec![notify_expanded])));
        }
        ctx.refresh_taskbar_env(&config, &metrics, expanded_env, false);
        assert!(ctx.last_visible);
        assert!(ctx.accepts_input());
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn uia_superseded_scan_populates_empty_cache_while_keeping_provisional_status() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let bar = rect(-24_000, -24_000, -22_080, -23_952);
        let t0 = Instant::now();

        // Start from an invalidated cache (`rects = None`).
        scanner.request_scan(1, bar, true);
        let target_1 = scanner.state.0.lock().unwrap().pending.take().unwrap();

        // While `target_1` is running on the worker thread, a soft recheck arrives and bumps generation.
        scanner.request_scan(1, bar, false);
        let target_2 = scanner.state.0.lock().unwrap().pending.unwrap();
        assert!(target_2.generation > target_1.generation);

        // When `target_1` finishes with valid rects, `publish_at` must populate the empty
        // cache immediately and wake refresh, while keeping `current == false` until `target_2` completes.
        let sample_rects = vec![rect(-22_300, -24_000, -22_080, -23_952)];
        assert!(scanner.state.0.lock().unwrap().publish_at(
            target_1,
            Some(sample_rects.clone()),
            t0,
        ));
        let snap = scanner
            .snapshot_for(1, bar)
            .expect("superseded post-invalidation scan must populate empty cache");
        assert!(!snap.current);
        assert_eq!(&*snap.rects, &sample_rects[..]);

        // Now complete `target_2`: snapshot becomes current and wakes refresh so hit-testing can be restored.
        let target_2_taken = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(scanner.state.0.lock().unwrap().publish_at(
            target_2_taken,
            Some(sample_rects),
            t0 + Duration::from_millis(50),
        ));
        assert!(scanner.snapshot_for(1, bar).unwrap().current);

        // Conversely, a scan started BEFORE a hard `invalidate = true` boundary must never
        // repopulate the invalidated cache.
        scanner.request_scan(1, bar, false);
        let pre_inval_target = scanner.state.0.lock().unwrap().pending.take().unwrap();
        scanner.request_scan(1, bar, true);
        assert!(!scanner.state.0.lock().unwrap().publish_at(
            pre_inval_target,
            Some(vec![rect(-22_300, -24_000, -22_080, -23_952)]),
            t0 + Duration::from_millis(100),
        ));
        assert!(
            scanner.snapshot_for(1, bar).is_none(),
            "scan from before an invalidation boundary must not populate cache"
        );
    }

    #[test]
    fn occlusion_result_rejects_unverified_band1_occluder_when_tray_missing_from_enum() {
        let dummy = HWND(0x1234 as *mut std::ffi::c_void);
        assert_eq!(
            occlusion_result_after_enum(false, Some(1), Some(dummy)),
            OcclusionCheck::TrayMissing,
            "Band-1 occluder without seeing tray in EnumWindows must be treated as TrayMissing"
        );
        assert_eq!(
            occlusion_result_after_enum(true, Some(1), Some(dummy)),
            OcclusionCheck::Occluded(dummy)
        );
        assert_eq!(
            occlusion_result_after_enum(false, Some(6), Some(dummy)),
            OcclusionCheck::Occluded(dummy)
        );

        let _zorder = z_order_test_lock();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_topmost_test_window(hinstance, class_name);
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd]);
        unsafe {
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                -24_000,
                -24_000,
                400,
                48,
                SWP_NOACTIVATE,
            )
            .unwrap();
        }

        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: true,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: Config::default(),
                metrics: Metrics::default(),
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: None,
            uia_scanner: Arc::new(UiaScanner {
                state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
                worker: Mutex::new(None),
            }),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (120, 40),
            last_pos: Some((-23_800, -24_000)),
            last_visible: true,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: None,
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        ctx.hide("tray_missing_in_enum", None);
        assert!(ctx.hidden_by_tray_missing);
        assert!(
            ctx.topmost_recheck_armed,
            "tray_missing_in_enum hide must arm the 500ms recovery timer"
        );
        assert!(
            ctx.recheck_topmost_occlusion_with_tray(tray),
            "recheck_topmost_occlusion_with_tray must wake recovery once tray is visible in EnumWindows"
        );
    }

    #[test]
    fn fg_event_proc_wakes_on_tracked_occluder_destroy_and_ignores_non_tray_movesize() {
        use std::sync::atomic::Ordering;

        struct EventGlobalsGuard {
            hwnd: isize,
            tray: isize,
            occluder: isize,
            refresh: bool,
            layout: bool,
        }
        impl Drop for EventGlobalsGuard {
            fn drop(&mut self) {
                LIVE_HWND.store(self.hwnd, Ordering::Release);
                LIVE_TRAY_HWND.store(self.tray, Ordering::Release);
                LIVE_OCCLUDER_HWND.store(self.occluder, Ordering::Release);
                REFRESH_PENDING.store(self.refresh, Ordering::Release);
                LAYOUT_RECHECK_PENDING.store(self.layout, Ordering::Release);
            }
        }

        let _zorder = z_order_test_lock();
        let _guard = EventGlobalsGuard {
            hwnd: LIVE_HWND.load(Ordering::Acquire),
            tray: LIVE_TRAY_HWND.load(Ordering::Acquire),
            occluder: LIVE_OCCLUDER_HWND.load(Ordering::Acquire),
            refresh: REFRESH_PENDING.load(Ordering::Acquire),
            layout: LAYOUT_RECHECK_PENDING.load(Ordering::Acquire),
        };

        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let tray = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let occluder = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let unrelated = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![hwnd, tray, occluder, unrelated]);

        LIVE_HWND.store(hwnd.0 as isize, Ordering::Release);
        LIVE_TRAY_HWND.store(tray.0 as isize, Ordering::Release);
        LIVE_OCCLUDER_HWND.store(occluder.0 as isize, Ordering::Release);

        // Destroying an unrelated window must not wake refresh.
        REFRESH_PENDING.store(false, Ordering::Release);
        unsafe {
            fg_event_proc(
                HWINEVENTHOOK::default(),
                EVENT_OBJECT_DESTROY,
                unrelated,
                OBJID_WINDOW.0,
                0,
                0,
                0,
            );
        }
        assert!(!REFRESH_PENDING.load(Ordering::Acquire));

        // Destroying the tracked occluder window must immediately wake refresh.
        unsafe {
            fg_event_proc(
                HWINEVENTHOOK::default(),
                EVENT_OBJECT_DESTROY,
                occluder,
                OBJID_WINDOW.0,
                0,
                0,
                0,
            );
        }
        assert!(
            REFRESH_PENDING.load(Ordering::Acquire),
            "EVENT_OBJECT_DESTROY for LIVE_OCCLUDER_HWND must wake refresh"
        );

        // EVENT_SYSTEM_MOVESIZEEND on a non-tray window must wake refresh without setting LAYOUT_RECHECK_PENDING.
        REFRESH_PENDING.store(false, Ordering::Release);
        LAYOUT_RECHECK_PENDING.store(false, Ordering::Release);
        unsafe {
            fg_event_proc(
                HWINEVENTHOOK::default(),
                EVENT_SYSTEM_MOVESIZEEND,
                unrelated,
                OBJID_WINDOW.0,
                0,
                0,
                0,
            );
        }
        assert!(REFRESH_PENDING.load(Ordering::Acquire));
        assert!(
            !LAYOUT_RECHECK_PENDING.load(Ordering::Acquire),
            "non-tray EVENT_SYSTEM_MOVESIZEEND must not queue a taskbar UIA layout recheck"
        );

        // EVENT_SYSTEM_MOVESIZEEND on `LIVE_TRAY_HWND` must queue `LAYOUT_RECHECK_PENDING`.
        REFRESH_PENDING.store(false, Ordering::Release);
        unsafe {
            fg_event_proc(
                HWINEVENTHOOK::default(),
                EVENT_SYSTEM_MOVESIZEEND,
                tray,
                OBJID_WINDOW.0,
                0,
                0,
                0,
            );
        }
        assert!(REFRESH_PENDING.load(Ordering::Acquire));
        assert!(LAYOUT_RECHECK_PENDING.load(Ordering::Acquire));
    }

    #[test]
    fn core_geometry_change_remeasures_layout_after_pending_uia_wait() {
        let _zorder = z_order_test_lock();
        let _com = ComMtaGuard::enter();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![tray, hwnd]);
        let small_bar = rect(-10_000, -10_000, -8_080, -9_970);
        let notify_initial = rect(-8_300, -10_000, -8_080, -9_952);
        let env = TaskbarEnv {
            tray_hwnd: tray,
            taskbar_rect: small_bar,
            tray_notify_rect: Some(notify_initial),
            dpi: 96,
        };
        unsafe {
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                small_bar.left,
                small_bar.top,
                small_bar.width(),
                small_bar.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
            .unwrap();
        }

        let config = Config {
            enabled: true,
            font_size: 12.0,
            ..Config::default()
        };
        let metrics = Metrics {
            cpu_pct: Some(25.0),
            ram_pct: Some(50.0),
            ..Metrics::default()
        };
        let scanner = Arc::new(UiaScanner {
            state: Arc::new((
                Mutex::new(UiaShared {
                    cache: UiaCache {
                        tray_hwnd: tray.0 as isize,
                        taskbar_rect: small_bar,
                        rects: Some(vec![notify_initial]),
                        generation: 0,
                    },
                    ..Default::default()
                }),
                Condvar::new(),
            )),
            worker: Mutex::new(None),
        });
        let mut ctx = WindowCtx {
            hwnd,
            in_elevated_band: false,
            user_closing: false,
            shared: Arc::new(Mutex::new(SharedState {
                config: config.clone(),
                metrics,
                generation: 0,
            })),
            visibility: Arc::new(MonitorVisibility {
                visible: std::sync::atomic::AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            }),
            callback: Arc::new(|_| {}),
            renderer: Renderer::new(),
            uia_scanner: scanner.clone(),
            light_taskbar: false,
            hovered: false,
            tracking_mouse: false,
            last_display: None,
            last_size: (0, 0),
            last_pos: None,
            last_visible: false,
            last_present_error: None,
            last_anchor_slot: None,
            last_compact: false,
            last_stacked: false,
            last_env_geom: Some((tray.0 as isize, small_bar, Some(notify_initial), 96)),
            pending_uia_scan: false,
            invalidate_uia_layout: false,
            restore_from_cached_layout: false,
            tracked_fg_tid: 0,
            fg_loc_hook: HWINEVENTHOOK::default(),
            tracked_fs_tid: 0,
            fs_loc_hook: HWINEVENTHOOK::default(),
            fs_lifecycle_hook: HWINEVENTHOOK::default(),
            fs_cloak_hook: HWINEVENTHOOK::default(),
            tracked_tray_tid: 0,
            tray_hook: HWINEVENTHOOK::default(),
            topmost_hooks: Vec::new(),
            last_fg_hwnd: 0,
            fullscreen_recheck_remaining: 0,
            fullscreen_recheck_armed: false,
            hidden_by_occlusion: false,
            hidden_by_tray_missing: false,
            last_occluded_rect: None,
            applied_clip: None,
            clip_occluder: HWND::default(),
            topmost_recheck_armed: false,
        };

        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.last_visible);
        let old_size = ctx.last_size;

        // Grow taskbar height from 30 px to 48 px: the first refresh updates
        // `last_env_geom`, invalidates UIA, and returns early while UIA is pending.
        let grown_bar = rect(-10_000, -10_000, -8_080, -9_952);
        let grown_env = TaskbarEnv {
            taskbar_rect: grown_bar,
            ..env
        };
        ctx.refresh_taskbar_env(&config, &metrics, grown_env, true);
        assert!(!ctx.last_visible);

        // Once UIA finishes scanning the grown taskbar and triggers `refresh(false)`,
        // `ctx` must remeasure and rerender for the new 48 px taskbar bounds rather
        // than reusing the stale 30 px layout size.
        {
            let mut state = scanner.state.0.lock().unwrap();
            let target = state.pending.take().unwrap();
            assert!(state.publish(target, Some(vec![notify_initial])));
        }
        ctx.refresh_taskbar_env(&config, &metrics, grown_env, false);
        assert!(ctx.last_visible);
        let display = build_display(&config, &metrics, false, false, 96, false);
        let expected = ctx
            .renderer
            .as_mut()
            .unwrap()
            .measure_layout(&display, LayoutBounds::from_taskbar(grown_bar, 96))
            .unwrap();
        assert_ne!(old_size, (expected.px_w, expected.px_h));
        assert_eq!(
            ctx.last_size,
            (expected.px_w, expected.px_h),
            "layout must be remeasured after core taskbar geometry changes across a UIA wait"
        );
    }

    #[test]
    fn collect_cached_rects_propagates_element_error_instead_of_returning_partial_occupancy() {
        let ok_rects = [
            RECT {
                left: -9_900,
                top: -10_000,
                right: -9_850,
                bottom: -9_952,
            },
            RECT {
                left: -8_300,
                top: -10_000,
                right: -8_080,
                bottom: -9_952,
            },
        ];
        let full = collect_cached_rects(2, |i| Ok(ok_rects[i as usize])).unwrap();
        assert_eq!(full.len(), 2);

        let err = collect_cached_rects(2, |i| {
            if i == 0 {
                Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80040201u32 as i32,
                )))
            } else {
                Ok(ok_rects[1])
            }
        });
        assert!(
            err.is_err(),
            "per-element failure must abort scan rather than publishing a partial obstacle map"
        );
    }

    #[test]
    fn taskbar_flyout_shadow_does_not_clip_the_widget() {
        assert!(is_taskbar_flyout_class(Some(&ClassNameBuf::from_ascii(
            "Xaml_WindowedPopupClass"
        ))));
        assert!(is_taskbar_flyout_class(Some(&ClassNameBuf::from_ascii(
            "TopLevelWindowForOverflowXamlIsland"
        ))));
        assert!(!is_taskbar_flyout_class(Some(&ClassNameBuf::from_ascii(
            "#32768"
        ))));
        assert!(!is_taskbar_flyout_class(None));

        let taskbar = rect(0, 1000, 1920, 1050);
        let widget = rect(1400, 1008, 1600, 1040);
        let menu_frame = rect(1380, 820, 1620, 1018); // 18 px of shadow inside the taskbar
        let clamped = occluder_frame_for_target(menu_frame, true, Some(taskbar));
        assert_eq!(clamped.bottom, taskbar.top);
        assert!(
            !clamped.intersects(widget),
            "invisible flyout shadow must not be subtracted from the widget region"
        );
        assert!(local_clip_rects(widget, &[clamped]).is_empty());

        // Ordinary occluders keep their full frame, and we cannot clamp without
        // a known taskbar geometry.
        assert_eq!(
            occluder_frame_for_target(menu_frame, false, Some(taskbar)),
            menu_frame
        );
        assert_eq!(
            occluder_frame_for_target(menu_frame, true, None),
            menu_frame
        );
    }

    #[test]
    fn local_clip_rects_intersect_translate_sort_and_dedup() {
        let target = rect(100, 200, 180, 240);
        let clips = local_clip_rects(
            target,
            &[
                rect(150, 210, 200, 230), // partial overlap -> local (50,10,80,30)
                rect(-20, 0, 110, 250),   // clipped to target's left edge
                rect(150, 210, 200, 230), // duplicate
                rect(500, 500, 600, 600), // disjoint: dropped
                rect(100, 200, 180, 205), // top strip
            ],
        );
        assert_eq!(
            clips,
            vec![rect(0, 0, 10, 40), rect(0, 0, 80, 5), rect(50, 10, 80, 30),],
            "clips must be local to the widget, bounded by it, stable and unique"
        );
        assert!(local_clip_rects(target, &[rect(500, 500, 600, 600)]).is_empty());
    }

    #[test]
    fn clip_key_tolerance_skips_jitter_but_not_real_moves() {
        let base = AppliedClip {
            size: (200, 40),
            local_rects: vec![rect(20, 0, 90, 40)],
        };
        let jitter = AppliedClip {
            size: (200, 40),
            local_rects: vec![rect(21, 0, 91, 40)],
        };
        let moved = AppliedClip {
            size: (200, 40),
            local_rects: vec![rect(30, 0, 100, 40)],
        };
        let resized = AppliedClip {
            size: (220, 40),
            local_rects: vec![rect(21, 0, 91, 40)],
        };
        assert!(applied_clip_equivalent(&base, &jitter));
        assert!(!applied_clip_equivalent(&base, &moved));
        assert!(!applied_clip_equivalent(&base, &resized));
    }

    #[test]
    fn occlusion_outcome_distinguishes_clear_clipped_and_hidden() {
        let size = (100, 40);
        assert_eq!(occlusion_outcome(size, &[]), OcclusionOutcome::Clear);
        assert_eq!(
            occlusion_outcome((0, 40), &[rect(0, 0, 10, 10)]),
            OcclusionOutcome::Clear
        );

        let partial = local_clip_rects(rect(0, 0, 100, 40), &[rect(20, 0, 60, 40)]);
        assert_eq!(occlusion_outcome(size, &partial), OcclusionOutcome::Clipped);

        // Two strips that only cover the widget when their union is considered.
        let halves = local_clip_rects(
            rect(0, 0, 100, 40),
            &[rect(-10, 0, 50, 40), rect(50, 0, 110, 40)],
        );
        assert_eq!(
            occlusion_outcome(size, &halves),
            OcclusionOutcome::Hidden,
            "a full union must hide even when no single occluder covers the widget"
        );

        // Adjacent strips leave a one-pixel gap: still clipped, not hidden.
        let gapped = local_clip_rects(
            rect(0, 0, 100, 40),
            &[rect(0, 0, 49, 40), rect(50, 0, 100, 40)],
        );
        assert_eq!(occlusion_outcome(size, &gapped), OcclusionOutcome::Clipped);
    }

    #[test]
    fn failed_uia_scan_still_clears_cached_layout_and_counts_failure() {
        let scanner = UiaScanner::start();
        let target_rect = rect(0, 0, 1920, 48);
        let target = UiaTarget {
            tray_hwnd: 42,
            taskbar_rect: target_rect,
            generation: 0,
        };
        {
            let mut guard = scanner.state.0.lock().unwrap();
            assert!(guard.publish(target, Some(vec![rect(100, 0, 200, 48)])));
            assert!(guard.publish(target, None));
            assert_eq!(
                guard.cache.rects, None,
                "a real failure must clear cached pixels"
            );
            assert_eq!(guard.consecutive_failures, 1);
        }
        assert!(scanner.snapshot_for(42, target_rect).is_none());
    }
}
