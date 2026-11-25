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

use windows::Win32::Foundation::{HWND, RECT};

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
    /// Windows' documented blend defaults, with sub-pixel rendering **on**.
    ///
    /// This is sample data for tests and neutral blend input. [`query`] must fail closed
    /// when DirectWrite cannot name the monitor's actual parameters and stripe order.
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
    if is_remote_session() {
        // Remote Desktop compresses and may rescale the framebuffer for an unknown client
        // panel geometry, turning sub-pixel fringes into colour noise.
        tracing::debug!("remote desktop session active; sub-pixel text disabled");
        return TextRenderingParams::disabled();
    }
    if !is_per_monitor_dpi_aware() {
        // DWM would stretch our output, smearing the sub-pixel fringes into visible
        // colour noise. Grayscale survives scaling; sub-pixel does not.
        tracing::debug!("not per-monitor DPI aware; sub-pixel text disabled");
        return TextRenderingParams::disabled();
    }
    let params = match hwnd {
        Some(hwnd) => query_window_monitors(hwnd).unwrap_or_else(TextRenderingParams::disabled),
        None => dwrite_params(None)
            .and_then(validated_params)
            .unwrap_or_else(TextRenderingParams::disabled),
    };
    tracing::debug!(?params, "sub-pixel text parameters");
    params
}

/// Cheap identity of the window's current monitor placement.
pub fn window_placement_key(hwnd: isize) -> Option<crate::text_rendering::WindowPlacementKey> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let window = HWND(hwnd as *mut std::ffi::c_void);
    if window.0.is_null() {
        return None;
    }
    let mut window_rect = RECT::default();
    unsafe { GetWindowRect(window, &mut window_rect) }.ok()?;
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_invalid() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    Some(crate::text_rendering::WindowPlacementKey {
        window_rect: [
            window_rect.left,
            window_rect.top,
            window_rect.right,
            window_rect.bottom,
        ],
        monitor: monitor.0 as isize,
        monitor_rect: [
            info.rcMonitor.left,
            info.rcMonitor.top,
            info.rcMonitor.right,
            info.rcMonitor.bottom,
        ],
        effective_dpi: unsafe { GetDpiForWindow(window) },
    })
}

/// Query every monitor intersecting the window. Any inability to enumerate or read one
/// intersecting monitor returns `None`; the caller must then keep LCD disabled.
fn query_window_monitors(hwnd: isize) -> Option<TextRenderingParams> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    use windows::core::BOOL;

    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _dc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `query_window_monitors` passes a pointer to a live Vec that outlives
        // this synchronous callback.
        let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        monitors.push(monitor);
        BOOL(1)
    }

    let window = HWND(hwnd as *mut std::ffi::c_void);
    let mut window_rect = RECT::default();
    if window.0.is_null() || unsafe { GetWindowRect(window, &mut window_rect) }.is_err() {
        return None;
    }
    let window_rect = [
        window_rect.left,
        window_rect.top,
        window_rect.right,
        window_rect.bottom,
    ];

    let mut monitors = Vec::new();
    let enumerated = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM((&raw mut monitors) as isize),
        )
    };
    if !enumerated.as_bool() {
        return None;
    }
    let mut intersecting = Vec::new();
    for monitor in monitors {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            return None;
        }
        if rects_intersect(
            window_rect,
            [
                info.rcMonitor.left,
                info.rcMonitor.top,
                info.rcMonitor.right,
                info.rcMonitor.bottom,
            ],
        ) {
            intersecting.push(monitor);
        }
    }
    if intersecting.is_empty() {
        return None;
    }

    let factory = dwrite_factory();
    let params: Vec<_> = intersecting
        .iter()
        .map(|monitor| {
            factory
                .as_ref()
                .and_then(|factory| dwrite_params_for_monitor(factory, *monitor))
                .and_then(validated_params)
        })
        .collect();
    let policy = monitor_policy(&params);
    if params.len() > 1 && params.iter().any(|candidate| *candidate != params[0]) {
        tracing::debug!(
            monitor_count = params.len(),
            "window intersects monitors with conflicting text-rendering policies; LCD disabled"
        );
    }
    Some(policy)
}

fn rects_intersect(a: [i32; 4], b: [i32; 4]) -> bool {
    a[0] < b[2] && a[2] > b[0] && a[1] < b[3] && a[3] > b[1]
}

fn validated_params(params: TextRenderingParams) -> Option<TextRenderingParams> {
    (params.gamma.is_finite()
        && params.gamma >= 1.0
        && params.contrast.is_finite()
        && (0.0..=1.0).contains(&params.contrast)
        && params.cleartype_level.is_finite()
        && (0.0..=1.0).contains(&params.cleartype_level))
    .then_some(params)
}

/// Combine the validated policies of every monitor intersecting the window.
///
/// A failed individual DirectWrite query is unknown rather than an implicit RGB panel, so
/// even a single monitor fails closed. Unknown or disagreeing multi-monitor data also fails
/// closed.
fn monitor_policy(monitors: &[Option<TextRenderingParams>]) -> TextRenderingParams {
    match monitors {
        [] => TextRenderingParams::disabled(),
        [only] => only.unwrap_or_else(TextRenderingParams::disabled),
        [first, rest @ ..] => {
            let Some(first) = first else {
                return TextRenderingParams::disabled();
            };
            if rest.iter().all(|candidate| *candidate == Some(*first)) {
                *first
            } else {
                TextRenderingParams::disabled()
            }
        }
    }
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

/// Is this session being remoted over RDP?
fn is_remote_session() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};

    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
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
    use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromWindow};

    let factory = dwrite_factory()?;

    let monitor: HMONITOR = unsafe {
        MonitorFromWindow(
            HWND(hwnd.unwrap_or(0) as *mut std::ffi::c_void),
            MONITOR_DEFAULTTOPRIMARY,
        )
    };
    dwrite_params_for_monitor(&factory, monitor)
}

fn dwrite_factory() -> Option<windows::Win32::Graphics::DirectWrite::IDWriteFactory> {
    use windows::Win32::Graphics::DirectWrite::{DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory};

    unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ok()
}

fn orientation_supports_horizontal_lcd(
    orientation: windows::Win32::Graphics::Gdi::DEVMODE_DISPLAY_ORIENTATION,
) -> bool {
    use windows::Win32::Graphics::Gdi::{DMDO_90, DMDO_270};

    orientation != DMDO_90 && orientation != DMDO_270
}

fn monitor_supports_horizontal_lcd(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> bool {
    use windows::Win32::Graphics::Gdi::{
        DEVMODEW, DM_DISPLAYORIENTATION, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
        GetMonitorInfoW, MONITORINFO, MONITORINFOEXW,
    };
    use windows::core::PCWSTR;

    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) }.as_bool() {
        return true;
    }

    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    if !unsafe {
        EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
        )
    }
    .as_bool()
    {
        return true;
    }

    if dm.dmFields.contains(DM_DISPLAYORIENTATION) {
        let orientation = unsafe { dm.Anonymous1.Anonymous2.dmDisplayOrientation };
        return orientation_supports_horizontal_lcd(orientation);
    }
    true
}

fn dwrite_params_for_monitor(
    factory: &windows::Win32::Graphics::DirectWrite::IDWriteFactory,
    monitor: windows::Win32::Graphics::Gdi::HMONITOR,
) -> Option<TextRenderingParams> {
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_PIXEL_GEOMETRY_BGR, DWRITE_PIXEL_GEOMETRY_FLAT, IDWriteRenderingParams,
    };

    let params: IDWriteRenderingParams =
        unsafe { factory.CreateMonitorRenderingParams(monitor) }.ok()?;

    let gamma = unsafe { params.GetGamma() };
    let contrast = unsafe { params.GetEnhancedContrast() };
    let level = unsafe { params.GetClearTypeLevel() };
    let geometry = unsafe { params.GetPixelGeometry() };
    let horizontal_panel = monitor_supports_horizontal_lcd(monitor);

    Some(TextRenderingParams {
        // A ClearType level of zero means the user asked for grayscale through the tuner
        // even though the system reports ClearType; honour it rather than drawing fringes
        // they explicitly turned off. A FLAT pixel geometry or 90/270-degree portrait
        // rotation means the display has no usable horizontal RGB/BGR stripes, so draw
        // grayscale instead of coloured fringes.
        enabled: level > 0.0 && geometry != DWRITE_PIXEL_GEOMETRY_FLAT && horizontal_panel,
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

    #[test]
    fn monitor_policy_keeps_lcd_when_every_intersecting_monitor_agrees() {
        let rgb = TextRenderingParams::default();
        assert_eq!(monitor_policy(&[Some(rgb), Some(rgb)]), rgb);

        let bgr = TextRenderingParams {
            bgr: true,
            gamma: 2.2,
            contrast: 0.7,
            ..rgb
        };
        assert_eq!(monitor_policy(&[Some(bgr), Some(bgr)]), bgr);
    }

    #[test]
    fn monitor_policy_fails_closed_on_conflicts_and_unknown_multi_monitor_data() {
        let rgb = TextRenderingParams::default();
        let bgr = TextRenderingParams { bgr: true, ..rgb };
        let warm = TextRenderingParams { gamma: 2.2, ..rgb };
        let disabled = TextRenderingParams::disabled();

        for policies in [
            [Some(rgb), Some(bgr)],
            [Some(rgb), Some(warm)],
            [Some(rgb), Some(disabled)],
            [Some(rgb), None],
            [None, Some(rgb)],
        ] {
            assert!(
                !monitor_policy(&policies).enabled,
                "conflicting or unknown monitor policy must disable LCD: {policies:?}"
            );
        }
    }

    #[test]
    fn unavailable_monitor_queries_fail_closed_even_on_one_monitor() {
        assert!(!monitor_policy(&[None]).enabled);
        assert!(!monitor_policy(&[]).enabled);
    }

    #[test]
    fn malformed_monitor_parameters_are_treated_as_unavailable() {
        for malformed in [
            TextRenderingParams {
                gamma: 0.0,
                ..TextRenderingParams::default()
            },
            TextRenderingParams {
                contrast: f32::NAN,
                ..TextRenderingParams::default()
            },
            TextRenderingParams {
                cleartype_level: 2.0,
                ..TextRenderingParams::default()
            },
        ] {
            assert_eq!(validated_params(malformed), None);
        }
        assert_eq!(
            validated_params(TextRenderingParams::default()),
            Some(TextRenderingParams::default())
        );
    }

    #[test]
    fn portrait_display_orientations_disable_horizontal_lcd() {
        use windows::Win32::Graphics::Gdi::{DMDO_90, DMDO_180, DMDO_270, DMDO_DEFAULT};

        assert!(orientation_supports_horizontal_lcd(DMDO_DEFAULT));
        assert!(orientation_supports_horizontal_lcd(DMDO_180));
        assert!(!orientation_supports_horizontal_lcd(DMDO_90));
        assert!(!orientation_supports_horizontal_lcd(DMDO_270));
    }
}
