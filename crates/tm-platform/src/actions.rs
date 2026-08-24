//! Platform action surface: services, startup apps, users, process control.
//!
//! Every method has a default "unsupported" implementation so non-Windows
//! platforms only override what they can honestly deliver. The UI disables
//! controls based on `capabilities()`.

use tm_core::error::Result;
use tm_core::model::{
    ServiceInfo, StartupItem, UserSession, PriorityClass, ProcStatus,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    pub services_list: bool,
    pub services_control: bool,
    pub startup_toggle: bool,
    pub users_sessions: bool,
    pub user_disconnect: bool,
    pub end_process: bool,
    pub suspend_resume: bool,
    pub set_priority: bool,
    pub set_affinity: bool,
    pub efficiency_mode: bool,
    pub run_new_task: bool,
    pub per_process_network: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserSessionAction {
    Disconnect,
    Logoff,
}

/// Extra per-process details fetched lazily (not part of the 1 Hz snapshot).
#[derive(Debug, Clone, Default)]
pub struct ProcessExtra {
    pub status: Option<ProcStatus>,
    pub elevated: Option<bool>,
    pub wow64: Option<bool>,
}

pub trait PlatformActions: Send + Sync {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    // ------------------------------------------------ services
    fn list_services(&self) -> Result<Vec<ServiceInfo>> {
        Err(tm_core::TmError::Unsupported("services"))
    }
    fn control_service(&self, _name: &str, _action: ServiceAction) -> Result<()> {
        Err(tm_core::TmError::Unsupported("service control"))
    }

    // ------------------------------------------------ startup
    fn list_startup(&self) -> Result<Vec<StartupItem>> {
        Err(tm_core::TmError::Unsupported("startup apps"))
    }
    fn set_startup_enabled(&self, _item_id: &str, _location: &str, _enabled: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("startup toggle"))
    }

    // ------------------------------------------------ users
    fn list_user_sessions(&self) -> Result<Vec<UserSession>> {
        Err(tm_core::TmError::Unsupported("user sessions"))
    }
    fn control_user_session(&self, _session_id: u32, _action: UserSessionAction) -> Result<()> {
        Err(tm_core::TmError::Unsupported("session control"))
    }

    // ------------------------------------------------ process control
    /// Terminate a process; `tree=true` also kills descendants.
    fn kill_process(&self, pid: u32, tree: bool) -> Result<()> {
        let _ = tree;
        self.kill_single(pid)
    }
    fn kill_single(&self, pid: u32) -> Result<()> {
        let _ = pid;
        Err(tm_core::TmError::Unsupported("end task"))
    }
    fn suspend_process(&self, _pid: u32, _suspend: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("suspend/resume"))
    }
    fn set_priority(&self, _pid: u32, _priority: PriorityClass) -> Result<()> {
        Err(tm_core::TmError::Unsupported("set priority"))
    }
    fn get_affinity_mask(&self, _pid: u32) -> Result<u64> {
        Err(tm_core::TmError::Unsupported("affinity"))
    }
    fn system_affinity_mask(&self) -> Result<u64> {
        Err(tm_core::TmError::Unsupported("affinity"))
    }
    fn set_affinity_mask(&self, _pid: u32, _mask: u64) -> Result<()> {
        Err(tm_core::TmError::Unsupported("affinity"))
    }
    fn set_efficiency_mode(&self, _pid: u32, _on: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("efficiency mode"))
    }

    // ------------------------------------------------ launching / elevation
    fn is_elevated(&self) -> bool {
        false
    }
    /// Launch `command_line`; optionally request elevation via OS dialog.
    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        let _ = (command_line, elevate);
        Err(tm_core::TmError::Unsupported("run new task"))
    }
    /// Restart this app with elevation (UAC prompt on Windows).
    fn relaunch_elevated(&self) -> Result<()> {
        Err(tm_core::TmError::Unsupported("elevation"))
    }

    /// Human-readable name of the platform backend.
    fn backend_name(&self) -> &'static str {
        "unknown"
    }
}
