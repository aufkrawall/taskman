//! Window bands (`CreateWindowInBand` / `GetWindowBand`) and band-1 Z-order
//! repair relative to `Shell_TrayWnd`.

use super::hooks::{IN_ELEVATED_BAND, LIVE_HWND, LIVE_TRAY_HWND};
use super::identity::is_window_cloaked;
use std::sync::OnceLock;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_HWNDPREV, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, HMENU, HWND_NOTOPMOST,
    HWND_TOP, HWND_TOPMOST, IsWindowVisible, SW_HIDE, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOOWNERZORDER, SWP_NOSENDCHANGING, SWP_NOSIZE, SetWindowPos, ShowWindow, WS_EX_TOPMOST,
};
use windows::core::{BOOL, PCWSTR, s, w};

// Native Task Manager's band (`ZBID_SYSTEM_TOOLS`), strictly above
// `Shell_TrayWnd` in band 1 (`ZBID_DESKTOP`) without Explorer ownership.
pub(super) const TASKBAR_MONITOR_BAND: u32 = 16;

pub(super) struct ZOrderScan {
    pub(super) target: HWND,
    pub(super) anchor: HWND,
    pub(super) saw_target: bool,
    pub(super) saw_anchor: bool,
}

pub(super) unsafe extern "system" fn z_order_enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
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

pub(super) fn is_window_above(target: HWND, anchor: HWND) -> bool {
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

pub(super) struct TaskbarZOrder {
    pub(super) tray: HWND,
    pub(super) widget: HWND,
    pub(super) topmost: bool,
    pub(super) tray_band: Option<u32>,
    pub(super) predecessor: Option<HWND>,
    pub(super) saw_tray: bool,
    pub(super) saw_widget: bool,
    pub(super) visible_after_widget: bool,
}

pub(super) unsafe extern "system" fn taskbar_z_order_enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
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

pub(super) fn hide_and_demote_widget(hwnd: HWND) {
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
pub(super) fn ensure_just_above_taskbar(widget_hwnd: HWND, tray_hwnd: HWND) -> bool {
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
pub(super) fn repair_visible_taskbar_order(widget_hwnd: HWND, tray_hwnd: HWND) {
    if !widget_hwnd.0.is_null()
        && !tray_hwnd.0.is_null()
        && unsafe { IsWindowVisible(widget_hwnd).as_bool() && IsWindowVisible(tray_hwnd).as_bool() }
    {
        ensure_just_above_taskbar(widget_hwnd, tray_hwnd);
    }
}

pub(super) fn repair_live_taskbar_order() {
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

pub(super) type CreateWindowInBandFn = unsafe extern "system" fn(
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
pub(super) type GetWindowBandFn = unsafe extern "system" fn(HWND, *mut u32) -> BOOL;

#[derive(Clone, Copy)]
pub(super) struct BandExports {
    pub(super) create_in_band: Option<CreateWindowInBandFn>,
    pub(super) get_band: Option<GetWindowBandFn>,
}

pub(super) fn band_exports() -> BandExports {
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

pub(super) fn window_band(hwnd: HWND) -> Option<u32> {
    let get_band = band_exports().get_band?;
    let mut band = 0u32;
    let ok = unsafe { get_band(hwnd, &mut band) };
    ok.as_bool().then_some(band)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::taskbar_monitor::test_support::{
        DpiContextGuard, TestWindows, create_topmost_test_window, rect, z_order_test_lock,
    };
    use crate::win::taskbar_monitor::thread::{
        create_elevated_monitor_window, create_monitor_window, register_window_class,
    };
    use crate::win::taskbar_monitor::uia::ComMtaGuard;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, SW_SHOWNOACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_POPUP,
    };

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
}
