//! Taskbar environment probe and fullscreen / auto-hide suppression.

use super::PixelRect;
use super::identity::{
    is_desktop_shell_class, is_non_occluding_shell_host_window, is_transparent_overlay,
    is_window_cloaked,
};
#[cfg(test)]
use super::occlusion::scan_above_tray_in_band;
use super::occlusion::{PrecheckedOcclusion, scan_above_tray};
use super::zorder::is_window_above;
use std::sync::Arc;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    HMONITOR, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{
    ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    SHAppBarMessage, SHQueryUserNotificationState,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GWL_STYLE, GetDesktopWindow, GetForegroundWindow, GetShellWindow,
    GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindowVisible, WS_CAPTION, WS_MAXIMIZE,
};
use windows::core::w;

#[derive(Clone, Copy)]
pub(super) struct TaskbarEnv {
    pub(super) tray_hwnd: HWND,
    pub(super) taskbar_rect: PixelRect,
    pub(super) tray_notify_rect: Option<PixelRect>,
    pub(super) dpi: u32,
}

pub(super) enum TaskbarEnvProbe {
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

pub(super) fn probe_taskbar_env(
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

pub(super) fn is_taskbar_autohidden(taskbar_rect: PixelRect, monitor_rect: PixelRect) -> bool {
    // Geometry first: `ABM_GETSTATE` is a synchronous cross-process message to
    // Explorer (~25 µs, and it waits on a hung Explorer). Only a taskbar that
    // actually looks slid away needs the auto-hide confirmation.
    if !taskbar_slid_offscreen(taskbar_rect, monitor_rect) {
        return false;
    }
    let mut abd = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    let state = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut abd) };
    (state as u32 & ABS_AUTOHIDE) != 0
}

pub(super) fn taskbar_slid_offscreen(taskbar_rect: PixelRect, monitor_rect: PixelRect) -> bool {
    // When an auto-hide taskbar collapses, Windows leaves only a ~2 px sliver
    // inside the monitor edge.
    let vis_left = taskbar_rect.left.max(monitor_rect.left);
    let vis_top = taskbar_rect.top.max(monitor_rect.top);
    let vis_right = taskbar_rect.right.min(monitor_rect.right);
    let vis_bottom = taskbar_rect.bottom.min(monitor_rect.bottom);
    (vis_right - vis_left) <= 8 || (vis_bottom - vis_top) <= 8
}

pub(super) unsafe fn fullscreen_covering_monitor_state(
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
    let fg_style = unsafe { GetWindowLongPtrW(fg, GWL_STYLE) } as u32;
    let fg_covers = !is_captioned_maximized_style(fg_style) && {
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
    // The Z-order test runs first: it stops at the tray and is false for every
    // ordinary foreground window, which skips the costlier shell query.
    if is_window_above(fg, tray_hwnd)
        && let Ok(state) = unsafe { SHQueryUserNotificationState() }
        && (state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE)
    {
        return (true, false, Some(fg));
    }
    (false, false, None)
}

/// A maximized window with a standard caption is not a fullscreen app, even
/// when its frame covers the whole monitor: with an auto-hide taskbar the work
/// area is the full monitor, so every maximized window's (border-inflated)
/// rect covers it. Windows keeps revealing the taskbar over such windows, so
/// the widget must not treat them as suppressors either.
pub(super) fn is_captioned_maximized_style(style: u32) -> bool {
    (style & WS_MAXIMIZE.0) != 0 && (style & WS_CAPTION.0) == WS_CAPTION.0
}

pub(super) fn fullscreen_above_result(
    saw_tray: bool,
    tray_band: Option<u32>,
    covered: bool,
) -> bool {
    // Windows 11 promotes `Shell_TrayWnd` into band 6 (`ZBID_IMMERSIVE_MOGO`)
    // when Start/Search opens, and `EnumWindows` omits immersive bands from
    // non-immersive callers; windows in bands above `tray_band` still cover it.
    covered && (saw_tray || tray_band.is_some_and(|band| band > 1))
}

#[cfg(test)]
pub(super) fn fullscreen_window_above_taskbar_in_band(
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
pub(super) fn is_fullscreen_window_above_taskbar_in_band(
    monitor_rect: PixelRect,
    tray_hwnd: HWND,
    own_hwnd: HWND,
    tray_band: Option<u32>,
) -> bool {
    fullscreen_window_above_taskbar_in_band(monitor_rect, tray_hwnd, own_hwnd, tray_band).is_some()
}

#[cfg(test)]
pub(super) fn is_fullscreen_window_above_taskbar(
    monitor_rect: PixelRect,
    tray_hwnd: HWND,
    own_hwnd: HWND,
) -> bool {
    scan_above_tray(tray_hwnd, own_hwnd, Some(monitor_rect), None, None)
        .fullscreen_coverer
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::taskbar_monitor::test_support::{rect, z_order_test_lock};
    use crate::win::taskbar_monitor::thread::register_window_class;
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GWL_EXSTYLE, HWND_TOPMOST, SW_HIDE, SW_SHOWNOACTIVATE,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
        WS_POPUP,
    };
    use windows::core::BOOL;

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

    #[test]
    fn maximized_captioned_windows_are_not_fullscreen_suppressors() {
        use windows::Win32::UI::WindowsAndMessaging::{WS_BORDER, WS_OVERLAPPEDWINDOW};
        assert!(is_captioned_maximized_style(
            (WS_OVERLAPPEDWINDOW | WS_MAXIMIZE).0
        ));
        assert!(!is_captioned_maximized_style(WS_OVERLAPPEDWINDOW.0));
        assert!(
            !is_captioned_maximized_style((WS_POPUP | WS_MAXIMIZE).0),
            "borderless fullscreen apps may be maximized popups"
        );
        assert!(
            !is_captioned_maximized_style((WS_POPUP | WS_BORDER | WS_MAXIMIZE).0),
            "a border alone is not a caption"
        );
        // Why the rect test alone is wrong: with an auto-hide taskbar the work
        // area is the monitor, so a maximized frame (inflated by its borders)
        // covers the whole monitor although the taskbar still reveals over it.
        let monitor = rect(0, 0, 1920, 1080);
        assert!(rect(-8, -8, 1928, 1088).covers(monitor));
    }
}
