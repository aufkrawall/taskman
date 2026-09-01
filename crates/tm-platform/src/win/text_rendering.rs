//! Windows text-rendering parameters, for sub-pixel (ClearType) rendering.
//!
//! Sub-pixel rendering is not unconditionally correct, and turning it on when it is not
//! valid looks *worse* than grayscale, not better. This module answers two questions:
//!
//! 1. **Is it allowed here?** The user may have turned font smoothing off entirely, or
//!    selected grayscale rather than ClearType. Both are explicit choices and must be
//!    honoured.
//! 2. **Is it valid here?** Sub-pixel coverage only works when our pixels land on the
//!    monitor's pixels one-for-one. If the process is not per-monitor DPI aware, DWM
//!    bitmap-stretches the window and the fringes become visible colour noise. The same
//!    applies over RDP and under Magnifier.
//!
//! and then supplies the blend parameters -- gamma, contrast, ClearType level and stripe
//! order -- from DirectWrite, which fuses the system settings with the user's own
//! `cttune.exe` calibration for the monitor the window is on.
//!
//! Nothing here leaks a `windows` type upward: [`TextRenderingParams`] is plain data, so
//! the renderer and the egui fork stay free of platform dependencies.

use windows::Win32::Foundation::HWND;

/// Everything the renderer needs to decide how to draw text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextRenderingParams {
    /// Whether sub-pixel rendering should be used at all.
    pub enabled: bool,

    /// The panel's sub-pixel stripe order. `false` is the usual RGB.
    pub bgr: bool,

    /// Gamma of the text blend space. Windows' default is 1.8.
    pub gamma: f32,

    /// Enhanced-contrast boost. Windows' default is 0.5.
    pub contrast: f32,

    /// How far to blend toward grayscale: 1.0 is full ClearType, 0.0 is grayscale.
    pub cleartype_level: f32,
}

impl Default for TextRenderingParams {
    /// Windows' documented defaults, with sub-pixel rendering **on**.
    ///
    /// Used when DirectWrite is unavailable but the system gates passed: falling back to
    /// grayscale there would throw away the feature over a missing accessor, and these
    /// values are what the vast majority of machines report anyway.
    fn default() -> Self {
        Self {
            enabled: true,
            bgr: false,
            gamma: 1.8,
            contrast: 0.5,
            cleartype_level: 1.0,
        }
    }
}

impl TextRenderingParams {
    /// Sub-pixel rendering disabled, everything else at its default.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Query the effective text-rendering parameters for the monitor showing `hwnd`.
///
/// **Call this only after the window exists.** One of the gates -- per-monitor DPI
/// awareness -- is a property the windowing library sets while creating its event loop,
/// so asking earlier reports the process default and always answers "disabled". That
/// mistake is silent: text simply comes out grayscale and nothing says why.
///
/// Pass `None` to use the primary monitor.
pub fn query(hwnd: Option<isize>) -> TextRenderingParams {
    if !system_wants_cleartype() {
        tracing::debug!("font smoothing is off or not ClearType; sub-pixel text disabled");
        return TextRenderingParams::disabled();
    }
    if !is_per_monitor_dpi_aware() {
        // DWM would stretch our output, smearing the sub-pixel fringes into visible
        // colour noise. Grayscale survives scaling; sub-pixel does not.
        tracing::debug!("not per-monitor DPI aware; sub-pixel text disabled");
        return TextRenderingParams::disabled();
    }
    let params = dwrite_params(hwnd).unwrap_or_default();
    tracing::debug!(?params, "sub-pixel text parameters");
    params
}

/// The monitor's blend parameters, **without** the validity gates.
///
/// Gamma, contrast and stripe order are properties of the display and the user's
/// calibration; they are meaningful whether or not sub-pixel rendering ends up being
/// used, and unlike [`query`] this is safe to call before a window exists. The renderer
/// needs them at construction time, and `enabled` on the result must be ignored.
pub fn blend_params(hwnd: Option<isize>) -> TextRenderingParams {
    dwrite_params(hwnd).unwrap_or_default()
}

/// Has the user asked for ClearType?
///
/// Two separate settings: font smoothing can be off entirely, and when on it can be
/// standard (grayscale) rather than ClearType. Both are honoured.
fn system_wants_cleartype() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETFONTSMOOTHING, SPI_GETFONTSMOOTHINGTYPE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        SystemParametersInfoW,
    };

    const FE_FONTSMOOTHINGCLEARTYPE: u32 = 0x0002;

    let mut smoothing = windows::core::BOOL(0);
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETFONTSMOOTHING,
            0,
            Some(std::ptr::from_mut(&mut smoothing).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() || !smoothing.as_bool() {
        return false;
    }

    let mut kind: u32 = 0;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETFONTSMOOTHINGTYPE,
            0,
            Some(std::ptr::from_mut(&mut kind).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_ok() && kind == FE_FONTSMOOTHINGCLEARTYPE
}

/// Is this process per-monitor DPI aware?
///
/// winit sets per-monitor-v2 during event-loop creation, so this normally passes -- but
/// "normally" is not "always" (an embedded host, a future manifest, a compatibility
/// shim), and the failure mode is ugly rather than obvious, so it is checked.
fn is_per_monitor_dpi_aware() -> bool {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT, DPI_AWARENESS_PER_MONITOR_AWARE,
        GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    };

    let ctx: DPI_AWARENESS_CONTEXT = unsafe { GetThreadDpiAwarenessContext() };
    let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(ctx) };
    awareness == DPI_AWARENESS_PER_MONITOR_AWARE
}

/// Ask DirectWrite for the monitor's rendering parameters.
///
/// This is the authoritative source: it fuses the system defaults, the registry values
/// and the user's per-monitor `cttune.exe` calibration. Reading the registry by hand would
/// reproduce a subset of that, badly.
fn dwrite_params(hwnd: Option<isize>) -> Option<TextRenderingParams> {
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_FACTORY_TYPE_SHARED, DWRITE_PIXEL_GEOMETRY_BGR, DWriteCreateFactory, IDWriteFactory,
        IDWriteRenderingParams,
    };
    use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromWindow};

    let factory: IDWriteFactory =
        unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ok()?;

    let monitor: HMONITOR = unsafe {
        MonitorFromWindow(
            HWND(hwnd.unwrap_or(0) as *mut std::ffi::c_void),
            MONITOR_DEFAULTTOPRIMARY,
        )
    };

    let params: IDWriteRenderingParams =
        unsafe { factory.CreateMonitorRenderingParams(monitor) }.ok()?;

    let gamma = unsafe { params.GetGamma() };
    let contrast = unsafe { params.GetEnhancedContrast() };
    let level = unsafe { params.GetClearTypeLevel() };
    let geometry = unsafe { params.GetPixelGeometry() };

    Some(TextRenderingParams {
        // A ClearType level of zero means the user asked for grayscale through the tuner
        // even though the system reports ClearType; honour it rather than drawing fringes
        // they explicitly turned off.
        enabled: level > 0.0,
        bgr: geometry == DWRITE_PIXEL_GEOMETRY_BGR,
        gamma,
        contrast,
        cleartype_level: level,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults must be the values Windows itself documents, because they are what a
    /// machine without DirectWrite falls back to.
    #[test]
    fn defaults_match_windows() {
        let d = TextRenderingParams::default();
        assert!(d.enabled);
        assert!(!d.bgr, "RGB is the usual stripe order");
        assert_eq!(d.gamma, 1.8);
        assert_eq!(d.contrast, 0.5);
        assert_eq!(d.cleartype_level, 1.0);
    }

    #[test]
    fn disabled_keeps_the_other_values_sane() {
        let d = TextRenderingParams::disabled();
        assert!(!d.enabled);
        // Even when disabled the blend parameters must stay usable: the renderer reads
        // them regardless, and a zero gamma would produce black text.
        assert!(d.gamma >= 1.0);
        assert!((0.0..=1.0).contains(&d.contrast));
    }

    /// `query` must never panic and must return usable values, whatever the machine
    /// reports. It runs against the real system, so it cannot assert a specific outcome --
    /// only that the result is well-formed.
    #[test]
    fn query_returns_usable_values_on_this_machine() {
        let p = query(None);
        assert!(p.gamma.is_finite() && p.gamma >= 1.0, "gamma {}", p.gamma);
        assert!(
            p.contrast.is_finite() && (0.0..=1.0).contains(&p.contrast),
            "contrast {}",
            p.contrast
        );
        assert!(
            p.cleartype_level.is_finite() && (0.0..=1.0).contains(&p.cleartype_level),
            "level {}",
            p.cleartype_level
        );
    }
}
