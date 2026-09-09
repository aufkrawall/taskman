//! Enumerate pids owning visible top-level windows (used for App detection,
//! mirroring Task Manager's "Apps" grouping).

use std::collections::HashSet;
use std::sync::Mutex;

use windows::Win32::Foundation::HWND;

#[derive(Clone, Default)]
pub struct WindowOwners {
    pub visible: HashSet<u32>,
    pub not_responding: HashSet<u32>,
}

/// Scratch state of one `EnumWindows` pass: the owners found so far plus the
/// two halves of a minimized Store app, which only pair up once every window
/// has been seen (see [`pair_minimized_views`]).
#[derive(Default)]
struct Scan {
    owners: WindowOwners,
    detached_frames: Vec<HWND>,
    hidden_cores: Vec<(u32, HWND)>,
}

/// Cached result refreshed once per sampling tick by the collector.
static CACHE: Mutex<Option<WindowOwners>> = Mutex::new(None);

pub fn window_owners() -> WindowOwners {
    let mut scan = Scan::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::EnumWindows(
            Some(enum_cb),
            windows::Win32::Foundation::LPARAM(&mut scan as *mut _ as isize),
        );
    }
    pair_minimized_views(&mut scan);
    let out = scan.owners;
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

/// What one enumerated top-level window contributes to the App grouping.
/// Split out from the Win32 callback so the rules are unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum WindowRole {
    /// A live app view: its process belongs in Apps.
    AppWindow,
    /// An `ApplicationFrameWindow` that is still on screen (minimized counts)
    /// but no longer hosts a `CoreWindow`. The frame belongs to the shared
    /// broker, so it names an app only after it is paired with the cloaked
    /// `CoreWindow` its app parked (see [`pair_minimized_views`]).
    DetachedFrame,
    /// A cloaked top-level `CoreWindow`. On its own it is no evidence of an
    /// app: windowless system UI hosts (`TextInputHost.exe`) and Store apps
    /// that were closed but are still resident both keep one alive.
    HiddenCore,
    /// A cloaked ordinary window, or the ghost frame a closed Store app
    /// leaves behind — neither is a window the user can see.
    Ignore,
}

/// Native rule (see the module docs of `tm_core::classify`): a visible window
/// makes an App. `cloaked` is the raw `DWMWA_CLOAKED` value, `hosts_core`
/// whether an `ApplicationFrameWindow` currently hosts a `CoreWindow` child.
fn window_role(class: &str, cloaked: u32, hosts_core: bool) -> WindowRole {
    if class.eq_ignore_ascii_case(UWP_FRAME_CLASS) {
        return match (hosts_core, cloaked) {
            // A hosted CoreWindow means the view is live even when the frame
            // is cloaked, which is how a window on another virtual desktop
            // looks.
            (true, _) => WindowRole::AppWindow,
            (false, 0) => WindowRole::DetachedFrame,
            (false, _) => WindowRole::Ignore,
        };
    }
    if class.eq_ignore_ascii_case(UWP_CORE_CLASS) && cloaked != 0 {
        return WindowRole::HiddenCore;
    }
    if cloaked != 0 {
        return WindowRole::Ignore;
    }
    WindowRole::AppWindow
}

unsafe fn window_class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let len = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut buffer) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..len as usize])
}

/// Raw `DWMWA_CLOAKED` value; 0 (visible to the compositor) when DWM refuses
/// the query, so a failed attribute read never hides a window.
unsafe fn cloaked_state(hwnd: HWND) -> u32 {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};

    let mut cloaked: u32 = 0;
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
    }
    .is_ok();
    if ok { cloaked } else { 0 }
}

/// PID owning the hosted UWP `CoreWindow` inside an `ApplicationFrameWindow`.
///
/// The frame window belongs to `ApplicationFrameHost.exe`, a shared broker.
/// Attributing it to that process would make every Store app look like
/// "Application Frame Host" and push the real app into Background; native
/// Task Manager names the app the user sees.
unsafe fn hosted_app_pid(frame: HWND) -> Option<u32> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetWindowThreadProcessId};

    unsafe extern "system" fn child_cb(child: HWND, lparam: LPARAM) -> windows::core::BOOL {
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

/// Count `pid` as an app owner and carry over the window's hung state.
///
/// SAFETY: `IsHungAppWindow` is a local state query and does not send a
/// blocking message into the target process.
unsafe fn record(owners: &mut WindowOwners, pid: u32, hwnd: HWND) {
    owners.visible.insert(pid);
    if unsafe { windows::Win32::UI::WindowsAndMessaging::IsHungAppWindow(hwnd) }.as_bool() {
        owners.not_responding.insert(pid);
    }
}

unsafe extern "system" fn enum_cb(
    hwnd: HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
    };

    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return windows::core::BOOL(1);
        }

        // Skip tool windows (tooltips, invisible helper windows).
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex_style & (WS_EX_TOOLWINDOW.0 as isize) != 0 {
            return windows::core::BOOL(1);
        }

        let class = window_class(hwnd);
        let hosted = if class.eq_ignore_ascii_case(UWP_FRAME_CLASS) {
            hosted_app_pid(hwnd)
        } else {
            None
        };
        let mut owner_pid = || {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            pid
        };

        // SAFETY: lparam points at our Scan for the duration of EnumWindows.
        let scan = &mut *(lparam.0 as *mut Scan);
        match window_role(&class, cloaked_state(hwnd), hosted.is_some()) {
            WindowRole::AppWindow => match hosted.unwrap_or_else(&mut owner_pid) {
                0 => {}
                pid => record(&mut scan.owners, pid, hwnd),
            },
            WindowRole::DetachedFrame => scan.detached_frames.push(hwnd),
            WindowRole::HiddenCore => match owner_pid() {
                0 => {}
                pid => scan.hidden_cores.push((pid, hwnd)),
            },
            WindowRole::Ignore => {}
        }
    }
    windows::core::BOOL(1)
}

/// Put a minimized Store app back into Apps.
///
/// While a Store app's view is minimized the shell detaches its `CoreWindow`
/// from the frame and cloaks it, so neither half identifies the app on its
/// own: the frame belongs to `ApplicationFrameHost.exe`, and the cloaked
/// `CoreWindow` looks exactly like the one a closed-but-resident app (or a
/// windowless host such as `TextInputHost.exe`) keeps alive. Both halves do
/// carry the same application user model id, so pair them through that: a
/// cloaked `CoreWindow` counts only once a frame still on screen claims it.
fn pair_minimized_views(scan: &mut Scan) {
    if scan.detached_frames.is_empty() || scan.hidden_cores.is_empty() {
        return;
    }
    ensure_com();
    let frame_ids: HashSet<String> = scan
        .detached_frames
        .iter()
        .filter_map(|frame| unsafe { window_app_id(*frame) })
        .collect();
    if frame_ids.is_empty() {
        return;
    }
    let paired = paired_app_pids(&frame_ids, &scan.hidden_cores, process_app_id);
    for (pid, hwnd) in std::mem::take(&mut scan.hidden_cores) {
        if paired.contains(&pid) {
            unsafe { record(&mut scan.owners, pid, hwnd) };
        }
    }
}

/// Pids of the cloaked `CoreWindow`s whose app is claimed by a frame.
fn paired_app_pids(
    frame_ids: &HashSet<String>,
    cores: &[(u32, HWND)],
    app_id_of: impl Fn(u32) -> Option<String>,
) -> HashSet<u32> {
    cores
        .iter()
        .filter(|(pid, _)| app_id_of(*pid).is_some_and(|id| frame_ids.contains(&id)))
        .map(|(pid, _)| *pid)
        .collect()
}

thread_local! {
    /// COM apartment for the shell property store below. The sampling thread
    /// has none of its own, and a thread that already is in one answers
    /// `RPC_E_CHANGED_MODE`, which serves just as well — so the result is
    /// deliberately ignored. Never uninitialized: the apartment lasts as long
    /// as the thread that opened it.
    static COM_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn ensure_com() {
    COM_READY.with(|ready| {
        if !ready.get() {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
            }
            ready.set(true);
        }
    });
}

/// Application user model id the shell attached to a window, lowercased
/// (`AppUserModelID`s are case-insensitive). `None` when the window carries
/// none, which is the normal answer for every non-packaged window.
unsafe fn window_app_id(hwnd: HWND) -> Option<String> {
    use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::UI::Shell::PropertiesSystem::{
        IPropertyStore, SHGetPropertyStoreForWindow,
    };

    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        let value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let text = PropVariantToStringAlloc(&value).ok()?;
        let id = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        id.filter(|id| !id.is_empty())
            .map(|id| id.to_ascii_lowercase())
    }
}

/// Application user model id of a packaged process, lowercased. `None` for
/// every ordinary desktop process (`APPMODEL_ERROR_NO_APPLICATION`).
fn process_app_id(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
    use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        // Application user model ids stay well below this; a longer one is
        // reported as no match instead of being queried a second time.
        let mut len = 512u32;
        let mut buffer = vec![0u16; len as usize];
        let status = GetApplicationUserModelId(
            handle,
            &mut len,
            Some(windows::core::PWSTR(buffer.as_mut_ptr())),
        );
        let _ = CloseHandle(handle);
        if status != ERROR_SUCCESS {
            return None;
        }
        // `len` counts the terminating NUL.
        let id = String::from_utf16_lossy(&buffer[..(len as usize).saturating_sub(1)]);
        (!id.is_empty()).then(|| id.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hwnd(raw: usize) -> HWND {
        HWND(raw as *mut std::ffi::c_void)
    }

    #[test]
    fn open_store_app_counts_through_its_frame() {
        assert_eq!(
            window_role(UWP_FRAME_CLASS, 0, true),
            WindowRole::AppWindow,
            "a frame hosting a CoreWindow is the app's live view"
        );
    }

    #[test]
    fn store_app_on_another_virtual_desktop_still_counts() {
        // Cloaked by the shell, but the view is intact: the frame still hosts
        // the app's CoreWindow.
        assert_eq!(window_role(UWP_FRAME_CLASS, 2, true), WindowRole::AppWindow);
    }

    #[test]
    fn minimized_store_app_frame_waits_for_pairing() {
        // Minimized: the frame stays uncloaked but the shell detached the
        // CoreWindow, so the frame names ApplicationFrameHost.exe and nothing
        // else until it is paired with the app's own window.
        assert_eq!(
            window_role(UWP_FRAME_CLASS, 0, false),
            WindowRole::DetachedFrame
        );
    }

    #[test]
    fn ghost_frame_of_a_closed_store_app_is_ignored() {
        assert_eq!(window_role(UWP_FRAME_CLASS, 2, false), WindowRole::Ignore);
    }

    #[test]
    fn cloaked_core_window_alone_is_not_an_app() {
        // TextInputHost.exe and a closed-but-resident SystemSettings.exe both
        // keep a visible, shell-cloaked CoreWindow; neither is an App.
        assert_eq!(
            window_role(UWP_CORE_CLASS, 2, false),
            WindowRole::HiddenCore
        );
        assert_eq!(
            window_role(UWP_CORE_CLASS, 1, false),
            WindowRole::HiddenCore
        );
    }

    #[test]
    fn uncloaked_core_window_is_an_app() {
        assert_eq!(window_role(UWP_CORE_CLASS, 0, false), WindowRole::AppWindow);
    }

    #[test]
    fn ordinary_windows_follow_the_cloak() {
        assert_eq!(
            window_role("Chrome_WidgetWin_1", 0, false),
            WindowRole::AppWindow
        );
        assert_eq!(
            window_role("Chrome_WidgetWin_1", 1, false),
            WindowRole::Ignore
        );
    }

    #[test]
    fn class_match_is_case_insensitive() {
        assert_eq!(
            window_role("applicationframewindow", 2, false),
            WindowRole::Ignore
        );
        assert_eq!(
            window_role("windows.ui.core.corewindow", 2, false),
            WindowRole::HiddenCore
        );
    }

    #[test]
    fn only_the_app_claimed_by_a_frame_is_paired() {
        let settings =
            "windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel";
        let frame_ids: HashSet<String> = [settings.to_string()].into_iter().collect();
        let cores = [(1884u32, hwnd(1)), (8756u32, hwnd(2)), (4242u32, hwnd(3))];
        let paired = paired_app_pids(&frame_ids, &cores, |pid| match pid {
            1884 => Some(settings.to_string()),
            // The input host is packaged too, but no frame claims it.
            8756 => Some("microsoftwindows.client.cbs_cw5n1h2txyewy!inputapp".to_string()),
            // Unpackaged: no application user model id at all.
            _ => None,
        });
        assert_eq!(paired, [1884u32].into_iter().collect::<HashSet<u32>>());
    }
}
