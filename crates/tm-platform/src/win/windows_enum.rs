//! Enumerate pids owning visible top-level windows (used for App detection,
//! mirroring Task Manager's "Apps" grouping).

use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Clone, Default)]
pub struct WindowOwners {
    pub visible: HashSet<u32>,
    pub not_responding: HashSet<u32>,
}

/// Cached result refreshed once per sampling tick by the collector.
static CACHE: Mutex<Option<WindowOwners>> = Mutex::new(None);

pub fn window_owners() -> WindowOwners {
    let mut out = WindowOwners::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::EnumWindows(
            Some(enum_cb),
            windows::Win32::Foundation::LPARAM(&mut out as *mut _ as isize),
        );
    }
    *tm_core::sync::lock(&CACHE) = Some(out.clone());
    out
}

/// Peek at the last enumerated owners without touching windows (cheap).
#[allow(dead_code)]
pub fn last_known_owners() -> Vec<u32> {
    tm_core::sync::lock(&CACHE)
        .as_ref()
        .map(|owners| owners.visible.iter().copied().collect())
        .unwrap_or_default()
}

unsafe extern "system" fn enum_cb(
    hwnd: windows::Win32::Foundation::HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, GetWindowThreadProcessId, IsHungAppWindow, IsWindowVisible,
        WS_EX_TOOLWINDOW,
    };

    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return windows::core::BOOL(1);
        }

        // Skip cloaked UWP windows (suspended apps).
        let mut cloaked: u32 = 0;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return windows::core::BOOL(1);
        }

        // Skip tool windows (tooltips, invisible helper windows).
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex_style & (WS_EX_TOOLWINDOW.0 as isize) != 0 {
            return windows::core::BOOL(1);
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return windows::core::BOOL(1);
        }

        // SAFETY: lparam points at our WindowOwners for the duration of
        // EnumWindows. IsHungAppWindow is a local state query and does not
        // send a blocking message into the target process.
        let out = &mut *(lparam.0 as *mut WindowOwners);
        out.visible.insert(pid);
        if IsHungAppWindow(hwnd).as_bool() {
            out.not_responding.insert(pid);
        }
    }
    windows::core::BOOL(1)
}
