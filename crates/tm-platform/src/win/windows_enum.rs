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

const UWP_FRAME_CLASS: &str = "ApplicationFrameWindow";
const UWP_CORE_CLASS: &str = "Windows.UI.Core.CoreWindow";

unsafe fn window_class(hwnd: windows::Win32::Foundation::HWND) -> String {
    let mut buffer = [0u16; 256];
    let len = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut buffer) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..len as usize])
}

/// PID owning the hosted UWP `CoreWindow` inside an `ApplicationFrameWindow`.
///
/// The frame window belongs to `ApplicationFrameHost.exe`, a shared broker.
/// Attributing it to that process would make every Store app look like
/// "Application Frame Host" and push the real app into Background; native
/// Task Manager names the app the user sees.
unsafe fn hosted_app_pid(frame: windows::Win32::Foundation::HWND) -> Option<u32> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetWindowThreadProcessId};

    unsafe extern "system" fn child_cb(
        child: windows::Win32::Foundation::HWND,
        lparam: LPARAM,
    ) -> windows::core::BOOL {
        unsafe {
            if window_class(child).eq_ignore_ascii_case(UWP_CORE_CLASS) {
                let mut pid = 0u32;
                GetWindowThreadProcessId(child, Some(&mut pid));
                if pid != 0 {
                    *(lparam.0 as *mut u32) = pid;
                    return windows::core::BOOL(0);
                }
            }
        }
        windows::core::BOOL(1)
    }

    let mut pid = 0u32;
    let _ = unsafe {
        EnumChildWindows(
            Some(frame),
            Some(child_cb),
            LPARAM(&mut pid as *mut u32 as isize),
        )
    };
    (pid != 0).then_some(pid)
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

        let class = window_class(hwnd);
        let is_uwp = class.eq_ignore_ascii_case(UWP_FRAME_CLASS)
            || class.eq_ignore_ascii_case(UWP_CORE_CLASS);

        // Cloaked windows are hidden by DWM: suspended UWP apps and windows
        // on other virtual desktops. Native Task Manager keeps a suspended
        // UWP app in Apps with status "Suspended", so those count as App
        // windows; every other cloaked window stays excluded.
        let mut cloaked: u32 = 0;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
            && !is_uwp
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
        // Attribute a UWP frame to the app it hosts, not to the broker. A
        // cloaked frame with no hosted CoreWindow is the leftover frame of a
        // suspended app; that app owns its CoreWindow as a SEPARATE top-level
        // window, so counting the ghost frame would add a bogus
        // "Application Frame Host" App row.
        if class.eq_ignore_ascii_case(UWP_FRAME_CLASS) {
            match hosted_app_pid(hwnd) {
                Some(hosted) => pid = hosted,
                None if cloaked != 0 => return windows::core::BOOL(1),
                None => {}
            }
        }
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
