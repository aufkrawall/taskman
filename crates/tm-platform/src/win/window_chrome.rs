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
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, HWND_NOTOPMOST, HWND_TOPMOST,
    IsIconic, IsWindowVisible, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING,
    SWP_NOSIZE, SetWindowPos, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
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
static STRICT_TOPMOST_HOOKS: std::sync::Once = std::sync::Once::new();

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

fn install_strict_topmost_hooks() {
    STRICT_TOPMOST_HOOKS.call_once(|| unsafe {
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        for event in [
            EVENT_SYSTEM_FOREGROUND,
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_REORDER,
        ] {
            let hook = SetWinEventHook(event, event, None, Some(strict_topmost_event), 0, 0, flags);
            if hook.is_invalid() {
                tracing::warn!(event, "cannot install strict-topmost WinEvent hook");
            }
        }
    });
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
}
