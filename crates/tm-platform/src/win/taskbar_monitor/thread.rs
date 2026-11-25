//! The `"tm-taskbar"` thread: window class/creation, `wnd_proc`, the context
//! menu, and the message loop.

use super::hooks::{
    FULLSCREEN_WAKE_PENDING, IN_ELEVATED_BAND, LAYOUT_RECHECK_PENDING, LIVE_HWND, LayoutUpdate,
    REFRESH_PENDING, fg_event_proc, install_own_cloak_hook, post_refresh,
};
use super::placement::context_menu_anchor;
use super::render::{Renderer, read_taskbar_is_light};
use super::uia::UiaScanner;
use super::visibility::WidgetState;
use super::window::{WindowCtx, cursor_inside_window};
use super::zorder::{TASKBAR_MONITOR_BAND, band_exports, window_band};
use super::{
    FULLSCREEN_EXIT_TIMER_ID, MonitorVisibility, PixelRect, SharedState, TOPMOST_RECHECK_TIMER_ID,
    UIA_SETTLE_TIMER_ID, WM_APP_REFRESH,
};
use crate::taskbar_monitor::Action;
use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex, OnceLock};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, ChangeWindowMessageFilterEx, CreatePopupMenu, CreateWindowExW,
    DeregisterShellHookWindow, DestroyMenu, DestroyWindow, DispatchMessageW, EVENT_OBJECT_CLOAKED,
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MENUPOPUPEND, EVENT_SYSTEM_MENUPOPUPSTART, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, FindWindowW, GWLP_USERDATA, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, GetWindowRect, HMENU, HSHELL_WINDOWACTIVATED,
    HSHELL_WINDOWCREATED, HSHELL_WINDOWDESTROYED, HSHELL_WINDOWREPLACED, IDC_ARROW, InsertMenuW,
    KillTimer, LoadCursorW, MA_NOACTIVATE, MF_BYPOSITION, MF_SEPARATOR, MF_STRING, MSG,
    MSGFLT_ALLOW, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterShellHookWindow,
    RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem, SetWindowLongPtrW,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_CLOSE, WM_CREATE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED,
    WM_DWMCOLORIZATIONCOLORCHANGED, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NULL,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

pub(super) const WM_MOUSELEAVE: u32 = 0x02A3;

// `HSHELL_HIGHBIT | HSHELL_WINDOWACTIVATED`: Windows Shell notification fired
// when a fullscreen / "rude" app enters or leaves fullscreen focus.
pub(super) const HSHELL_RUDEAPPACTIVATED_MSG: u32 = 0x8004;

pub(super) fn set_popup_menu_theme(dark: bool) {
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

pub(super) fn register_window_class(hinstance: HINSTANCE) -> Option<PCWSTR> {
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

pub(super) fn create_monitor_window(
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

pub(super) fn create_elevated_monitor_window(
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

pub(super) struct WndProcState {
    pub(super) msg_taskbar_created: u32,
    pub(super) msg_shell_hook: u32,
    pub(super) ctx: RefCell<WindowCtx>,
    pub(super) menu_open: Cell<bool>,
    pub(super) reentrant_refresh: Cell<bool>,
    pub(super) reentrant_force_render: Cell<bool>,
    pub(super) reentrant_layout_update: Cell<Option<LayoutUpdate>>,
    pub(super) reentrant_theme_dirty: Cell<bool>,
    pub(super) reentrant_close: Cell<bool>,
}

pub(super) struct MenuOpenGuard<'a>(pub(super) &'a Cell<bool>);

impl Drop for MenuOpenGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl WndProcState {
    pub(super) fn new(msg_taskbar_created: u32, msg_shell_hook: u32, ctx: WindowCtx) -> Self {
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

    pub(super) fn queue_reentrant_layout(&self, update: LayoutUpdate) {
        let next = match (self.reentrant_layout_update.get(), update) {
            (Some(LayoutUpdate::Invalidate), _) | (_, LayoutUpdate::Invalidate) => {
                LayoutUpdate::Invalidate
            }
            _ => LayoutUpdate::Recheck,
        };
        self.reentrant_layout_update.set(Some(next));
    }

    pub(super) fn with_ctx_mut(&self, hwnd: HWND, f: impl FnOnce(&mut WindowCtx)) -> bool {
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

    pub(super) fn with_ctx_mut_or(
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

pub(super) fn show_context_menu(state: &WndProcState, hwnd: HWND) {
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

pub(super) unsafe extern "system" fn wnd_proc(
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

pub(super) fn taskbar_thread_main(
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
                state: WidgetState::default(),
                last_present_error: None,
                last_anchor_slot: None,
                last_compact: false,
                last_stacked: false,
                last_env_geom: None,
                pending_uia_scan: true,
                debounce_uia_scan: false,
                invalidate_uia_layout: true,
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
            // Same order as the shutdown below: the state must be unreachable
            // from the window before it can be freed.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if let Err(error) = DestroyWindow(hwnd) {
                    tracing::warn!(
                        hwnd = hwnd.0 as isize,
                        %error,
                        "cannot destroy the monitor window"
                    );
                }
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
        let own_cloak_hook = install_own_cloak_hook();

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
            own_cloak_hook,
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
        // Detach the state before the window can outlive it: `DestroyWindow`
        // can fail, and a window still holding the pointer would dispatch its
        // next message straight into freed memory (or into whatever window
        // reused the handle). `wnd_proc` handles `WM_DESTROY` before it reads
        // the pointer and sends every other message with a null one to
        // `DefWindowProcW`, so clearing it first is the safe order.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
        if unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)).as_bool() } {
            unsafe {
                if let Err(error) = DestroyWindow(hwnd) {
                    tracing::warn!(
                        hwnd = hwnd.0 as isize,
                        %error,
                        "cannot destroy the monitor window"
                    );
                }
            }
        }
        unsafe {
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
    use crate::win::taskbar_monitor::test_support::{
        TestWindows, test_window_ctx, z_order_test_lock,
    };
    use crate::win::taskbar_monitor::zorder::ensure_just_above_taskbar;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindow, IsWindowVisible, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
    };

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

        let state_raw = Box::into_raw(Box::new(WndProcState::new(0, 0, {
            let mut ctx = test_window_ctx(HWND::default(), false);
            ctx.visibility = Arc::new(MonitorVisibility {
                visible: AtomicBool::new(false),
                notifier: Arc::new(|| {}),
            });
            ctx.callback = Arc::new(move |_| {
                let raw = state_slot_cb.load(Ordering::Acquire) as *const WndProcState;
                if !raw.is_null() {
                    let st = unsafe { &*raw };
                    callback_flag.store(st.ctx.try_borrow_mut().is_ok(), Ordering::Release);
                }
            });
            ctx
        })));
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
            assert!(ctx.show());
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
}
