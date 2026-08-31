//! Platform action surface: services, startup apps, users, process control.
//!
//! Every method has a default "unsupported" implementation so non-Windows
//! platforms only override what they can honestly deliver. The UI disables
//! controls based on `capabilities()`.

use tm_core::error::Result;
use tm_core::model::{PriorityClass, ProcStatus, ServiceInfo, StartupItem, UserSession};

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
    pub process_modules: bool,
    pub unload_module: bool,
    pub start_with_windows: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub enum TaskManagerReplacementState {
    Unsupported,
    Disabled,
    Enabled,
    Stale(String),
    Conflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreServiceState {
    Unsupported,
    NotInstalled,
    Stopped,
    Starting,
    Running {
        version: String,
    },
    Degraded(String),
    /// The service is installed and healthy, but this session's executable
    /// sits outside the protected install location, so the broker's
    /// image-path authorization will keep rejecting it. "Repair" cannot fix
    /// this; the session must switch to the installed GUI.
    ForeignClient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
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

/// One executable image mapped into a process. Module enumeration is an
/// explicit, on-demand diagnostic query rather than part of the hot sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessModule {
    pub name: String,
    pub path: String,
    pub base_address: u64,
    pub size_bytes: u64,
    /// False for the process image and loader-critical Windows DLLs.
    pub unloadable: bool,
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
    /// Terminate a process; `tree=true` also kills descendants. When
    /// `expected_start_epoch_s` is `Some`, platforms that CAN verify process
    /// identity must refuse to act on a recycled pid (creation-time check at
    /// kill time). Platforms without such verification ignore the hint —
    /// the UI-level snapshot check still applies there.
    fn kill_process(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        tree: bool,
    ) -> Result<()> {
        let _ = (expected_start_epoch_s, tree);
        self.kill_single(pid)
    }
    fn kill_single(&self, pid: u32) -> Result<()> {
        let _ = pid;
        Err(tm_core::TmError::Unsupported("end task"))
    }
    fn suspend_process(&self, _pid: u32, _suspend: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("suspend/resume"))
    }
    fn suspend_process_checked(
        &self,
        pid: u32,
        _expected_start_epoch_s: Option<i64>,
        suspend: bool,
    ) -> Result<()> {
        self.suspend_process(pid, suspend)
    }
    fn set_priority(&self, _pid: u32, _priority: PriorityClass) -> Result<()> {
        Err(tm_core::TmError::Unsupported("set priority"))
    }
    fn set_priority_checked(
        &self,
        pid: u32,
        _expected_start_epoch_s: Option<i64>,
        priority: PriorityClass,
    ) -> Result<()> {
        self.set_priority(pid, priority)
    }
    fn get_affinity_mask(&self, _pid: u32) -> Result<u64> {
        Err(tm_core::TmError::Unsupported("affinity"))
    }
    fn get_affinity_mask_checked(
        &self,
        pid: u32,
        _expected_start_epoch_s: Option<i64>,
    ) -> Result<u64> {
        self.get_affinity_mask(pid)
    }
    fn system_affinity_mask(&self) -> Result<u64> {
        Err(tm_core::TmError::Unsupported("affinity"))
    }
    fn set_affinity_mask(&self, _pid: u32, _mask: u64) -> Result<()> {
        Err(tm_core::TmError::Unsupported("affinity"))
    }
    fn set_affinity_mask_checked(
        &self,
        pid: u32,
        _expected_start_epoch_s: Option<i64>,
        mask: u64,
    ) -> Result<()> {
        self.set_affinity_mask(pid, mask)
    }
    fn set_efficiency_mode(&self, _pid: u32, _on: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("efficiency mode"))
    }
    fn set_efficiency_mode_checked(
        &self,
        pid: u32,
        _expected_start_epoch_s: Option<i64>,
        on: bool,
    ) -> Result<()> {
        self.set_efficiency_mode(pid, on)
    }
    fn set_uac_virtualization_checked(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _enabled: bool,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("UAC virtualization"))
    }
    fn list_process_modules(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
    ) -> Result<Vec<ProcessModule>> {
        Err(tm_core::TmError::Unsupported("process modules"))
    }
    /// Ask the target process to release one exact mapped module. This is a
    /// diagnostic escape hatch: callers must confirmation-gate it, and the
    /// platform must revalidate process identity plus module base/path.
    fn unload_process_module(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _base_address: u64,
        _expected_path: &str,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("unload module"))
    }

    // ------------------------------------------------ launching / elevation
    fn is_elevated(&self) -> bool {
        false
    }
    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        let _ = (command_line, elevate);
        Err(tm_core::TmError::Unsupported("run new task"))
    }
    fn run_new_task_probe(&self, command_line: &str, elevate: bool) -> Result<()> {
        let _ = (command_line, elevate);
        Err(tm_core::TmError::Unsupported("run new task"))
    }
    fn relaunch_elevated(&self) -> Result<()> {
        Err(tm_core::TmError::Unsupported("elevation"))
    }

    // ------------------------------------------------ shell integration
    fn task_manager_replacement_state(&self) -> TaskManagerReplacementState {
        TaskManagerReplacementState::Unsupported
    }
    fn set_task_manager_replacement(&self, _enabled: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("Task Manager replacement"))
    }
    fn set_start_with_windows(&self, _enabled: bool, _start_minimized: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("start with Windows"))
    }
    fn core_service_state(&self) -> CoreServiceState {
        CoreServiceState::Unsupported
    }
    fn set_core_service_installed(&self, _installed: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("core service"))
    }
    /// Hand the current session over to the protected installed GUI. Returns
    /// `Ok(false)` when there is no installed generation to switch into.
    fn switch_to_installed_gui(&self, _args: &[String]) -> Result<bool> {
        Ok(false)
    }

    /// Create a user-mode dump. `expected_start_epoch_s` has the same
    /// PID-reuse protection contract as [`Self::kill_process`].
    fn create_dump_file(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _path: &std::path::Path,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("create dump"))
    }
    fn open_file_location(&self, _path: &str) -> Result<()> {
        Err(tm_core::TmError::Unsupported("open file location"))
    }
    fn open_properties(&self, _path: &str) -> Result<()> {
        Err(tm_core::TmError::Unsupported("properties"))
    }
    fn open_url(&self, _url: &str) -> Result<()> {
        Err(tm_core::TmError::Unsupported("open url"))
    }

    fn last_bios_time_ms(&self) -> Option<u64> {
        None
    }

    fn process_icon_rgba(&self, _exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
        None
    }

    fn backend_name(&self) -> &'static str {
        "unknown"
    }
}
