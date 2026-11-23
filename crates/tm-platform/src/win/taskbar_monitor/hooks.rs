//! WinEvent hooks: the process-global wake state they share with the
//! widget thread, their callbacks, and `WindowCtx` hook synchronization.

use super::WM_APP_REFRESH;
#[cfg(test)]
use super::occlusion::collect_topmost_occluder_tids;
use super::occlusion::is_topmost_location_candidate;
use super::window::WindowCtx;
use super::zorder::repair_live_taskbar_order;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, GWL_STYLE,
    GetForegroundWindow, GetWindowLongPtrW, GetWindowThreadProcessId, IsChild, IsWindowVisible,
    OBJID_WINDOW, PostMessageW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WS_CHILD,
};

/// Global HWNDs and coalescing flags so WinEvent hooks (which carry no
/// user-data pointer) can wake `"tm-taskbar"` immediately without flooding its
/// message queue.
pub(super) static LIVE_HWND: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
pub(super) static LIVE_FG_HWND: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
pub(super) static LIVE_FS_HWND: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
pub(super) static LIVE_TRAY_HWND: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
pub(super) static LIVE_OCCLUDER_HWND: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
pub(super) static IN_ELEVATED_BAND: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(super) static REFRESH_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(super) static LAYOUT_RECHECK_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(super) static FULLSCREEN_WAKE_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LayoutUpdate {
    Recheck,
    Invalidate,
}

pub(super) fn taskbar_event_layout_update(
    event: u32,
    id_object: i32,
    id_child: i32,
) -> Option<LayoutUpdate> {
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

pub(super) fn post_refresh() {
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

impl WindowCtx {
    pub(super) fn unhook_dynamic_hooks(&mut self) {
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

    pub(super) fn sync_fullscreen_hook(&mut self, fs_hwnd: HWND) {
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
            self.fs_lifecycle_hook = install_fs_hook(EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE);
            self.fs_cloak_hook = install_fs_hook(EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED);
        }
        self.sync_fullscreen_location_hook();
    }

    /// `EVENT_OBJECT_LOCATIONCHANGE` also fires for every cursor/caret move on
    /// the hooked thread, and each delivery wakes this thread. When the
    /// fullscreen suppressor runs on the foreground thread (the usual game
    /// case) the foreground hook already covers it (`fg_location_event_proc`
    /// accepts `LIVE_FS_HWND`), so a second hook would only double every
    /// cursor-move wakeup. Install the dedicated one only when the threads
    /// differ (e.g. after Alt+Tab away from a still-settling fullscreen app).
    pub(super) fn sync_fullscreen_location_hook(&mut self) {
        let covered_by_fg_hook =
            self.tracked_fs_tid == self.tracked_fg_tid && !self.fg_loc_hook.is_invalid();
        let wanted = self.tracked_fs_tid != 0 && !covered_by_fg_hook;
        if wanted && self.fs_loc_hook.is_invalid() {
            self.fs_loc_hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    None,
                    Some(fs_location_event_proc),
                    0,
                    self.tracked_fs_tid,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
        } else if !wanted && !self.fs_loc_hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.fs_loc_hook);
            }
            self.fs_loc_hook = HWINEVENTHOOK::default();
        }
    }

    pub(super) fn sync_foreground_hook(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
        LIVE_FG_HWND.store(fg.0 as isize, std::sync::atomic::Ordering::Release);
        let mut fg_pid = 0u32;
        let tid = if fg.0.is_null() {
            0
        } else {
            unsafe { GetWindowThreadProcessId(fg, Some(&mut fg_pid)) }
        };
        let self_tid = unsafe { GetCurrentThreadId() };
        let self_pid = unsafe { GetCurrentProcessId() };
        let target_tid = foreground_hook_target_tid(tid, fg_pid, self_tid, self_pid);
        if target_tid == self.tracked_fg_tid {
            return;
        }
        let previous_hook = std::mem::take(&mut self.fg_loc_hook);
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
        // Hand location coverage of a still-tracked fullscreen thread over
        // before dropping the old hook, so no move of it is missed.
        self.sync_fullscreen_location_hook();
        if !previous_hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(previous_hook);
            }
        }
    }

    pub(super) fn sync_tray_hook(&mut self, tray_hwnd: HWND) {
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
    pub(super) fn sync_topmost_hooks(&mut self, tray_hwnd: HWND) {
        let target_tids = if self.in_elevated_band && !tray_hwnd.0.is_null() {
            let excluded = self.excluded_topmost_tids();
            collect_topmost_occluder_tids(tray_hwnd, self.hwnd, &excluded)
        } else {
            Vec::new()
        };
        self.sync_topmost_hooks_with_tids(&target_tids);
    }

    pub(super) fn sync_topmost_hooks_with_tids(&mut self, target_tids: &[u32]) {
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
}

pub(super) fn foreground_hook_target_tid(
    fg_tid: u32,
    fg_pid: u32,
    self_tid: u32,
    self_pid: u32,
) -> u32 {
    if fg_tid != 0 && fg_tid != self_tid && fg_pid != self_pid {
        fg_tid
    } else {
        0
    }
}

pub(super) unsafe extern "system" fn fg_event_proc(
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

pub(super) unsafe extern "system" fn fs_location_event_proc(
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

pub(super) unsafe extern "system" fn fg_location_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    // OBJID_WINDOW (0) + CHILDID_SELF (0): ignore carets, scrollbars, and child DOM controls.
    // This single per-thread hook also covers the fullscreen suppressor when it
    // runs on the foreground thread (see `sync_fullscreen_location_hook`).
    if id_object == OBJID_WINDOW.0
        && id_child == 0
        && !hwnd.0.is_null()
        && (hwnd.0 as isize == LIVE_FG_HWND.load(std::sync::atomic::Ordering::Acquire)
            || hwnd.0 as isize == LIVE_FS_HWND.load(std::sync::atomic::Ordering::Acquire)
            || (IN_ELEVATED_BAND.load(std::sync::atomic::Ordering::Acquire)
                && is_topmost_location_candidate(hwnd, id_object, id_child)))
    {
        post_refresh();
    }
}

pub(super) unsafe extern "system" fn topmost_location_event_proc(
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

/// Last `DWMWA_CLOAKED` value reported for the widget by
/// `own_cloak_event_proc` (0 = uncloaked). Diagnostics only.
pub(super) static OWN_CLOAK_STATE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Diagnostics for disappearances that are not `hide()` calls: the system
/// hooks skip our own process, so a DWM/shell cloak of the widget itself (e.g.
/// during a virtual-desktop switch animation) was invisible in the logs. This
/// thread-scoped hook on the widget's own thread records and logs it. The
/// value is a `DWM_CLOAKED_*` mask: 1 = app, 2 = shell, 4 = inherited.
pub(super) unsafe extern "system" fn own_cloak_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if id_object != OBJID_WINDOW.0
        || id_child != 0
        || hwnd.0.is_null()
        || hwnd.0 as isize != LIVE_HWND.load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    let mut cloaked: u32 = 0;
    let _ = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    OWN_CLOAK_STATE.store(cloaked, std::sync::atomic::Ordering::Release);
    tracing::debug!(
        event = if event == EVENT_OBJECT_CLOAKED {
            "cloaked"
        } else {
            "uncloaked"
        },
        cloaked_mask = cloaked,
        window_visible = unsafe { IsWindowVisible(hwnd).as_bool() },
        "taskbar monitor DWM cloak state changed (not a hide)"
    );
}

pub(super) fn install_own_cloak_hook() -> HWINEVENTHOOK {
    unsafe {
        SetWinEventHook(
            EVENT_OBJECT_CLOAKED,
            EVENT_OBJECT_UNCLOAKED,
            None,
            Some(own_cloak_event_proc),
            0,
            GetCurrentThreadId(),
            WINEVENT_OUTOFCONTEXT,
        )
    }
}

pub(super) unsafe extern "system" fn tray_event_proc(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::taskbar_monitor::identity::visible_frame_rect;
    use crate::win::taskbar_monitor::occlusion::{
        is_potential_occluder_window, is_topmost_or_elevated_window,
    };
    use crate::win::taskbar_monitor::test_support::{
        TestWindows, pump_until, rect, test_window_ctx, z_order_test_lock,
    };
    use crate::win::taskbar_monitor::thread::{
        create_elevated_monitor_window, create_monitor_window, register_window_class,
    };
    use crate::win::taskbar_monitor::uia::ComMtaGuard;
    use crate::win::taskbar_monitor::zorder::{is_window_above, window_band};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, GWL_EXSTYLE, GetDesktopWindow,
        GetShellWindow, HWND_TOPMOST, MSG, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, ShowWindow, TranslateMessage, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

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

        let mut ctx = test_window_ctx(hwnd, true);

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
    fn fullscreen_hook_tracks_suppressing_window_moves_after_foreground_leaves() {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;

        let _zorder = z_order_test_lock();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![hwnd]);

        enum FsCmd {
            Move(i32),
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
                    FsCmd::Move(width) => {
                        unsafe {
                            SetWindowPos(
                                fs_hwnd,
                                None,
                                -22_000,
                                -22_000,
                                width,
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

        let mut ctx = test_window_ctx(hwnd, true);

        ctx.sync_fullscreen_hook(fs_hwnd);
        assert_eq!(ctx.tracked_fs_tid, fs_tid);
        assert!(!ctx.fs_loc_hook.is_invalid());

        let mut woke = false;
        for width in [800, 807, 814] {
            REFRESH_PENDING.store(false, Ordering::Release);
            cmd_tx.send(FsCmd::Move(width)).unwrap();
            ack_rx.recv().unwrap();
            if pump_until(Duration::from_secs(1), || {
                REFRESH_PENDING.load(Ordering::Acquire)
            }) {
                woke = true;
                break;
            }
        }
        assert!(
            woke,
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

        let mut ctx = test_window_ctx(hwnd, true);

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
    fn layout_scan_requests_debounce_only_event_driven_rechecks() {
        let mut ctx = test_window_ctx(HWND::default(), false);
        ctx.request_layout_scan(LayoutUpdate::Recheck, true);
        assert!(ctx.pending_uia_scan && ctx.debounce_uia_scan);
        // The settle-timer path and invalidations scan immediately; a later
        // event-driven recheck must not demote an already immediate request.
        ctx.request_layout_scan(LayoutUpdate::Recheck, false);
        assert!(ctx.pending_uia_scan && !ctx.debounce_uia_scan);
        ctx.request_layout_scan(LayoutUpdate::Recheck, true);
        assert!(!ctx.debounce_uia_scan);
        ctx.pending_uia_scan = false;
        ctx.request_layout_scan(LayoutUpdate::Recheck, true);
        ctx.request_layout_scan(LayoutUpdate::Invalidate, false);
        assert!(ctx.pending_uia_scan && !ctx.debounce_uia_scan && ctx.invalidate_uia_layout);
    }

    #[test]
    fn fullscreen_location_hook_reuses_the_foreground_hook_on_the_same_thread() {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;

        let _zorder = z_order_test_lock();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![hwnd]);

        // `Some(width)` resizes the suppressor, `None` destroys it.
        let (cmd_tx, cmd_rx) = mpsc::channel::<Option<i32>>();
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
            ready_tx
                .send((fs_hwnd.0 as isize, unsafe { GetCurrentThreadId() }))
                .unwrap();
            while let Ok(Some(width)) = cmd_rx.recv() {
                unsafe {
                    SetWindowPos(
                        fs_hwnd,
                        None,
                        -22_000,
                        -22_000,
                        width,
                        600,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    )
                    .unwrap();
                }
                ack_tx.send(()).unwrap();
            }
            unsafe {
                let _ = DestroyWindow(fs_hwnd);
            }
        });
        let (fs_raw, fs_tid) = ready_rx.recv().expect("fullscreen thread ready");
        let fs_hwnd = HWND(fs_raw as *mut std::ffi::c_void);
        LIVE_HWND.store(hwnd.0 as isize, Ordering::Release);
        LIVE_FG_HWND.store(0, Ordering::Release);

        let install_fg_hook = |tid: u32| unsafe {
            SetWinEventHook(
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_OBJECT_LOCATIONCHANGE,
                None,
                Some(fg_location_event_proc),
                0,
                tid,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        let resize_wakes_refresh = |width: i32| {
            for offset in [0, 7, 14] {
                REFRESH_PENDING.store(false, Ordering::Release);
                cmd_tx.send(Some(width + offset)).unwrap();
                ack_rx.recv().unwrap();
                if pump_until(Duration::from_secs(1), || {
                    REFRESH_PENDING.load(Ordering::Acquire)
                }) {
                    return true;
                }
            }
            false
        };

        let mut ctx = test_window_ctx(hwnd, true);
        // The usual game case: the suppressor is the foreground window, so the
        // foreground hook already listens on its thread.
        ctx.tracked_fg_tid = fs_tid;
        ctx.fg_loc_hook = install_fg_hook(fs_tid);
        assert!(!ctx.fg_loc_hook.is_invalid());
        ctx.sync_fullscreen_hook(fs_hwnd);
        assert_eq!(ctx.tracked_fs_tid, fs_tid);
        assert!(
            ctx.fs_loc_hook.is_invalid(),
            "a second LOCATIONCHANGE hook on the same thread doubles every cursor-move wakeup"
        );
        assert!(!ctx.fs_lifecycle_hook.is_invalid());
        assert!(!ctx.fs_cloak_hook.is_invalid());
        assert!(
            resize_wakes_refresh(800),
            "the shared foreground hook must still wake on suppressor moves"
        );

        // Foreground leaves the suppressor's thread: the dedicated hook takes over.
        unsafe {
            let _ = UnhookWinEvent(std::mem::take(&mut ctx.fg_loc_hook));
        }
        ctx.tracked_fg_tid = 0;
        ctx.sync_fullscreen_location_hook();
        assert!(!ctx.fs_loc_hook.is_invalid());
        assert!(
            resize_wakes_refresh(1024),
            "suppressor moves must wake refresh after foreground left its thread"
        );

        // Foreground returns to it: the duplicate is dropped again.
        ctx.tracked_fg_tid = fs_tid;
        ctx.fg_loc_hook = install_fg_hook(fs_tid);
        ctx.sync_fullscreen_location_hook();
        assert!(ctx.fs_loc_hook.is_invalid());

        ctx.unhook_dynamic_hooks();
        LIVE_HWND.store(0, Ordering::Release);
        REFRESH_PENDING.store(false, Ordering::Release);
        let _ = cmd_tx.send(None);
        let _ = fs_join.join();
    }

    #[test]
    fn own_cloak_hook_reports_widget_cloak_changes() {
        use std::sync::atomic::Ordering;
        use windows::Win32::Graphics::Dwm::DWMWA_CLOAK;

        let _zorder = z_order_test_lock();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let hwnd = create_monitor_window(hinstance, class_name, std::ptr::null_mut()).unwrap();
        let _windows = TestWindows(vec![hwnd]);
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                -22_000,
                -22_000,
                100,
                40,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        LIVE_HWND.store(hwnd.0 as isize, Ordering::Release);
        OWN_CLOAK_STATE.store(0, Ordering::Release);
        let hook = install_own_cloak_hook();
        assert!(!hook.is_invalid());

        let set_cloak = |cloak: bool| {
            let value = windows::core::BOOL::from(cloak);
            unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_CLOAK,
                    (&value as *const windows::core::BOOL).cast(),
                    std::mem::size_of::<windows::core::BOOL>() as u32,
                )
            }
        };
        let cloaked = set_cloak(true).is_ok()
            && pump_until(Duration::from_secs(2), || {
                OWN_CLOAK_STATE.load(Ordering::Acquire) != 0
            });
        let uncloaked = set_cloak(false).is_ok()
            && pump_until(Duration::from_secs(2), || {
                OWN_CLOAK_STATE.load(Ordering::Acquire) == 0
            });
        unsafe {
            let _ = UnhookWinEvent(hook);
        }
        LIVE_HWND.store(0, Ordering::Release);
        assert!(cloaked, "a DWM cloak of the widget itself must be observed");
        assert!(uncloaked, "the matching uncloak must be observed");
    }

    #[test]
    fn foreground_hook_skips_own_process_and_own_thread() {
        assert_eq!(foreground_hook_target_tid(0, 100, 10, 200), 0);
        assert_eq!(foreground_hook_target_tid(10, 100, 10, 200), 0);
        assert_eq!(
            foreground_hook_target_tid(42, 200, 10, 200),
            0,
            "own-process foreground window (UI thread) must not receive LOCATIONCHANGE hook"
        );
        assert_eq!(foreground_hook_target_tid(42, 100, 10, 200), 42);
    }
}
