//! Native window frame appearance: the caption Windows itself draws.
//!
//! The app owns everything below the caption but not the caption itself, so
//! the two only look like one surface if DWM is told what the app is painting.
//! Left alone, a dark UI sits under a light title bar with black glyphs.
//!
//! Everything here is a `DwmSetWindowAttribute` call. Attributes Windows does
//! not know are rejected with an HRESULT and ignored — which is the whole
//! compatibility story: `IMMERSIVE_DARK_MODE` needs Windows 10 1809, the color
//! attributes need Windows 11 22000, and `SYSTEMBACKDROP_TYPE` needs 22621.
//! Nothing branches on a build number; each call simply may or may not land.

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_CLOAK, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE, DwmSetWindowAttribute,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, HWND_NOTOPMOST, HWND_TOPMOST, IsIconic,
    IsWindowVisible, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING, SWP_NOSIZE,
    SetWindowPos, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

/// What the caption should look like, in the app's own terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleBar {
    /// Fill of the caption strip. Set this to the color the client area
    /// paints directly underneath it and the seam disappears.
    pub caption: [u8; 3],
    /// Caption text and the minimize/maximize/close glyphs.
    pub text: [u8; 3],
    /// The one-pixel window border.
    pub border: [u8; 3],
    /// Whether the app is running a dark theme. Drives the immersive-dark-mode
    /// flag, which is what colors the system menu and the hover/pressed
    /// highlights of the caption buttons — those are not covered by
    /// `DWMWA_TEXT_COLOR`.
    pub dark: bool,
    /// Ask DWM for the Windows 11 Mica material behind the window, honoring
    /// the user's "Transparency effects" setting.
    ///
    /// What this can and cannot do is worth being exact about. DWM composes
    /// the material BEHIND the window and it shows through wherever the
    /// window is transparent. The window's client area is opaque — the CPU
    /// renderer presents through `BitBlt` from a DIB, which carries no usable
    /// alpha — so the material can only appear where DWM draws: the frame,
    /// the rounded corners and the caption. And on the caption an explicit
    /// [`Self::caption`] colour wins over the material.
    ///
    /// So this is a real request, not decoration, but a caption colour is the
    /// stronger of the two and is what makes the caption and the strip the
    /// app paints directly below it read as one surface. Making that strip
    /// itself translucent would take a presentation path that carries
    /// per-pixel alpha, which the CPU renderer does not have.
    pub backdrop: bool,
}

/// `DWMWA_SYSTEMBACKDROP_TYPE` value for Mica ("main window" material).
const DWMSBT_MAINWINDOW: i32 = 2;
/// ...and for "no backdrop", which is the pre-Windows-11 look.
const DWMSBT_NONE: i32 = 1;

/// COLORREF is 0x00BBGGRR — the reverse of the RGB order everywhere else, and
/// getting it wrong yields a plausible-looking wrong color rather than an
/// error, so it is converted in exactly one place.
fn colorref(rgb: [u8; 3]) -> COLORREF {
    COLORREF(u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16))
}

fn set_attr<T: Copy>(hwnd: HWND, attr: DWMWINDOWATTRIBUTE, value: &T) -> bool {
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attr,
            (value as *const T).cast(),
            std::mem::size_of::<T>() as u32,
        )
    }
    .is_ok()
}

/// Push `look` onto the window. Cheap enough to call whenever it changes;
/// callers must not call it every frame (each attribute triggers a frame
/// recomposition).
pub fn apply(hwnd: isize, look: TitleBar) {
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);

    // Dark mode first: it repaints the caption, so setting it after the
    // colors would briefly show the system default over them.
    let dark = windows::core::BOOL::from(look.dark);
    set_attr(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark);

    set_attr(
        hwnd,
        DWMWA_SYSTEMBACKDROP_TYPE,
        &if look.backdrop {
            DWMSBT_MAINWINDOW
        } else {
            DWMSBT_NONE
        },
    );

    set_attr(hwnd, DWMWA_CAPTION_COLOR, &colorref(look.caption));
    set_attr(hwnd, DWMWA_TEXT_COLOR, &colorref(look.text));
    set_attr(hwnd, DWMWA_BORDER_COLOR, &colorref(look.border));
}

static STRICT_TOPMOST_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static STRICT_TOPMOST_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// The installed hooks, so they can be taken back down again.
///
/// Stored as raw pointer values: `HWINEVENTHOOK` is not `Send`, and these are
/// only ever touched from the UI thread that installed them.
static STRICT_TOPMOST_HOOKS: std::sync::Mutex<Vec<isize>> = std::sync::Mutex::new(Vec::new());

const OBJID_WINDOW_I32: i32 = 0;

fn reassert_strict_topmost() {
    use std::sync::atomic::Ordering;

    if !STRICT_TOPMOST_ENABLED.load(Ordering::Acquire) {
        return;
    }
    let raw = STRICT_TOPMOST_HWND.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut std::ffi::c_void);
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return;
        }
        // HWND_TOPMOST is also an insertion point. Reapplying it moves this
        // window to the front of the topmost band without stealing focus.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING,
        );
    }
}

unsafe extern "system" fn strict_topmost_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    _event_hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if event == EVENT_SYSTEM_FOREGROUND || id_object == OBJID_WINDOW_I32 {
        reassert_strict_topmost();
    }
}

/// Take the strict-topmost hooks back down.
///
/// Not merely tidy: these are SYSTEM-WIDE out-of-context hooks, so for as
/// long as they exist Windows marshals a matching event from EVERY process on
/// the desktop into this process's message queue. Leaving them installed
/// after the user turned always-on-top back off means paying that for a
/// callback that now returns immediately, for the rest of the session.
fn remove_strict_topmost_hooks() {
    let mut hooks = match STRICT_TOPMOST_HOOKS.lock() {
        Ok(hooks) => hooks,
        Err(poisoned) => poisoned.into_inner(),
    };
    for raw in hooks.drain(..) {
        unsafe {
            let _ = UnhookWinEvent(HWINEVENTHOOK(raw as *mut std::ffi::c_void));
        }
    }
}

/// Install the hooks that keep the window at the front of its band.
///
/// Only two events, and the choice is load-bearing. `EVENT_SYSTEM_FOREGROUND`
/// and `EVENT_OBJECT_SHOW` are the two that can actually put another window
/// above this one. `EVENT_OBJECT_REORDER` used to be here as well and was the
/// expensive mistake: it fires for z-order churn *inside* other processes'
/// windows — every list, tree, tab strip and toolbar in every running app —
/// and each one costs a cross-process marshal into this process before the
/// callback can decide it was uninteresting. Reasserting the band on
/// foreground changes and on newly shown windows covers the real cases.
fn install_strict_topmost_hooks() {
    let mut hooks = match STRICT_TOPMOST_HOOKS.lock() {
        Ok(hooks) => hooks,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !hooks.is_empty() {
        return;
    }
    let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
    for event in [EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_SHOW] {
        let hook =
            unsafe { SetWinEventHook(event, event, None, Some(strict_topmost_event), 0, 0, flags) };
        if hook.is_invalid() {
            tracing::warn!(event, "cannot install strict-topmost WinEvent hook");
        } else {
            hooks.push(hook.0 as isize);
        }
    }
}

/// Ask the patched winit to create the main window in window band 16 — the
/// band native Task Manager uses, which is above the Start menu (band 6).
///
/// The band can only be chosen at window creation (`SetWindowBand` fails with
/// `ERROR_ACCESS_DENIED` for existing windows), so this must run before the
/// GUI window exists. Call [`clear_topmost_band`] once it does, so child
/// processes launched by TaskMan do not inherit the setting.
pub fn request_topmost_band() {
    unsafe {
        let _ = windows::Win32::System::Environment::SetEnvironmentVariableW(
            windows::core::w!("TASKMAN_WINDOW_BAND"),
            windows::core::w!("16"),
        );
    }
}

/// Stop advertising the band to processes TaskMan launches.
pub fn clear_topmost_band() {
    unsafe {
        let _ = windows::Win32::System::Environment::SetEnvironmentVariableW(
            windows::core::w!("TASKMAN_WINDOW_BAND"),
            windows::core::PCWSTR::null(),
        );
    }
}

/// Keep TaskMan above the Windows shell as well as ordinary top-level windows.
///
/// The taskbar and ordinary topmost windows live in window band 1
/// (`ZBID_DESKTOP`), so reinserting at the front of that band keeps TaskMan
/// above them. The Start menu, search flyout and other immersive shell
/// surfaces live in band 6 (`ZBID_IMMERSIVE_MOBILE`), which no normal window
/// can outrank: `SetWindowBand` to a higher band fails with
/// ERROR_INVALID_PARAMETER (87) from a normal process, and UIAccess (band 2)
/// is still below 6. That is a Windows shell design boundary, not a missing
/// reassert — measured 2026-09-09 and recorded in `known-debt.md`. Do not try
/// to "fix" it with polling or by fighting the shell.
pub fn set_strict_topmost(hwnd: isize, enabled: bool) {
    use std::sync::atomic::Ordering;

    let native = HWND(hwnd as *mut std::ffi::c_void);
    if enabled {
        STRICT_TOPMOST_HWND.store(hwnd, Ordering::Release);
        STRICT_TOPMOST_ENABLED.store(true, Ordering::Release);
        install_strict_topmost_hooks();
        reassert_strict_topmost();
    } else {
        STRICT_TOPMOST_ENABLED.store(false, Ordering::Release);
        STRICT_TOPMOST_HWND.store(0, Ordering::Release);
        remove_strict_topmost_hooks();
        unsafe {
            let _ = SetWindowPos(
                native,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING,
            );
        }
    }
}

/// Hide or reveal a window at the COMPOSITOR, without changing whether
/// Windows considers it shown.
///
/// This exists to kill the white flash when the window comes back from the
/// tray. `ShowWindow(SW_HIDE)` drops the window's composed content, and a
/// hidden window receives no `WM_PAINT`, so there is no way to have a frame
/// ready before it appears: DWM composes the empty window first and the app's
/// first frame lands one beat later. Cloaking inverts the order — show the
/// window cloaked so it starts painting, then uncloak once a real frame has
/// been presented.
///
/// A cloaked window is still "visible" to Windows (it keeps its taskbar
/// button and its focus), which is exactly why this is used only for the
/// handful of frames around a restore and never as the tray-hide mechanism.
pub fn set_cloaked(hwnd: isize, cloaked: bool) {
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    set_attr(
        hwnd,
        DWMWA_CLOAK,
        &windows::core::BOOL::from(cloaked).0.cast_unsigned(),
    );
}

/// `hwnd` as a live window handle, or `None` for zero and stale handles.
fn live_window(hwnd: isize) -> Option<HWND> {
    if hwnd == 0 {
        return None;
    }
    let target = HWND(hwnd as *mut std::ffi::c_void);
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(target)) }
        .as_bool()
        .then_some(target)
}

/// Show, restore, raise and activate `hwnd` without merging anybody's input
/// queue. Safe from any thread.
///
/// On the thread that owns the window everything is synchronous, so the
/// window is up when this returns. From any other thread every call is POSTED
/// instead: a plain `ShowWindow`/`SetWindowPos` against another thread's
/// window waits, with no timeout, for that thread to answer — and the window
/// this is used on is TaskMan's own, whose UI thread is exactly what may be
/// wedged when somebody reaches for a task manager. A target already known
/// to be hung is left alone.
///
/// It cannot override the foreground lock, so over an exclusive-fullscreen
/// window it may flash the taskbar button instead of switching. The one
/// thread allowed to do more uses [`force_foreground_attached`].
pub fn force_foreground(hwnd: isize) {
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsHungAppWindow};

    let Some(target) = live_window(hwnd) else {
        return;
    };
    unsafe {
        if GetWindowThreadProcessId(target, None) == GetCurrentThreadId() {
            activate_owned(target);
        } else if !IsHungAppWindow(target).as_bool() {
            activate_foreign(target);
        }
    }
}

/// [`force_foreground`] on the window's own thread: nothing here can wait on
/// anybody else.
unsafe fn activate_owned(target: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, HWND_TOP, SW_RESTORE, SW_SHOW, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
        SetForegroundWindow, ShowWindow,
    };

    unsafe {
        let _ = ShowWindow(
            target,
            if IsIconic(target).as_bool() {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        let _ = SetWindowPos(
            target,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = BringWindowToTop(target);
        let _ = SetForegroundWindow(target);

        if let Ok(user32) =
            windows::Win32::System::LibraryLoader::GetModuleHandleW(windows::core::w!("user32.dll"))
            && let Some(proc) = windows::Win32::System::LibraryLoader::GetProcAddress(
                user32,
                windows::core::s!("SwitchToThisWindow"),
            )
        {
            type SwitchToThisWindowFn = unsafe extern "system" fn(HWND, windows::core::BOOL);
            let switch_fn: SwitchToThisWindowFn = std::mem::transmute(proc);
            switch_fn(target, true.into());
        }
    }
}

/// [`force_foreground`] from a thread that does not own the window: the same
/// requests, queued to the owner instead of waited on.
///
/// `SwitchToThisWindow` is deliberately absent: it has no asynchronous form,
/// and everything it adds (restoring, raising) is already queued here.
unsafe fn activate_foreign(target: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOP, SW_RESTORE, SW_SHOW, SWP_ASYNCWINDOWPOS, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
        SetForegroundWindow, ShowWindowAsync,
    };

    unsafe {
        let _ = ShowWindowAsync(
            target,
            if IsIconic(target).as_bool() {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        let _ = SetWindowPos(
            target,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_ASYNCWINDOWPOS,
        );
        // Activating a window of another input queue queues the activation
        // to that queue; it does not wait for the owner to process it.
        let _ = SetForegroundWindow(target);
    }
}

/// Take the foreground for `hwnd` over a fullscreen application by merging
/// this thread's input queue with the foreground thread's for one call.
///
/// Under standard foreground-lock rules a background process cannot steal the
/// foreground, and over an exclusive or borderless fullscreen game a plain
/// `SetForegroundWindow` is dropped or only flashes the taskbar button. A
/// thread that shares the FOREGROUND thread's input queue is exempt, which is
/// the whole reason the Ctrl+Shift+Esc hook exists.
///
/// ## Why the merge is kept this small
///
/// `AttachThreadInput` MERGES input queues, and MSDN is blunt about what that
/// costs: threads that share an input queue stop responding together. The
/// queue merged with is the game or editor the user is typing into, so the
/// merge is held to exactly one call, under these rules:
///
/// * **Only from a thread that pumps its own message queue.** The hotkey
///   worker does and is the only caller; the launcher's startup thread and
///   the UI thread inside `update()` do not.
/// * **Only the foreground thread is attached, never the target's.**
///   Attachments chain: attaching the target as well — as this once did —
///   puts the game in one queue with TaskMan's UI thread, whose frame then
///   stands between the game and its own input until the detach. The lock
///   exemption only needs the foreground queue. See [`merge_partner`].
/// * **Only the activation runs inside it.** Showing, restoring and raising
///   are the window's owner's job — the UI thread answers the same hotkey
///   through its show request — and from this thread each would be a call
///   that waits on another thread. Activating a window of another queue is
///   queued to that queue rather than waited on.
/// * **A hung target cancels everything**, and a hung foreground window
///   cancels the merge: joining a queue that is not being pumped is how one
///   stuck application takes the desktop's input with it.
pub fn force_foreground_attached(hwnd: isize) {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsHungAppWindow, SetForegroundWindow,
    };

    let Some(target) = live_window(hwnd) else {
        return;
    };
    unsafe {
        if IsHungAppWindow(target).as_bool() {
            return;
        }
        let current = GetCurrentThreadId();
        let target_thread = GetWindowThreadProcessId(target, None);
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.0.is_null() || IsHungAppWindow(foreground).as_bool() {
            None
        } else {
            Some(GetWindowThreadProcessId(foreground, None))
        };
        let partner = merge_partner(current, target_thread, foreground_thread);
        let attached = partner != 0 && AttachThreadInput(current, partner, true).as_bool();
        let _ = SetForegroundWindow(target);
        if attached {
            let _ = AttachThreadInput(current, partner, false);
        }
    }
}

/// The one thread [`force_foreground_attached`] may merge with, or 0 for none.
///
/// Only a live foreground thread qualifies (`None` covers "no foreground
/// window" and "foreground window hung"), and never this thread — merging a
/// queue with itself is a no-op the detach would then have to guess about —
/// nor the target's: when the target's thread already holds the foreground
/// there is no lock to get past, and attaching it would chain the merge onto
/// TaskMan's UI thread.
fn merge_partner(current: u32, target: u32, foreground: Option<u32>) -> u32 {
    match foreground {
        Some(thread) if thread != 0 && thread != current && thread != target => thread,
        _ => 0,
    }
}

/// Whether the user has Windows' "Transparency effects" turned on.
///
/// Honoring it is not decoration: it is an accessibility and battery setting,
/// and Windows' own apps drop their material when it is off. Unreadable
/// registry state counts as "on", matching the Windows default.
pub fn transparency_effects_enabled() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::w;

    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("EnableTransparency"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    if status.is_err() { true } else { value != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// COLORREF is byte-reversed against every other color in this codebase.
    /// A wrong order produces a believable colour, not a failure, so it is
    /// pinned: pure red must set the LOW byte.
    #[test]
    fn colorref_is_bgr_ordered() {
        assert_eq!(colorref([0xff, 0x00, 0x00]).0, 0x0000_00ff);
        assert_eq!(colorref([0x00, 0xff, 0x00]).0, 0x0000_ff00);
        assert_eq!(colorref([0x00, 0x00, 0xff]).0, 0x00ff_0000);
        assert_eq!(colorref([0x19, 0x1a, 0x1b]).0, 0x001b_1a19);
    }

    /// Reads a real registry value; it must answer without panicking and the
    /// Windows default (on) must survive a missing value.
    #[test]
    fn transparency_preference_is_readable() {
        let _ = transparency_effects_enabled();
    }

    #[test]
    fn force_foreground_handles_zero_and_invalid_hwnd() {
        // Zero or invalid HWND must gracefully return without panicking.
        force_foreground(0);
        force_foreground(0x12345);
        force_foreground_attached(0);
        force_foreground_attached(0x12345);
    }

    /// The merge is with the foreground thread and nothing else. Attaching
    /// the target's thread too chained the game's input queue onto TaskMan's
    /// UI thread, so a slow frame there stalled the game's keyboard.
    #[test]
    fn merge_partner_is_only_ever_a_live_foreign_foreground_thread() {
        const CURRENT: u32 = 10;
        const TARGET: u32 = 20;
        const GAME: u32 = 30;
        assert_eq!(merge_partner(CURRENT, TARGET, Some(GAME)), GAME);
        assert_eq!(
            merge_partner(CURRENT, TARGET, Some(TARGET)),
            0,
            "the target's own thread must never be merged with"
        );
        assert_eq!(merge_partner(CURRENT, TARGET, Some(CURRENT)), 0);
        assert_eq!(
            merge_partner(CURRENT, TARGET, None),
            0,
            "no (or a hung) foreground window means no merge"
        );
        assert_eq!(merge_partner(CURRENT, TARGET, Some(0)), 0);
    }
}
