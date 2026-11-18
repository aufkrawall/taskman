//! Enumerate pids owning visible top-level windows (used for App detection,
//! mirroring Task Manager's "Apps" grouping).

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_WINDOW_HANDLE, ERROR_SUCCESS, GetLastError, HWND, LPARAM,
    SetLastError, WPARAM,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::{s, w};

#[derive(Clone, Default)]
pub struct WindowOwners {
    pub visible: HashSet<u32>,
    pub not_responding: HashSet<u32>,
}

/// Scratch state of one `EnumWindows` pass: the owners found so far, candidates
/// for non-blocking responsiveness corroboration, plus the two halves of a
/// minimized Store app (see [`pair_minimized_views`]).
#[derive(Default)]
struct Scan {
    owners: WindowOwners,
    detached_frames: Vec<HWND>,
    hidden_cores: Vec<(u32, HWND)>,
    /// `(pid, tid, hwnd)` of eligible windows where `IsHungAppWindow` fired.
    hung_candidates: Vec<(u32, u32, HWND)>,
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
    scan.owners.not_responding =
        resolve_hung_candidates(&scan.hung_candidates, probe_window_responds);
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
const SHELL_TRAY_CLASS: &str = "Shell_TrayWnd";
const GHOST_CLASS: &str = "Ghost";

// Window Z-order bands matching Task Manager (`IsDesktopWindow`): standard
// desktop apps (1), UIAccess (2), native Task Manager / system tools (16),
// plus `ZBID_GENUINE_WINDOWS` (13) for Store apps.
const ZBID_DESKTOP: u32 = 1;
const ZBID_UIACCESS: u32 = 2;
const ZBID_GENUINE_WINDOWS: u32 = 13;
const ZBID_SYSTEM_TOOLS: u32 = 16;

/// Timeout for one non-aborting `WM_NULL` probe confirming an
/// `IsHungAppWindow` hit. 200 ms covers 5+ FPS background loops (e.g. a 30 FPS
/// game responds within ~33 ms) while returning quickly when truly wedged.
const WM_NULL_PROBE_TIMEOUT_MS: u32 = 200;
/// Cap total `WM_NULL` probe wait per scan so multiple simultaneously frozen
/// apps cannot stall the sampler tick.
const MAX_PROBE_BUDGET_PER_SCAN: Duration = Duration::from_millis(600);

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

/// Owning PID and `CoreWindow` handle hosted inside an `ApplicationFrameWindow`.
///
/// The frame window belongs to `ApplicationFrameHost.exe`, a shared broker.
/// Attributing it to that process would make every Store app look like
/// "Application Frame Host" and push the real app into Background; native
/// Task Manager names the app the user sees. Targeting the hosted `CoreWindow`
/// for responsiveness also probes the app's own UI thread rather than the
/// shared frame broker.
unsafe fn hosted_app_window(frame: HWND) -> Option<(u32, HWND)> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetWindowThreadProcessId};

    unsafe extern "system" fn child_cb(child: HWND, lparam: LPARAM) -> windows::core::BOOL {
        unsafe {
            if window_class(child).eq_ignore_ascii_case(UWP_CORE_CLASS) {
                let mut pid = 0u32;
                GetWindowThreadProcessId(child, Some(&mut pid));
                if pid != 0 {
                    *(lparam.0 as *mut Option<(u32, HWND)>) = Some((pid, child));
                    return windows::core::BOOL(0);
                }
            }
        }
        windows::core::BOOL(1)
    }

    let mut found: Option<(u32, HWND)> = None;
    let _ = unsafe {
        EnumChildWindows(
            Some(frame),
            Some(child_cb),
            LPARAM(&mut found as *mut Option<(u32, HWND)> as isize),
        )
    };
    found
}

fn is_interactive_style(ex_style: u32) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };
    let has_appwindow = (ex_style & WS_EX_APPWINDOW.0) != 0;
    let non_interactive = (ex_style & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0)) != 0;
    has_appwindow || !non_interactive
}

fn is_unowned_or_appwindow(ex_style: u32, owner_visible: bool) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::WS_EX_APPWINDOW;
    (ex_style & WS_EX_APPWINDOW.0) != 0 || !owner_visible
}

fn should_skip_class(class: &str) -> bool {
    class.eq_ignore_ascii_case(SHELL_TRAY_CLASS) || class.eq_ignore_ascii_case(GHOST_CLASS)
}

fn is_interactive_band(class: &str, band: Option<u32>) -> bool {
    let Some(band) = band else {
        return true;
    };
    if class.eq_ignore_ascii_case(UWP_FRAME_CLASS) || class.eq_ignore_ascii_case(UWP_CORE_CLASS) {
        return matches!(
            band,
            ZBID_DESKTOP | ZBID_UIACCESS | ZBID_GENUINE_WINDOWS | ZBID_SYSTEM_TOOLS
        );
    }
    matches!(band, ZBID_DESKTOP | ZBID_UIACCESS | ZBID_SYSTEM_TOOLS)
}

fn hung_window_from_ghost_window(hwnd: HWND) -> Option<HWND> {
    type HungWindowFromGhostWindowFn = unsafe extern "system" fn(HWND) -> HWND;
    static FN: OnceLock<Option<HungWindowFromGhostWindowFn>> = OnceLock::new();
    let get_orig = (*FN.get_or_init(|| unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else {
            return None;
        };
        GetProcAddress(user32, s!("HungWindowFromGhostWindow")).map(|p| {
                std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    HungWindowFromGhostWindowFn,
                >(p)
            })
    }))?;
    let orig = unsafe { get_orig(hwnd) };
    (!orig.is_invalid() && orig != hwnd).then_some(orig)
}

unsafe fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

unsafe fn owner_is_visible(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GW_OWNER, GetWindow, IsWindowVisible};
    match unsafe { GetWindow(hwnd, GW_OWNER) } {
        Ok(owner) if !owner.is_invalid() && owner != hwnd => {
            unsafe { IsWindowVisible(owner) }.as_bool()
        }
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum EligibleWindow {
    App { pid: u32, probe_hwnd: HWND },
    DetachedFrame(HWND),
    HiddenCore { pid: u32, hwnd: HWND },
    Ignore,
}

unsafe fn classify_top_level(hwnd: HWND, allow_cloaked: bool) -> EligibleWindow {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, IsWindowVisible,
    };

    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return EligibleWindow::Ignore;
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if !is_interactive_style(ex_style)
            || !is_unowned_or_appwindow(ex_style, owner_is_visible(hwnd))
        {
            return EligibleWindow::Ignore;
        }
        let class = window_class(hwnd);
        if should_skip_class(&class) {
            return EligibleWindow::Ignore;
        }
        let hosted = if class.eq_ignore_ascii_case(UWP_FRAME_CLASS) {
            hosted_app_window(hwnd)
        } else {
            None
        };
        let cloaked = if allow_cloaked {
            0
        } else {
            cloaked_state(hwnd)
        };
        match window_role(&class, cloaked, hosted.is_some()) {
            WindowRole::AppWindow => {
                if !is_interactive_band(&class, super::taskbar_monitor::window_band(hwnd)) {
                    return EligibleWindow::Ignore;
                }
                if let Some((pid, core_hwnd)) = hosted {
                    return EligibleWindow::App {
                        pid,
                        probe_hwnd: core_hwnd,
                    };
                }
                let pid = window_pid(hwnd);
                if pid == 0 {
                    EligibleWindow::Ignore
                } else {
                    EligibleWindow::App {
                        pid,
                        probe_hwnd: hwnd,
                    }
                }
            }
            WindowRole::DetachedFrame => {
                if is_interactive_band(&class, super::taskbar_monitor::window_band(hwnd)) {
                    EligibleWindow::DetachedFrame(hwnd)
                } else {
                    EligibleWindow::Ignore
                }
            }
            WindowRole::HiddenCore => {
                let pid = window_pid(hwnd);
                if pid == 0 {
                    EligibleWindow::Ignore
                } else {
                    EligibleWindow::HiddenCore { pid, hwnd }
                }
            }
            WindowRole::Ignore => EligibleWindow::Ignore,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    Responded,
    TimedOut,
    AccessDenied,
    InvalidWindow,
}

fn hung_from_probe(outcome: ProbeOutcome) -> bool {
    match outcome {
        ProbeOutcome::Responded | ProbeOutcome::InvalidWindow => false,
        // UIPI blocks `WM_NULL` from an unelevated TaskMan to an elevated/UIAccess
        // window (`ERROR_ACCESS_DENIED`); keep the filtered `IsHungAppWindow` verdict
        // so genuinely hung elevated apps still surface.
        ProbeOutcome::TimedOut | ProbeOutcome::AccessDenied => true,
    }
}

/// Probe whether `hwnd`'s message loop dispatches `WM_NULL` within `timeout_ms`.
///
/// Deliberately omits `SMTO_ABORTIFHUNG` so a thread that is actively pumping
/// (for instance a backgrounded 30 FPS game hitting `win32kfull!IsPumpingInputMsgs`
/// via filtered `PeekMessage` or a transient hitch) still answers and refutes
/// a false-positive `IsHungAppWindow` report.
fn probe_window_responds(hwnd: HWND, timeout_ms: u32) -> ProbeOutcome {
    use windows::Win32::UI::WindowsAndMessaging::{SMTO_BLOCK, SendMessageTimeoutW, WM_NULL};

    let mut result: usize = 0;
    unsafe { SetLastError(ERROR_SUCCESS) };
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_BLOCK,
            timeout_ms,
            Some(&mut result),
        )
    };
    if sent.0 != 0 {
        return ProbeOutcome::Responded;
    }
    match unsafe { GetLastError() } {
        ERROR_ACCESS_DENIED => ProbeOutcome::AccessDenied,
        ERROR_INVALID_WINDOW_HANDLE => ProbeOutcome::InvalidWindow,
        _ => ProbeOutcome::TimedOut,
    }
}

fn resolve_hung_candidates(
    candidates: &[(u32, u32, HWND)],
    mut probe: impl FnMut(HWND, u32) -> ProbeOutcome,
) -> HashSet<u32> {
    let mut not_responding = HashSet::new();
    if candidates.is_empty() {
        return not_responding;
    }
    let start = Instant::now();
    let mut probed_threads: HashSet<u32> = HashSet::new();

    for &(pid, tid, hwnd) in candidates {
        if not_responding.contains(&pid) {
            continue;
        }
        if tid != 0 && !probed_threads.insert(tid) {
            continue;
        }
        if start.elapsed() >= MAX_PROBE_BUDGET_PER_SCAN {
            not_responding.insert(pid);
            continue;
        }
        if hung_from_probe(probe(hwnd, WM_NULL_PROBE_TIMEOUT_MS)) {
            not_responding.insert(pid);
        }
    }
    not_responding
}

/// Count `pid` as an app owner and record `hwnd` as a hung-probe candidate when
/// `IsHungAppWindow` reports true.
///
/// Only eligible interactive windows reach here; final `not_responding`
/// membership is verified post-scan via [`resolve_hung_candidates`] and then
/// debounced by `sampler::NotRespondingHold`.
unsafe fn record(scan: &mut Scan, pid: u32, hwnd: HWND) {
    scan.owners.visible.insert(pid);
    if unsafe { windows::Win32::UI::WindowsAndMessaging::IsHungAppWindow(hwnd) }.as_bool() {
        let tid = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, None)
        };
        scan.hung_candidates.push((pid, tid, hwnd));
    }
}

unsafe extern "system" fn enum_cb(
    hwnd: HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return windows::core::BOOL(1);
        }

        // SAFETY: lparam points at our Scan for the duration of EnumWindows.
        let scan = &mut *(lparam.0 as *mut Scan);

        // Resolve DWM ghost windows back to the underlying hung window so
        // `dwm.exe` is never blamed while real ghosted hangs remain detectable.
        if let Some(orig) = hung_window_from_ghost_window(hwnd) {
            if let EligibleWindow::App { pid, probe_hwnd } = classify_top_level(orig, true) {
                record(scan, pid, probe_hwnd);
            }
            return windows::core::BOOL(1);
        }

        match classify_top_level(hwnd, false) {
            EligibleWindow::App { pid, probe_hwnd } => record(scan, pid, probe_hwnd),
            EligibleWindow::DetachedFrame(frame) => scan.detached_frames.push(frame),
            EligibleWindow::HiddenCore { pid, hwnd } => scan.hidden_cores.push((pid, hwnd)),
            EligibleWindow::Ignore => {}
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
            unsafe { record(scan, pid, hwnd) };
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

    #[test]
    fn interactive_style_filters_tool_and_noactivate_unless_appwindow() {
        use windows::Win32::UI::WindowsAndMessaging::{
            WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        };
        assert!(is_interactive_style(0));
        assert!(!is_interactive_style(WS_EX_TOOLWINDOW.0));
        assert!(!is_interactive_style(WS_EX_NOACTIVATE.0));
        assert!(!is_interactive_style(
            WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0
        ));
        assert!(is_interactive_style(WS_EX_NOACTIVATE.0 | WS_EX_APPWINDOW.0));
        assert!(is_interactive_style(WS_EX_TOOLWINDOW.0 | WS_EX_APPWINDOW.0));
    }

    #[test]
    fn owned_window_excluded_only_when_owner_visible_without_appwindow() {
        use windows::Win32::UI::WindowsAndMessaging::WS_EX_APPWINDOW;
        assert!(is_unowned_or_appwindow(0, false));
        assert!(!is_unowned_or_appwindow(0, true));
        assert!(is_unowned_or_appwindow(WS_EX_APPWINDOW.0, true));
    }

    #[test]
    fn interactive_band_filters_shell_overlays_while_keeping_desktop_and_uwp() {
        assert!(is_interactive_band("UnrealWindow", None));
        assert!(is_interactive_band("UnrealWindow", Some(ZBID_DESKTOP)));
        assert!(is_interactive_band("UnrealWindow", Some(ZBID_UIACCESS)));
        assert!(is_interactive_band("UnrealWindow", Some(ZBID_SYSTEM_TOOLS)));
        assert!(!is_interactive_band("UnrealWindow", Some(6)));
        assert!(!is_interactive_band(
            "UnrealWindow",
            Some(ZBID_GENUINE_WINDOWS)
        ));
        assert!(is_interactive_band(
            UWP_FRAME_CLASS,
            Some(ZBID_GENUINE_WINDOWS)
        ));
        assert!(is_interactive_band(
            UWP_CORE_CLASS,
            Some(ZBID_GENUINE_WINDOWS)
        ));
    }

    #[test]
    fn skip_class_excludes_shell_tray_and_ghost() {
        assert!(should_skip_class("Shell_TrayWnd"));
        assert!(should_skip_class("shell_traywnd"));
        assert!(should_skip_class("Ghost"));
        assert!(!should_skip_class("UnrealWindow"));
    }

    #[test]
    fn resolve_hung_candidates_dedups_threads_and_honors_probe_outcomes() {
        // Active message pump refutes `IsHungAppWindow` and deduplicates
        // multiple windows on the same UI thread into a single probe.
        let mut probed = Vec::new();
        let out = resolve_hung_candidates(&[(42, 100, hwnd(1)), (42, 100, hwnd(2))], |h, _ms| {
            probed.push(h.0 as usize);
            ProbeOutcome::Responded
        });
        assert_eq!(probed, vec![1]);
        assert!(out.is_empty());

        // Timeout or UIPI AccessDenied keeps the PID marked non-responding
        // and short-circuits further threads of the same PID.
        for outcome in [ProbeOutcome::TimedOut, ProbeOutcome::AccessDenied] {
            let mut count = 0;
            let out =
                resolve_hung_candidates(&[(77, 10, hwnd(1)), (77, 11, hwnd(2))], |_h, _ms| {
                    count += 1;
                    outcome
                });
            assert_eq!(count, 1);
            assert_eq!(out, [77u32].into_iter().collect::<HashSet<u32>>());
        }

        // First thread healthy, second thread genuinely wedged -> PID reported.
        let out = resolve_hung_candidates(&[(88, 1, hwnd(1)), (88, 2, hwnd(2))], |h, _ms| {
            if h == hwnd(1) {
                ProbeOutcome::Responded
            } else {
                ProbeOutcome::TimedOut
            }
        });
        assert_eq!(out, [88u32].into_iter().collect::<HashSet<u32>>());

        // Destroyed window during scan is ignored.
        let out = resolve_hung_candidates(&[(99, 5, hwnd(1))], |_, _| ProbeOutcome::InvalidWindow);
        assert!(out.is_empty());
    }

    struct OwnedHwnd(HWND);
    impl Drop for OwnedHwnd {
        fn drop(&mut self) {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
            }
        }
    }

    fn create_test_popup(
        ex_style: windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE,
        visible: bool,
        owner: Option<HWND>,
    ) -> OwnedHwnd {
        use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WS_POPUP, WS_VISIBLE};
        let mut style = WS_POPUP;
        if visible {
            style |= WS_VISIBLE;
        }
        let handle = unsafe {
            CreateWindowExW(
                ex_style,
                w!("STATIC"),
                w!("tm-enum-test"),
                style,
                0,
                0,
                16,
                16,
                owner,
                None,
                None,
                None,
            )
        }
        .expect("create test window");
        OwnedHwnd(handle)
    }

    #[test]
    fn real_windows_enforce_noactivate_and_visible_owner_rules() {
        use windows::Win32::UI::WindowsAndMessaging::{
            WINDOW_EX_STYLE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        };

        let me = std::process::id();
        let visible_owner = create_test_popup(WINDOW_EX_STYLE(0), true, None);
        let hidden_owner = create_test_popup(WINDOW_EX_STYLE(0), false, None);
        let noactivate = create_test_popup(WS_EX_NOACTIVATE, true, None);
        let tool = create_test_popup(WS_EX_TOOLWINDOW, true, None);
        let noactivate_app = create_test_popup(WS_EX_NOACTIVATE | WS_EX_APPWINDOW, true, None);
        let owned_visible = create_test_popup(WINDOW_EX_STYLE(0), true, Some(visible_owner.0));
        let owned_app = create_test_popup(WS_EX_APPWINDOW, true, Some(visible_owner.0));
        let owned_hidden = create_test_popup(WINDOW_EX_STYLE(0), true, Some(hidden_owner.0));

        unsafe {
            assert_eq!(
                classify_top_level(visible_owner.0, false),
                EligibleWindow::App {
                    pid: me,
                    probe_hwnd: visible_owner.0,
                }
            );
            assert_eq!(
                classify_top_level(noactivate.0, false),
                EligibleWindow::Ignore
            );
            assert_eq!(classify_top_level(tool.0, false), EligibleWindow::Ignore);
            assert_eq!(
                classify_top_level(noactivate_app.0, false),
                EligibleWindow::App {
                    pid: me,
                    probe_hwnd: noactivate_app.0,
                }
            );
            assert_eq!(
                classify_top_level(owned_visible.0, false),
                EligibleWindow::Ignore
            );
            assert_eq!(
                classify_top_level(owned_app.0, false),
                EligibleWindow::App {
                    pid: me,
                    probe_hwnd: owned_app.0,
                }
            );
            assert_eq!(
                classify_top_level(owned_hidden.0, false),
                EligibleWindow::App {
                    pid: me,
                    probe_hwnd: owned_hidden.0,
                }
            );
        }
    }

    #[test]
    fn real_probe_distinguishes_pumping_from_blocked_window_threads() {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            CreateEventW, GetCurrentThreadId, INFINITE, SetEvent, WaitForSingleObject,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage,
            WINDOW_EX_STYLE, WM_QUIT,
        };

        // 1. Live pumping message loop responds immediately to `WM_NULL`.
        let (tx, rx) = std::sync::mpsc::channel();
        let pumping_handle = std::thread::spawn(move || {
            let win = create_test_popup(WINDOW_EX_STYLE(0), false, None);
            let tid = unsafe { GetCurrentThreadId() };
            tx.send((win.0.0 as isize, tid)).unwrap();
            let mut msg = MSG::default();
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });
        let (pump_raw, pump_tid) = rx.recv().unwrap();
        let pump_hwnd = HWND(pump_raw as *mut std::ffi::c_void);
        assert_eq!(
            probe_window_responds(pump_hwnd, 200),
            ProbeOutcome::Responded
        );
        unsafe {
            let _ = PostThreadMessageW(pump_tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        pumping_handle.join().unwrap();

        // 2. Blocked window thread that does not pump times out at `WM_NULL`.
        struct OwnedEvent(windows::Win32::Foundation::HANDLE);
        impl Drop for OwnedEvent {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let event = OwnedEvent(unsafe { CreateEventW(None, true, false, None) }.unwrap());
        let event_isize = event.0.0 as isize;
        let (tx_hung, rx_hung) = std::sync::mpsc::channel();

        let blocked_handle = std::thread::spawn(move || {
            let win = create_test_popup(WINDOW_EX_STYLE(0), false, None);
            tx_hung.send(win.0.0 as isize).unwrap();
            let ev = windows::Win32::Foundation::HANDLE(event_isize as *mut std::ffi::c_void);
            let _ = unsafe { WaitForSingleObject(ev, INFINITE) };
        });

        let hung_raw = rx_hung.recv().unwrap();
        let hung_hwnd = HWND(hung_raw as *mut std::ffi::c_void);
        assert_eq!(probe_window_responds(hung_hwnd, 40), ProbeOutcome::TimedOut);
        unsafe {
            let _ = SetEvent(event.0);
        }
        blocked_handle.join().unwrap();
    }
}
