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

mod fullscreen;
mod hooks;
mod identity;
mod occlusion;
mod placement;
mod render;
#[cfg(test)]
mod test_support;
mod thread;
mod uia;
mod visibility;
mod window;
mod zorder;

use self::hooks::{LIVE_HWND, post_refresh};
use self::thread::taskbar_thread_main;
use crate::taskbar_monitor::{Action, Config, Metrics};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP, WM_CLOSE};

/// Query `GetWindowBand` for `hwnd` when exported by `user32.dll`.
pub(crate) fn window_band(hwnd: HWND) -> Option<u32> {
    zorder::window_band(hwnd)
}

const WM_APP_REFRESH: u32 = WM_APP + 10;

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

    /// Ask the taskbar monitor thread to re-probe occlusion/layout (e.g. after
    /// our own GUI window moves or resizes, since own-process windows are not
    /// monitored with `EVENT_OBJECT_LOCATIONCHANGE`).
    pub fn request_refresh(&self) {
        let enabled = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .enabled;
        if enabled {
            self.post_refresh();
        }
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

    /// Whether the bar's controls run left-to-right, so its cross axis is the
    /// height. The widget's box fills that cross extent exactly, which is why
    /// the three layers must agree on which axis it is.
    fn is_horizontal(self) -> bool {
        self.width() >= self.height()
    }

    /// The bar's cross-axis extent in device pixels: the widget box is exactly
    /// this tall on a horizontal bar and exactly this wide on a vertical one.
    fn cross_extent(self) -> i32 {
        if self.is_horizontal() {
            self.height()
        } else {
            self.width()
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
