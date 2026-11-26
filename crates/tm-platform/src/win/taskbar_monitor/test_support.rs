//! Shared fixtures for the taskbar monitor tests.

use super::uia::{UiaScanner, UiaShared};
use super::visibility::WidgetState;
use super::window::WindowCtx;
use super::{MonitorVisibility, PixelRect, SharedState};
use crate::taskbar_monitor::{Config, Metrics};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, TranslateMessage, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

// Desktop Z-order (`HWND_TOPMOST` / `EnumWindows`) is session-global state;
// serialize tests that mutate or assert relative top-level Z-order.
pub(super) static ZORDER_TEST_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn z_order_test_lock() -> std::sync::MutexGuard<'static, ()> {
    ZORDER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) struct TestWindows(pub(super) Vec<HWND>);

impl Drop for TestWindows {
    fn drop(&mut self) {
        for hwnd in self.0.iter().rev() {
            unsafe {
                let _ = DestroyWindow(*hwnd);
            }
        }
    }
}

pub(super) fn create_topmost_test_window(hinstance: HINSTANCE, class_name: PCWSTR) -> HWND {
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

pub(super) fn rect(left: i32, top: i32, right: i32, bottom: i32) -> PixelRect {
    PixelRect {
        left,
        top,
        right,
        bottom,
    }
}

pub(super) struct DpiContextGuard(pub(super) windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);

impl DpiContextGuard {
    pub(super) fn per_monitor_v2() -> Self {
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
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

pub(super) fn test_window_ctx(hwnd: HWND, in_elevated_band: bool) -> WindowCtx {
    WindowCtx {
        hwnd,
        in_elevated_band,
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
        state: WidgetState::default(),
        last_present_error: None,
        last_cross_mismatch: None,
        last_anchor_slot: None,
        last_compact: false,
        last_stacked: false,
        last_env_geom: None,
        pending_uia_scan: false,
        debounce_uia_scan: false,
        invalidate_uia_layout: false,
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
        topmost_recheck_armed: false,
    }
}

/// Dispatches this thread's messages (out-of-context WinEvents arrive
/// through them) until `done` holds or the deadline passes.
pub(super) fn pump_until(timeout: Duration, done: impl Fn() -> bool) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    };
    let deadline = Instant::now() + timeout;
    let mut msg = MSG::default();
    loop {
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            let _ = unsafe { TranslateMessage(&msg) };
            unsafe { DispatchMessageW(&msg) };
        }
        if done() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        unsafe {
            let _ = MsgWaitForMultipleObjects(
                None,
                false,
                remaining.as_millis().min(50) as u32,
                QS_ALLINPUT,
            );
        }
    }
}
