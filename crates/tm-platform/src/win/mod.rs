//! Windows backend.

mod cpu_info;
pub(crate) mod cpu_load;
mod gpu;
pub mod icons;
pub mod locale;
mod perfcounters;
mod process_ops;
mod sampler;
mod services;
mod startup;
mod threads_map;
pub(crate) mod users;
mod version;
mod windows_enum;

use crate::actions::*;
use tm_core::engine::SystemCollector;
use tm_core::error::Result;
use tm_core::model::*;

/// Firmware POST duration of the last boot in ms — the same value the Task
/// Manager shows as "Letzte BIOS-Zeit". Registry: FwPOSTTime under Session
/// Manager\Power (0/missing = unknown).
pub fn last_bios_time_ms() -> Option<u64> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
    let sub: Vec<u16> = "SYSTEM\x5cCurrentControlSet\x5cControl\x5cSession Manager\x5cPower"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value: Vec<u16> = "FwPOSTTime"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            windows::core::PCWSTR(sub.as_ptr()),
            windows::core::PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as _),
            Some(&mut size),
        )
    };
    if rc.is_err() || data == 0 {
        None
    } else {
        Some(data as u64)
    }
}

/// Refresh rate of the primary display's current mode, from the active
/// DEVMODE (`EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)`). Values below 20
/// Hz are treated as "driver reports nonsense" (0/1 are seen on some
/// virtual display adapters) and mapped to None.
pub fn display_refresh_hz() -> Option<f32> {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let ok = unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool();
    if !ok || dm.dmDisplayFrequency < 20 {
        return None;
    }
    Some(dm.dmDisplayFrequency as f32)
}

/// Collector combining sysinfo sampling with Win32/PDH extras.
pub struct WinCollector {
    inner: sampler::Sampler,
}

impl SystemCollector for WinCollector {
    fn sample(&mut self, now: std::time::Instant) -> Result<Snapshot> {
        self.inner.sample(now)
    }

    fn backend_name(&self) -> &'static str {
        "windows/sysinfo+pdh+nt-cpu"
    }
}

/// Windows action surface.
#[derive(Default)]
pub struct WinActions;

impl PlatformActions for WinActions {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            services_list: true,
            services_control: true,
            startup_toggle: true,
            users_sessions: true,
            user_disconnect: true,
            end_process: true,
            suspend_resume: true,
            set_priority: true,
            set_affinity: true,
            efficiency_mode: true,
            run_new_task: true,
            per_process_network: false, // would require ETW
        }
    }

    fn list_services(&self) -> Result<Vec<ServiceInfo>> {
        services::list_services()
    }
    fn control_service(&self, name: &str, action: ServiceAction) -> Result<()> {
        services::control_service(name, action)
    }

    fn list_startup(&self) -> Result<Vec<StartupItem>> {
        Ok(startup::list_startup())
    }
    fn set_startup_enabled(&self, item_id: &str, location: &str, enabled: bool) -> Result<()> {
        startup::set_startup_enabled(item_id, location, enabled)
    }

    fn list_user_sessions(&self) -> Result<Vec<UserSession>> {
        users::list_sessions()
    }
    fn control_user_session(&self, session_id: u32, action: UserSessionAction) -> Result<()> {
        users::control_session(session_id, action)
    }

    fn kill_process(&self, pid: u32, tree: bool) -> Result<()> {
        process_ops::kill_process(pid, tree)
    }
    fn kill_single(&self, pid: u32) -> Result<()> {
        process_ops::kill_single(pid)
    }
    fn suspend_process(&self, pid: u32, suspend: bool) -> Result<()> {
        process_ops::suspend_process(pid, suspend)
    }
    fn set_priority(&self, pid: u32, priority: PriorityClass) -> Result<()> {
        process_ops::set_priority(pid, priority)
    }
    fn get_affinity_mask(&self, pid: u32) -> Result<u64> {
        process_ops::get_affinity(pid)
    }
    fn system_affinity_mask(&self) -> Result<u64> {
        process_ops::system_affinity()
    }
    fn set_affinity_mask(&self, pid: u32, mask: u64) -> Result<()> {
        process_ops::set_affinity(pid, mask)
    }
    fn set_efficiency_mode(&self, pid: u32, on: bool) -> Result<()> {
        process_ops::set_efficiency_mode(pid, on)
    }

    fn is_elevated(&self) -> bool {
        process_ops::is_elevated()
    }
    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        process_ops::run_new_task(command_line, elevate)
    }
    fn relaunch_elevated(&self) -> Result<()> {
        process_ops::relaunch_elevated()
    }
    fn create_dump_file(&self, pid: u32, path: &std::path::Path) -> Result<()> {
        process_ops::create_dump_file(pid, path)
    }
    fn open_file_location(&self, path: &str) -> Result<()> {
        process_ops::open_file_location(path)
    }
    fn open_properties(&self, path: &str) -> Result<()> {
        process_ops::open_properties(path)
    }
    fn open_url(&self, url: &str) -> Result<()> {
        process_ops::open_url(url)
    }

    fn last_bios_time_ms(&self) -> Option<u64> {
        last_bios_time_ms()
    }

    fn process_icon_rgba(&self, exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
        icons::extract_icon_rgba(exe_path).map(|ic| (ic.width, ic.height, ic.rgba))
    }

    fn backend_name(&self) -> &'static str {
        "win32"
    }
}

pub fn create() -> (WinCollector, WinActions) {
    (
        WinCollector {
            inner: sampler::Sampler::new(),
        },
        WinActions,
    )
}
