//! Windows backend.

mod cpu_info;
mod gpu;
mod perfcounters;
mod process_ops;
mod sampler;
mod services;
mod startup;
mod threads_map;
pub(crate) mod users;
mod windows_enum;

use crate::actions::*;
use tm_core::engine::SystemCollector;
use tm_core::error::Result;
use tm_core::model::*;

/// Collector combining sysinfo sampling with Win32/PDH extras.
pub struct WinCollector {
    inner: sampler::Sampler,
}

impl SystemCollector for WinCollector {
    fn sample(&mut self, now: std::time::Instant) -> Result<Snapshot> {
        self.inner.sample(now)
    }

    fn backend_name(&self) -> &'static str {
        "windows-sysinfo+pdh"
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
