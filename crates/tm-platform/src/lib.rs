//! tm-platform — OS-specific collectors and actions behind clean traits.

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
