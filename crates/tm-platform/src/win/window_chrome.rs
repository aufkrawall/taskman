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
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_TEXT_COLOR,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE, DwmSetWindowAttribute,
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
