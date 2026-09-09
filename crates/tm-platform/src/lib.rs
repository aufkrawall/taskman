//! tm-platform — OS-specific collectors and actions behind clean traits.

// Windows FFI needs `unsafe`, so it cannot be denied crate-wide; keep every
// unsafe operation inside an explicit block instead (the edition-2024 default,
// restated here so it cannot be lost to a future edition/toolchain change).
#![deny(unsafe_op_in_unsafe_fn)]

pub mod actions;

#[cfg(target_os = "windows")]
pub mod win;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

/// OS locale detection: number/date formatting rules from regional settings.
/// Windows queries the NLS APIs; other platforms fall back to env vars.
pub fn detect_locale() -> tm_core::locale::LocaleFmt {
    #[cfg(target_os = "windows")]
    {
        win::locale::detect_locale()
            .unwrap_or_else(|| tm_core::locale::detect_env().unwrap_or_default())
    }
    #[cfg(not(target_os = "windows"))]
    {
        tm_core::locale::detect_env().unwrap_or_default()
    }
}

/// Refresh rate of the primary display's current mode in Hz.
///
/// Reads the active DEVMODE via `EnumDisplaySettingsW`; this is what vsync
/// (FIFO present) actually paces frames to, so it is the honest ceiling for
/// frame-rate diagnostics. Returns None when the driver reports nothing
/// useful (some virtual/adapters report 0 or 1).
#[cfg(target_os = "windows")]
pub fn display_refresh_hz() -> Option<f32> {
    win::display_refresh_hz()
}

#[cfg(not(target_os = "windows"))]
pub fn display_refresh_hz() -> Option<f32> {
    None
}

/// Paint the native window caption so it matches what the app draws directly
/// below it, and follow the user's Windows transparency preference.
///
/// `hwnd` is the raw window handle; a non-Windows host ignores it. Call only
/// when the appearance actually changes — every attribute recomposes the
/// window frame.
#[cfg(target_os = "windows")]
pub fn apply_title_bar(hwnd: isize, caption: [u8; 3], text: [u8; 3], border: [u8; 3], dark: bool) {
    win::window_chrome::apply(
        hwnd,
        win::window_chrome::TitleBar {
            caption,
            text,
            border,
            dark,
            backdrop: win::window_chrome::transparency_effects_enabled(),
        },
    );
}

#[cfg(not(target_os = "windows"))]
pub fn apply_title_bar(
    _hwnd: isize,
    _caption: [u8; 3],
    _text: [u8; 3],
    _border: [u8; 3],
    _dark: bool,
) {
}

/// Make the Windows always-on-top setting win against shell-owned topmost
/// surfaces too. Non-Windows hosts keep using the toolkit's window level.
#[cfg(target_os = "windows")]
pub fn set_strict_topmost(hwnd: isize, enabled: bool) {
    win::window_chrome::set_strict_topmost(hwnd, enabled);
}

#[cfg(not(target_os = "windows"))]
pub fn set_strict_topmost(_hwnd: isize, _enabled: bool) {}

/// Create the main window in native Task Manager's window band (16), which is
/// above the Start menu. Must be called before the GUI window exists; the
/// patched winit reads the band at creation time.
#[cfg(target_os = "windows")]
pub fn request_topmost_band() {
    win::window_chrome::request_topmost_band();
}

#[cfg(not(target_os = "windows"))]
pub fn request_topmost_band() {}

/// Stop advertising the window band to child processes (call once the window
/// exists).
#[cfg(target_os = "windows")]
pub fn clear_topmost_band() {
    win::window_chrome::clear_topmost_band();
}

#[cfg(not(target_os = "windows"))]
pub fn clear_topmost_band() {}

/// Hide or reveal a window at the compositor — see
/// [`win::window_chrome::set_cloaked`]. Non-Windows hosts ignore it.
#[cfg(target_os = "windows")]
pub fn set_window_cloaked(hwnd: isize, cloaked: bool) {
    win::window_chrome::set_cloaked(hwnd, cloaked);
}

#[cfg(not(target_os = "windows"))]
pub fn set_window_cloaked(_hwnd: isize, _cloaked: bool) {}

/// Build only the platform action surface (process control, services,
/// startup apps, ...).
///
/// This is deliberately separate from [`create_collector`]: constructing
/// actions must never pay for sampler construction (sysinfo warmup, CPU
/// topology probing, SMBIOS parsing). GUI startup uses this and defers the
/// collector to the engine thread.
pub fn create_actions() -> Box<dyn actions::PlatformActions> {
    #[cfg(target_os = "windows")]
    {
        Box::new(win::create_actions())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::create_actions())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::create_actions())
    }
}

/// Build only the system collector.
pub fn create_collector() -> Box<dyn tm_core::engine::SystemCollector> {
    #[cfg(target_os = "windows")]
    {
        Box::new(win::create_collector())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::create_collector())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::create_collector())
    }
}

/// Sub-pixel (`ClearType`) text-rendering parameters for the current display.
///
/// Windows reports the user's font-smoothing choice and their per-monitor `cttune.exe`
/// calibration; every other platform reports "off" for now. Sub-pixel rendering is only
/// correct when our pixels map one-to-one onto the panel's, so this is a capability query
/// rather than a preference -- see `win::text_rendering` for the gates it applies.
pub mod text_rendering {
    /// Plain data, deliberately free of any platform type so the renderer and the egui
    /// fork stay platform-independent.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Params {
        pub enabled: bool,
        pub bgr: bool,
        pub gamma: f32,
        pub contrast: f32,
        pub cleartype_level: f32,
    }

    impl Default for Params {
        fn default() -> Self {
            Self {
                enabled: false,
                bgr: false,
                gamma: 1.8,
                contrast: 0.5,
                cleartype_level: 1.0,
            }
        }
    }

    /// Query the display showing `hwnd` (or the primary display when `None`).
    ///
    /// **Only valid once a window exists** -- one of the gates depends on DPI awareness,
    /// which winit sets while building its event loop.
    #[cfg(target_os = "windows")]
    pub fn query(hwnd: Option<isize>) -> Params {
        convert(crate::win::text_rendering::query(hwnd))
    }

    /// Blend parameters only, without the validity gates. Safe to call before a window
    /// exists; ignore `enabled` on the result.
    #[cfg(target_os = "windows")]
    pub fn blend_params(hwnd: Option<isize>) -> Params {
        convert(crate::win::text_rendering::blend_params(hwnd))
    }

    #[cfg(target_os = "windows")]
    fn convert(p: crate::win::text_rendering::TextRenderingParams) -> Params {
        Params {
            enabled: p.enabled,
            bgr: p.bgr,
            gamma: p.gamma,
            contrast: p.contrast,
            cleartype_level: p.cleartype_level,
        }
    }

    /// Linux: follow fontconfig's `rgba` sub-pixel order.
    ///
    /// `fc-match -v` reports the desktop's declared stripe order as an integer:
    /// 0 unknown, 1 rgb, 2 bgr, 3 vrgb, 4 vbgr, 5 none. Only the horizontal
    /// orders can be rendered; everything else (including an absent property,
    /// which is what a fractional-scaling compositor that resamples fringes
    /// wants) stays grayscale. This is the same signal GTK/Qt consume.
    #[cfg(target_os = "linux")]
    pub fn query(_hwnd: Option<isize>) -> Params {
        fontconfig_subpixel()
    }

    /// Linux: the same fontconfig blend signal; safe before a window exists.
    #[cfg(target_os = "linux")]
    pub fn blend_params(_hwnd: Option<isize>) -> Params {
        fontconfig_subpixel()
    }

    #[cfg(target_os = "linux")]
    fn fontconfig_subpixel() -> Params {
        let mut params = Params::default();
        let Ok(out) = std::process::Command::new("fc-match")
            .args(["-v", "sans-serif"])
            .output()
        else {
            return params;
        };
        if !out.status.success() {
            return params;
        }
        if let Some(order) = parse_fontconfig_rgba(&String::from_utf8_lossy(&out.stdout)) {
            match order {
                1 => {
                    params.enabled = true;
                    params.bgr = false;
                }
                2 => {
                    params.enabled = true;
                    params.bgr = true;
                }
                _ => {}
            }
        }
        params
    }

    /// First `rgba:` value from `fc-match -v` output, if present.
    #[cfg(any(target_os = "linux", test))]
    fn parse_fontconfig_rgba(text: &str) -> Option<u8> {
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("rgba:") else {
                continue;
            };
            return rest
                .trim()
                .split(['(', ' '])
                .next()
                .and_then(|value| value.parse::<u8>().ok());
        }
        None
    }

    /// Other platforms: sub-pixel rendering is off.
    ///
    /// The rasterizer and blend path are platform-independent, so enabling
    /// this elsewhere is a matter of finding the equivalent signal. Guessing
    /// wrong looks worse than grayscale.
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    pub fn query(_hwnd: Option<isize>) -> Params {
        Params::default()
    }

    /// Other platforms: the neutral defaults.
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    pub fn blend_params(_hwnd: Option<isize>) -> Params {
        Params::default()
    }

    #[cfg(test)]
    mod tests {
        use super::parse_fontconfig_rgba;

        #[test]
        fn fontconfig_rgba_orders_map_to_subpixel_modes() {
            assert_eq!(parse_fontconfig_rgba("rgba: 1(i)(w)\n"), Some(1));
            assert_eq!(parse_fontconfig_rgba("rgba: 2(i)(w)\n"), Some(2));
            assert_eq!(parse_fontconfig_rgba("rgba: 5(i)(w)\n"), Some(5));
            assert_eq!(parse_fontconfig_rgba("antialias: True(w)\n"), None);
            assert_eq!(parse_fontconfig_rgba(""), None);
        }
    }
}
