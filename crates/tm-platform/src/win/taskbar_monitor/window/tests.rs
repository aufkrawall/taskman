use super::*;
use crate::win::taskbar_monitor::hooks::taskbar_event_layout_update;
use crate::win::taskbar_monitor::occlusion::{OcclusionCheck, occlusion_result_after_enum};
use crate::win::taskbar_monitor::test_support::{
    DpiContextGuard, TestWindows, create_topmost_test_window, rect, test_window_ctx,
    z_order_test_lock,
};
use crate::win::taskbar_monitor::thread::{
    create_elevated_monitor_window, create_monitor_window, register_window_class,
};
use crate::win::taskbar_monitor::uia::{
    ComMtaGuard, UIA_MIN_SCAN_INTERVAL, UIA_STALE_LAYOUT_MAX_FAILURES, UiaCache, UiaShared,
};
use crate::win::taskbar_monitor::visibility::{HideReason, Visibility};
use crate::win::taskbar_monitor::zorder::{is_window_above, repair_visible_taskbar_order};
use std::sync::Condvar;
use tm_core::format::format_taskbar_pct;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, GWL_EXSTYLE, GetWindowLongPtrW, HWND_NOTOPMOST,
    HWND_TOP, HWND_TOPMOST, IsWindowVisible, OBJID_WINDOW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SetWindowLongPtrW, ShowWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

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
                    stale: false,
                },
                // The cached layout came from scanning this very taskbar.
                last_target: Some((tray.0 as isize, bar)),
                ..Default::default()
            }),
            Condvar::new(),
        )),
        worker: Mutex::new(None),
    });
    let mut ctx = test_window_ctx(hwnd, false);
    ctx.shared = Arc::new(Mutex::new(SharedState {
        config: config.clone(),
        metrics,
        generation: 0,
    }));
    ctx.visibility = Arc::new(MonitorVisibility {
        visible: std::sync::atomic::AtomicBool::new(false),
        notifier: Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }),
    });
    ctx.renderer = Renderer::new();
    ctx.uia_scanner = scanner;
    ctx.last_env_geom = Some((tray.0 as isize, bar, Some(notify), 96));
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(
        ctx.is_visible(),
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
            ctx.is_visible(),
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
        assert!(ctx.is_visible());
        assert!(
            !ctx.accepts_input(),
            "pending occupancy must not steal clicks"
        );
        assert_eq!(ctx.last_pos, previous_pos);
        assert_eq!(ctx.last_size, previous_size);
        assert_eq!(ctx.last_display, previous_display);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
    }
    // The burst costs Explorer one trailing scan, not one per event.
    assert!(
        ctx.uia_scanner.state.0.lock().unwrap().not_before.is_some(),
        "event-driven rechecks must wait for the burst to settle"
    );
    // Values keep updating in place while occupancy is unconfirmed.
    let live = Metrics {
        cpu_pct: Some(42.0),
        ..metrics
    };
    ctx.refresh_taskbar_env(&config, &live, env, false);
    assert!(ctx.is_visible());
    assert!(!ctx.accepts_input());
    assert_eq!(ctx.last_pos, previous_pos);
    assert_eq!(ctx.last_size, previous_size);
    assert_eq!(
        ctx.last_display.as_ref().and_then(|d| d.cpu_val.clone()),
        Some(format_taskbar_pct(Some(42.0))),
        "revalidation must not freeze the displayed values"
    );
    {
        let mut state = ctx.uia_scanner.state.0.lock().unwrap();
        let target = state.pending.take().unwrap();
        assert!(state.publish(target, Some(vec![notify])));
    }
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(ctx.is_visible());
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
    assert!(!ctx.is_visible());
    assert_eq!(notifications.load(Ordering::Relaxed), 2);

    // A hidden widget must not reappear on provisional cached data.
    ctx.request_layout_scan(LayoutUpdate::Recheck, true);
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(!ctx.is_visible());
    {
        let mut state = ctx.uia_scanner.state.0.lock().unwrap();
        let target = state.pending.take().unwrap();
        assert!(state.publish(target, Some(vec![notify])));
    }
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(ctx.is_visible());
    assert!(ctx.accepts_input());
    assert_eq!(notifications.load(Ordering::Relaxed), 3);

    // A transiently failed validation keeps the confirmed footprint in
    // place, click-through, while the worker retries; it never blinks out.
    ctx.request_layout_scan(LayoutUpdate::Recheck, true);
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    let held_pos = ctx.last_pos;
    for _ in 0..UIA_STALE_LAYOUT_MAX_FAILURES {
        {
            let mut state = ctx.uia_scanner.state.0.lock().unwrap();
            let target = state.pending.take().unwrap();
            assert!(!state.publish(target, None));
        }
        ctx.refresh_taskbar_env(&config, &metrics, env, false);
        assert!(ctx.is_visible(), "a transient UIA failure must not hide");
        assert!(
            !ctx.accepts_input(),
            "an unconfirmed layout must not take clicks"
        );
        assert_eq!(ctx.last_pos, held_pos);
        assert_eq!(notifications.load(Ordering::Relaxed), 3);
    }
    // A persistent failure discards cached pixels; rechecks cannot revive them.
    {
        let mut state = ctx.uia_scanner.state.0.lock().unwrap();
        let target = state.pending.take().unwrap();
        assert!(state.publish(target, None));
    }
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(!ctx.is_visible());
    assert!(!ctx.accepts_input());
    assert_eq!(notifications.load(Ordering::Relaxed), 4);

    {
        let mut state = ctx.uia_scanner.state.0.lock().unwrap();
        let target = state.pending.take().unwrap();
        assert!(state.publish(target, Some(vec![notify])));
    }
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(ctx.is_visible());
    assert_eq!(notifications.load(Ordering::Relaxed), 5);

    // A failed hook-time positioning attempt may already have hidden the
    // native popup. A successful refresh must be able to show it again.
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    assert!(ctx.is_visible());
    ctx.show();
    assert!(unsafe { IsWindowVisible(hwnd).as_bool() });
    assert_eq!(notifications.load(Ordering::Relaxed), 5);

    // A root-window notification also invalidates if the queried geometry moved.
    let mut moved = env;
    moved.taskbar_rect.left -= 1;
    moved.taskbar_rect.right -= 1;
    ctx.request_layout_scan(LayoutUpdate::Recheck, true);
    ctx.refresh_taskbar_env(&config, &metrics, moved, false);
    assert!(!ctx.is_visible());
    assert_eq!(notifications.load(Ordering::Relaxed), 6);
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
                    stale: false,
                },
                ..Default::default()
            }),
            Condvar::new(),
        )),
        worker: Mutex::new(None),
    });
    let mut ctx = test_window_ctx(hwnd, true);
    ctx.shared = Arc::new(Mutex::new(SharedState {
        config: config.clone(),
        metrics,
        generation: 0,
    }));
    ctx.renderer = Renderer::new();
    ctx.uia_scanner = scanner;
    ctx.last_env_geom = Some((tray.0 as isize, bar, Some(notify), 96));

    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(ctx.is_visible());
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
    assert!(ctx.is_visible());

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
        !ctx.is_visible() && unsafe { !IsWindowVisible(hwnd).as_bool() },
        "Band-16 widget must hide when a topmost popup overlaps its footprint"
    );

    // Click-through transparent overlay does not occlude the widget.
    let orig_ex = unsafe { GetWindowLongPtrW(popup, GWL_EXSTYLE) };
    unsafe {
        SetWindowLongPtrW(popup, GWL_EXSTYLE, orig_ex | WS_EX_TRANSPARENT.0 as isize);
    }
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(ctx.is_visible());

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
        ctx.is_visible(),
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
    assert!(!ctx.is_visible());

    unsafe {
        let _ = ShowWindow(popup, SW_HIDE);
    }
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(ctx.is_visible());
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

    let ctx = test_window_ctx(hwnd, false);
    assert!(ctx.ensure_visible_order(tray, None));
    assert!(is_window_above(hwnd, tray));
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
                    stale: false,
                },
                ..Default::default()
            }),
            Condvar::new(),
        )),
        worker: Mutex::new(None),
    });
    let mut ctx = test_window_ctx(hwnd, false);
    ctx.shared = Arc::new(Mutex::new(SharedState {
        config: config.clone(),
        metrics,
        generation: 0,
    }));
    ctx.visibility = Arc::new(MonitorVisibility {
        visible: std::sync::atomic::AtomicBool::new(false),
        notifier: Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }),
    });
    ctx.renderer = Renderer::new();
    ctx.uia_scanner = scanner;
    ctx.last_env_geom = Some((tray.0 as isize, bar, Some(notify), 96));

    // Initial confirmed visibility.
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(ctx.is_visible());
    assert!(ctx.accepts_input());
    assert!(ctx.state.layout_confirmed);
    assert_eq!(notifications.load(Ordering::Relaxed), 1);

    // Simulate entering fullscreen: geometry is preserved, background UIA
    // revalidation is marked pending, and the widget hides.
    ctx.pending_uia_scan = true;
    ctx.hide(HideReason::Fullscreen { suppressor: None });
    assert!(!ctx.is_visible());
    assert!(!unsafe { IsWindowVisible(hwnd).as_bool() });
    assert_eq!(notifications.load(Ordering::Relaxed), 2);

    // Simulate Alt+Tab out of fullscreen with updated metrics before the
    // background UIA scan completes: widget must reappear immediately using
    // its cached layout, while passing clicks through (`WS_EX_TRANSPARENT`).
    metrics.cpu_pct = Some(42.0);
    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(
        ctx.is_visible() && unsafe { IsWindowVisible(hwnd).as_bool() },
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
    assert!(ctx.is_visible());
    assert!(ctx.accepts_input());
    assert_eq!(notifications.load(Ordering::Relaxed), 3);

    // If geometry actually changed while in fullscreen (e.g. resolution/DPI
    // change), returning must invalidate stale cache and wait for UIA.
    ctx.pending_uia_scan = true;
    ctx.hide(HideReason::Fullscreen { suppressor: None });
    let mut changed_env = env;
    changed_env.taskbar_rect.top -= 4;
    ctx.refresh_taskbar_env(&config, &metrics, changed_env, false);
    assert!(
        !ctx.is_visible() && unsafe { !IsWindowVisible(hwnd).as_bool() },
        "changed taskbar geometry after fullscreen must wait for fresh UIA scan"
    );
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
        // The mock tray is deliberately NOT topmost. When the test process is
        // not in the foreground, Windows inserts a newly promoted topmost
        // window below the existing topmost windows instead of at the top, so
        // a topmost mock tray would make the promotion below depend on which
        // app has focus. Every topmost window is above every non-topmost one,
        // so the promotion is deterministic against a non-topmost tray.
        SetWindowPos(
            tray,
            Some(HWND_NOTOPMOST),
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
    let tray_raw = tray.0 as isize;

    #[derive(Clone, Copy)]
    enum BgZCmd {
        PromoteTopmost,
        DemoteNoTopmost,
        Exit,
    }
    let (cmd_tx, cmd_rx) = mpsc::channel::<BgZCmd>();
    let (ready_tx, ready_rx) = mpsc::channel::<(u32, isize)>();
    let (ack_tx, ack_rx) = mpsc::channel::<()>();
    let bg_join = std::thread::spawn(move || {
        let _dpi = DpiContextGuard::per_monitor_v2();
        let tray = HWND(tray_raw as *mut std::ffi::c_void);
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
            // Start visible, non-topmost and directly behind `tray` without
            // stealing desktop focus.
            SetWindowPos(
                bg,
                Some(tray),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
            .unwrap();
        }
        ready_tx
            .send((unsafe { GetCurrentThreadId() }, bg.0 as isize))
            .unwrap();
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
                        // Inserting after a non-topmost window also clears
                        // topmost status (HWND_NOTOPMOST alone would land it
                        // above the non-topmost mock tray).
                        SetWindowPos(
                            bg,
                            Some(tray),
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

    let (bg_tid, bg_raw) = ready_rx.recv().unwrap();
    let bg = HWND(bg_raw as *mut std::ffi::c_void);
    assert!(
        !is_window_above(bg, tray),
        "background window must start behind the mock tray"
    );
    let mut ctx = test_window_ctx(hwnd, true);
    ctx.last_size = (widget_rect.width(), widget_rect.height());
    ctx.last_pos = Some((widget_rect.left, widget_rect.top));
    ctx.state.visibility = Visibility::Shown;

    assert!(!ctx.recheck_topmost_occlusion_with_tray(tray));
    assert!(ctx.is_visible());
    assert!(!ctx.topmost_hooks.iter().any(|&(t, _)| t == bg_tid));

    // Promote already-visible background window to TOPMOST with no move/size/activate.
    cmd_tx.send(BgZCmd::PromoteTopmost).unwrap();
    ack_rx.recv().unwrap();
    assert!(
        is_window_above(bg, tray),
        "topmost promotion must order the background window above the mock tray"
    );
    assert!(!ctx.recheck_topmost_occlusion_with_tray(tray));
    assert!(
        !ctx.is_visible() && unsafe { !IsWindowVisible(hwnd).as_bool() },
        "pure Z-order promotion of an overlapping background window must hide elevated widget"
    );
    assert!(ctx.hidden_by_occlusion());
    assert!(ctx.topmost_recheck_armed);
    assert!(ctx.topmost_hooks.iter().any(|&(t, _)| t == bg_tid));

    // Demote it back behind the taskbar with no move/size/activate.
    cmd_tx.send(BgZCmd::DemoteNoTopmost).unwrap();
    ack_rx.recv().unwrap();
    assert!(
        !is_window_above(bg, tray),
        "demotion must order the background window behind the mock tray"
    );
    assert!(
        ctx.recheck_topmost_occlusion_with_tray(tray),
        "demoting the overlapping background window must request refresh to restore"
    );
    assert!(!ctx.topmost_hooks.iter().any(|&(t, _)| t == bg_tid));

    ctx.hide(HideReason::Fullscreen { suppressor: None });
    assert!(!ctx.hidden_by_occlusion());
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

    let mut ctx = test_window_ctx(hwnd, true);
    ctx.last_size = (rect_old.width(), rect_old.height());
    ctx.last_pos = Some((rect_old.left, rect_old.top));

    // Simulate a relocation to `rect_new` blocked by `popup_new` before `last_pos` updates.
    ctx.hide(HideReason::Occluded {
        rect: rect_new,
        occluder: popup_new,
    });
    assert!(ctx.hidden_by_occlusion());
    assert_eq!(ctx.state.occluded_rect(), Some(rect_new));
    assert_eq!(ctx.last_rect(), Some(rect_old));
    assert!(!ctx.recheck_topmost_occlusion_with_tray(tray));

    // Clear the blocked candidate slot while `rect_old` stays occluded by `popup_old`.
    unsafe {
        let _ = ShowWindow(popup_new, SW_HIDE);
    }
    assert!(
        ctx.recheck_topmost_occlusion_with_tray(tray),
        "recheck must inspect the occluded footprint before stale `last_rect()`"
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
    let mut ctx = test_window_ctx(hwnd, false);
    ctx.shared = Arc::new(Mutex::new(SharedState {
        config: config.clone(),
        metrics,
        generation: 0,
    }));
    ctx.uia_scanner = scanner.clone();
    ctx.last_env_geom = Some((tray.0 as isize, bar, None, 96));

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
    assert!(!ctx.is_visible());
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
                    stale: false,
                },
                ..Default::default()
            }),
            Condvar::new(),
        )),
        worker: Mutex::new(None),
    });
    let mut ctx = test_window_ctx(hwnd, false);
    ctx.shared = Arc::new(Mutex::new(SharedState {
        config: config.clone(),
        metrics,
        generation: 0,
    }));
    ctx.visibility = Arc::new(MonitorVisibility {
        visible: std::sync::atomic::AtomicBool::new(false),
        notifier: Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }),
    });
    ctx.renderer = Renderer::new();
    ctx.uia_scanner = scanner.clone();
    ctx.last_env_geom = Some((tray.0 as isize, bar, Some(notify_initial), 96));

    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(ctx.is_visible());
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
        ctx.is_visible() && unsafe { IsWindowVisible(hwnd).as_bool() },
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
    assert!(ctx.is_visible());
    assert!(ctx.accepts_input());
    assert_eq!(notifications.load(Ordering::Relaxed), 1);
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

    let mut ctx = test_window_ctx(hwnd, true);
    ctx.last_size = (120, 40);
    ctx.last_pos = Some((-23_800, -24_000));
    ctx.state.visibility = Visibility::Shown;

    ctx.hide(HideReason::TrayMissing);
    assert!(ctx.hidden_by_tray_missing());
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
                    stale: false,
                },
                ..Default::default()
            }),
            Condvar::new(),
        )),
        worker: Mutex::new(None),
    });
    let mut ctx = test_window_ctx(hwnd, false);
    ctx.shared = Arc::new(Mutex::new(SharedState {
        config: config.clone(),
        metrics,
        generation: 0,
    }));
    ctx.renderer = Renderer::new();
    ctx.uia_scanner = scanner.clone();
    ctx.last_env_geom = Some((tray.0 as isize, small_bar, Some(notify_initial), 96));

    ctx.refresh_taskbar_env(&config, &metrics, env, false);
    assert!(ctx.is_visible());
    let old_size = ctx.last_size;

    // Grow taskbar height from 30 px to 48 px: the first refresh updates
    // `last_env_geom`, invalidates UIA, and returns early while UIA is pending.
    let grown_bar = rect(-10_000, -10_000, -8_080, -9_952);
    let grown_env = TaskbarEnv {
        taskbar_rect: grown_bar,
        ..env
    };
    ctx.refresh_taskbar_env(&config, &metrics, grown_env, true);
    assert!(!ctx.is_visible());

    // Once UIA finishes scanning the grown taskbar and triggers `refresh(false)`,
    // `ctx` must remeasure and rerender for the new 48 px taskbar bounds rather
    // than reusing the stale 30 px layout size.
    {
        let mut state = scanner.state.0.lock().unwrap();
        let target = state.pending.take().unwrap();
        assert!(state.publish(target, Some(vec![notify_initial])));
    }
    ctx.refresh_taskbar_env(&config, &metrics, grown_env, false);
    assert!(ctx.is_visible());
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
fn fit_target_rect_hides_a_box_that_does_not_fill_the_bar() {
    // Pure geometry: no window is needed, only a `WindowCtx` to hold the
    // diagnostic rate limit.
    let mut ctx = test_window_ctx(HWND::default(), false);
    let bar = rect(-10_000, -10_000, -8_800, -9_952);

    // A box that fills the bar's cross axis is placed unchanged, silently.
    let full = rect(-9_400, -10_000, -9_240, -9_952);
    assert_eq!(ctx.fit_target_rect(full, bar), Some(full));
    assert!(ctx.last_cross_mismatch.is_none());

    // The defect itself: a box a few pixels short leaves a strip of taskbar
    // that takes the click, so it is hidden rather than presented short, and
    // the dead band is diagnosed once per rate-limit window.
    let short = rect(-9_400, -10_000, -9_240, -9_968);
    assert_eq!(ctx.fit_target_rect(short, bar), None);
    let first = ctx
        .last_cross_mismatch
        .expect("a box shorter than the bar must be diagnosed");
    assert_eq!(ctx.fit_target_rect(short, bar), None);
    assert_eq!(
        ctx.last_cross_mismatch,
        Some(first),
        "the cross-axis diagnostic must be rate-limited"
    );

    // A box taller than the bar, or one escaping it on the main axis, cannot be
    // shown without leaving the bar: hide instead of clipping it.
    let tall = rect(-9_400, -10_004, -9_240, -9_952);
    assert_eq!(ctx.fit_target_rect(tall, bar), None);
    let overlong = rect(-10_010, -10_000, -8_850, -9_952);
    assert_eq!(ctx.fit_target_rect(overlong, bar), None);

    // Vertical bar: the cross axis is the width, so a narrow box is hidden too.
    let vertical = rect(-9_000, -10_000, -8_920, -9_000);
    let narrow = rect(-8_984, -9_900, -8_920, -9_800);
    assert_eq!(ctx.fit_target_rect(narrow, vertical), None);
    let full_width = rect(-9_000, -9_900, -8_920, -9_800);
    assert_eq!(ctx.fit_target_rect(full_width, vertical), Some(full_width));
}
