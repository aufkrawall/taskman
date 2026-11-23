//! tm-platform — OS-specific collectors and actions behind clean traits.

// Windows FFI needs `unsafe`, so it cannot be denied crate-wide; keep every
// unsafe operation inside an explicit block instead (the edition-2024 default,
// restated here so it cannot be lost to a future edition/toolchain change).
#![deny(unsafe_op_in_unsafe_fn)]

pub mod actions;
/// Freedesktop icon-theme lookup, shared by the Unix backends. Platform-
/// neutral on purpose so its tests run on every host.
pub mod xdg_icons;

/// Shell-free parsing for the Run dialog and typed Linux helper arguments.
#[cfg(any(target_os = "linux", test))]
mod command_line;
/// Parsing and construction for a future protected Linux helper.
#[cfg(any(target_os = "linux", test))]
mod linux_action_support;

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

/// Whether DWM currently cloaks the window (see
/// [`win::window_chrome::is_cloaked`]). Always `false` off Windows.
#[cfg(target_os = "windows")]
pub fn is_window_cloaked(hwnd: isize) -> bool {
    win::window_chrome::is_cloaked(hwnd)
}

#[cfg(not(target_os = "windows"))]
pub fn is_window_cloaked(_hwnd: isize) -> bool {
    false
}

/// Move/shrink the window back inside the work area of its (nearest)
/// monitor — see [`win::window_chrome::ensure_window_on_screen`]. Returns
/// whether it was moved.
#[cfg(target_os = "windows")]
pub fn ensure_window_on_screen(hwnd: isize) -> bool {
    win::window_chrome::ensure_window_on_screen(hwnd)
}

#[cfg(not(target_os = "windows"))]
pub fn ensure_window_on_screen(_hwnd: isize) -> bool {
    false
}

/// Outer position of a normal window in physical screen pixels.
#[cfg(target_os = "windows")]
pub fn window_outer_position_px(hwnd: isize) -> Option<[i32; 2]> {
    win::window_chrome::window_outer_position_px(hwnd)
}

#[cfg(not(target_os = "windows"))]
pub fn window_outer_position_px(_hwnd: isize) -> Option<[i32; 2]> {
    None
}

/// Move the window to a physical-pixel position without resizing it.
#[cfg(target_os = "windows")]
pub fn set_window_outer_position_px(hwnd: isize, pos: [i32; 2]) {
    win::window_chrome::set_window_outer_position_px(hwnd, pos);
}

#[cfg(not(target_os = "windows"))]
pub fn set_window_outer_position_px(_hwnd: isize, _pos: [i32; 2]) {}

/// Show, restore and activate `hwnd` without touching anyone else's input
/// queue. Safe to call from any thread; from a thread that does not own the
/// window every request is posted, never waited on.
#[cfg(target_os = "windows")]
pub fn force_foreground(hwnd: isize) {
    win::window_chrome::force_foreground(hwnd);
}

#[cfg(not(target_os = "windows"))]
pub fn force_foreground(_hwnd: isize) {}

/// Take the foreground for `hwnd` over fullscreen exclusive/borderless 3D
/// games by briefly merging input queues with the foreground thread. Only
/// activates: showing and restoring stay with the window's own thread.
///
/// **Only from a thread that pumps its own message queue.** While the queues
/// are merged the foreground application cannot process input either; see
/// `win::window_chrome::force_foreground_attached` for the full contract.
#[cfg(target_os = "windows")]
pub fn force_foreground_attached(hwnd: isize) {
    win::window_chrome::force_foreground_attached(hwnd);
}

#[cfg(not(target_os = "windows"))]
pub fn force_foreground_attached(_hwnd: isize) {}

/// Ensure `hwnd` owns keyboard focus when it is the system foreground window.
///
/// On Windows, activating a window from the tray, taskbar, or caption click does
/// not always dispatch `WM_SETFOCUS`, which can leave egui's viewport focus flag
/// (`i.focused`) false even while keystrokes arrive. Calling this when in foreground
/// synchronizes Win32 focus and returns whether the window is currently foreground.
#[cfg(target_os = "windows")]
pub fn sync_foreground_focus(hwnd: isize) -> bool {
    win::window_chrome::sync_foreground_focus(hwnd)
}

#[cfg(not(target_os = "windows"))]
pub fn sync_foreground_focus(_hwnd: isize) -> bool {
    false
}

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

/// Create an inert action backend for `--mock` mode.
pub fn create_mock_actions() -> Box<dyn actions::PlatformActions> {
    Box::new(actions::MockActions)
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

/// Windows taskbar performance monitor (`UpdateLayeredWindow` + software Direct2D).
/// Non-Windows hosts expose a zero-cost stub so callers stay cross-platform.
pub mod taskbar_monitor {
    #[cfg(not(target_os = "windows"))]
    use std::sync::Arc;
    use tm_core::settings::{Settings, TaskbarColorMode, TaskbarNetUnit};

    #[derive(Debug, Clone, PartialEq)]
    pub struct Config {
        pub enabled: bool,
        pub show_cpu: bool,
        pub show_ram: bool,
        pub show_network: bool,
        pub font_family: String,
        pub font_size: f32,
        pub net_unit_bits: bool,
        pub net_unit_mode: TaskbarNetUnit,
        pub color_mode: TaskbarColorMode,
        pub custom_color: [u8; 3],
        pub menu_open_label: String,
        pub menu_settings_label: String,
        pub menu_exit_label: String,
        pub menu_dark: bool,
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                enabled: false,
                show_cpu: true,
                show_ram: true,
                show_network: true,
                font_family: "Segoe UI".into(),
                font_size: 9.0,
                net_unit_bits: false,
                net_unit_mode: TaskbarNetUnit::Auto,
                color_mode: TaskbarColorMode::Auto,
                custom_color: [255, 255, 255],
                menu_open_label: "Task Manager".into(),
                menu_settings_label: "Settings".into(),
                menu_exit_label: "Exit".into(),
                menu_dark: true,
            }
        }
    }

    impl Config {
        pub fn from_settings(
            s: &Settings,
            menu_open_label: String,
            menu_settings_label: String,
            menu_exit_label: String,
            menu_dark: bool,
        ) -> Self {
            Self {
                enabled: s.taskbar_monitor_enabled,
                show_cpu: s.taskbar_show_cpu,
                show_ram: s.taskbar_show_ram,
                show_network: s.taskbar_show_network,
                font_family: s.taskbar_font_family.clone(),
                font_size: s.taskbar_font_size,
                net_unit_bits: s.taskbar_net_unit_bits,
                net_unit_mode: s.taskbar_net_unit_mode,
                color_mode: s.taskbar_color_mode,
                custom_color: s.taskbar_custom_color,
                menu_open_label,
                menu_settings_label,
                menu_exit_label,
                menu_dark,
            }
        }

        /// Compare against `Settings` and borrowed labels without allocating.
        pub fn matches_settings(
            &self,
            s: &Settings,
            menu_open_label: &str,
            menu_settings_label: &str,
            menu_exit_label: &str,
            menu_dark: bool,
        ) -> bool {
            self.enabled == s.taskbar_monitor_enabled
                && self.show_cpu == s.taskbar_show_cpu
                && self.show_ram == s.taskbar_show_ram
                && self.show_network == s.taskbar_show_network
                && self.font_family == s.taskbar_font_family
                && self.font_size == s.taskbar_font_size
                && self.net_unit_bits == s.taskbar_net_unit_bits
                && self.net_unit_mode == s.taskbar_net_unit_mode
                && self.color_mode == s.taskbar_color_mode
                && self.custom_color == s.taskbar_custom_color
                && self.menu_open_label == menu_open_label
                && self.menu_settings_label == menu_settings_label
                && self.menu_exit_label == menu_exit_label
                && self.menu_dark == menu_dark
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub struct Metrics {
        pub cpu_pct: Option<f32>,
        pub ram_pct: Option<f32>,
        pub net_sent_bps: Option<f64>,
        pub net_recv_bps: Option<f64>,
    }

    impl Metrics {
        pub fn from_snapshot(snap: &tm_core::model::Snapshot) -> Self {
            let ram_pct = if snap.memory.total_bytes > 0 {
                let pct =
                    (snap.memory.used_bytes as f64 / snap.memory.total_bytes as f64 * 100.0) as f32;
                pct.is_finite().then_some(pct)
            } else {
                None
            };
            let mut has_net = false;
            let mut sent = 0.0f64;
            let mut recv = 0.0f64;
            for n in &snap.networks {
                if n.kind == "Loopback" {
                    continue;
                }
                has_net = true;
                sent += n.sent_bps;
                recv += n.recv_bps;
            }
            Self {
                cpu_pct: (snap.cpu.utilization_pct_available
                    && snap.cpu.utilization_pct.is_finite())
                .then_some(snap.cpu.utilization_pct),
                ram_pct,
                net_sent_bps: (has_net && sent.is_finite()).then_some(sent),
                net_recv_bps: (has_net && recv.is_finite()).then_some(recv),
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Action {
        OpenMain,
        OpenSettings,
        Exit,
    }

    #[cfg(target_os = "windows")]
    pub use crate::win::taskbar_monitor::TaskbarMonitorHandle;

    #[cfg(not(target_os = "windows"))]
    pub struct TaskbarMonitorHandle;

    #[cfg(not(target_os = "windows"))]
    impl TaskbarMonitorHandle {
        pub fn new(_callback: Arc<dyn Fn(Action) + Send + Sync>) -> Self {
            Self
        }
        pub fn new_with_visibility_notifier(
            _callback: Arc<dyn Fn(Action) + Send + Sync>,
            _visibility_notifier: Arc<dyn Fn() + Send + Sync>,
        ) -> Self {
            Self
        }
        pub fn is_visible(&self) -> bool {
            false
        }
        pub fn set_config(&self, _config: Config) {}
        pub fn update_metrics(&self, _metrics: Metrics) {}
        pub fn request_refresh(&self) {}
        pub fn shutdown(&self) {}
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tm_core::model::{NetworkInfo, Snapshot};

        #[test]
        fn metrics_from_snapshot_excludes_loopback_and_handles_missing_ram() {
            let mut snap = Snapshot::default();
            snap.cpu.utilization_pct_available = true;
            snap.cpu.utilization_pct = 24.5;
            snap.memory.total_bytes = 0;
            snap.networks.push(NetworkInfo {
                name: "lo".into(),
                kind: "Loopback".into(),
                recv_bps: 99_999.0,
                sent_bps: 99_999.0,
                ..Default::default()
            });

            let m = Metrics::from_snapshot(&snap);
            assert_eq!(m.cpu_pct, Some(24.5));
            assert_eq!(m.ram_pct, None);
            assert_eq!(m.net_recv_bps, None);
            assert_eq!(m.net_sent_bps, None);

            snap.memory.total_bytes = 1000;
            snap.memory.used_bytes = 400;
            snap.networks.push(NetworkInfo {
                name: "eth0".into(),
                kind: "Ethernet".into(),
                recv_bps: 1200.0,
                sent_bps: 300.0,
                ..Default::default()
            });
            snap.networks.push(NetworkInfo {
                name: "wlan0".into(),
                kind: "Wi-Fi".into(),
                recv_bps: 800.0,
                sent_bps: 200.0,
                ..Default::default()
            });

            let m2 = Metrics::from_snapshot(&snap);
            assert_eq!(m2.ram_pct, Some(40.0));
            assert_eq!(m2.net_recv_bps, Some(2000.0));
            assert_eq!(m2.net_sent_bps, Some(500.0));

            snap.cpu.utilization_pct = f32::NAN;
            snap.networks[1].recv_bps = f64::NAN;
            let m3 = Metrics::from_snapshot(&snap);
            assert_eq!(m3.cpu_pct, None);
            assert_eq!(m3.net_recv_bps, None);
            assert_eq!(m3.net_sent_bps, Some(500.0));
        }
    }
}
