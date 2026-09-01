//! Windows backend.

mod autostart;
pub mod core_service;
mod cpu_info;
pub(crate) mod cpu_load;
mod gpu;
pub mod icons;
pub mod locale;
pub mod memory_info;
mod net_etw;
mod net_info;

/// Test-only handles into the ETW network trace, so integration tests can
/// tell "no events" apart from "pruned away" without exposing the module.
#[doc(hidden)]
pub fn net_etw_test_start() -> Option<net_etw::NetworkUsage> {
    net_etw::NetworkUsage::start(net_etw::TraceRole::App)
}

#[doc(hidden)]
pub fn live_pids_for_test() -> std::collections::HashSet<u32> {
    core_service::live_pids_for_test()
}
mod perfcounters;
mod process_ops;
mod sampler;
mod services;
mod startup;
mod taskmgr_replacement;
/// Sub-pixel (ClearType) text-rendering parameters and their validity gates.
pub mod text_rendering;
mod threads_map;
pub(crate) mod users;
mod version;
/// Native caption appearance (DWM attributes) — see `window_chrome`.
pub mod window_chrome;
mod windows_enum;

use crate::actions::*;
use tm_core::engine::SystemCollector;
use tm_core::error::Result;
use tm_core::model::*;

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

/// Entry point used only by the short-lived elevated helper process.
pub fn set_task_manager_replacement_direct(enabled: bool) -> Result<()> {
    taskmgr_replacement::set_direct(enabled)
}

pub(crate) fn task_manager_replacement_state_for(
    exe: &std::path::Path,
) -> TaskManagerReplacementState {
    match taskmgr_replacement::state_for_exe(exe) {
        taskmgr_replacement::State::Disabled => TaskManagerReplacementState::Disabled,
        taskmgr_replacement::State::Enabled => TaskManagerReplacementState::Enabled,
        taskmgr_replacement::State::Stale(value) => TaskManagerReplacementState::Stale(value),
        taskmgr_replacement::State::Conflict(value) => TaskManagerReplacementState::Conflict(value),
    }
}

pub(crate) fn set_task_manager_replacement_direct_for(
    enabled: bool,
    exe: &std::path::Path,
) -> Result<()> {
    taskmgr_replacement::set_direct_for_exe(enabled, exe)
}

/// Whether THIS process runs with an elevated (admin) token.
pub fn is_elevated() -> bool {
    process_ops::is_elevated()
}

/// Keep the UI/service control plane responsive when ordinary workloads
/// saturate the machine. Above-normal is intentional: HIGH/REALTIME can starve
/// input and disk-flush threads and would make a Task Manager replacement less
/// reliable rather than more reliable.
pub fn prioritize_control_plane() {
    use windows::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetCurrentThread, SetPriorityClass,
        SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    unsafe {
        if let Err(error) = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) {
            tracing::debug!(%error, "could not raise TaskMan process priority");
        }
        if let Err(error) = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) {
            tracing::debug!(%error, "could not raise TaskMan control-thread priority");
        }
    }
}

/// Spawn a new elevated instance of the current exe (runas verb → UAC
/// consent), passing `args` through, and return once it is spawning.
/// Backs the "always start elevated" setting at startup; the interactive
/// settings-dialog restart uses the no-args `relaunch_elevated` action
/// instead.
/// Quote one argument per the MSVCRT/CommandLineToArgvW rules. The elevated
/// relaunch crosses a privilege boundary (the UAC-approved process parses
/// this command line itself), so the quoting must be correct by
/// construction: embedded quotes must not be able to terminate an argument
/// and splice in extra ones, and trailing backslashes must not escape the
/// closing quote. Arguments without spaces, tabs or quotes pass through
/// unchanged, so plain flags look exactly as before.
fn quote_win_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 3);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // Every pending backslash now precedes a quote and must be
                // doubled; the quote itself becomes an escaped literal.
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push_str("\\\"");
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Backslashes directly before the closing quote double up too.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

pub fn relaunch_elevated_with_args(args: &[String]) -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| tm_core::TmError::platform("current_exe", e.to_string()))?;
    let mut cmdline = format!("\"{}\"", exe.to_string_lossy());
    for a in args {
        cmdline.push(' ');
        cmdline.push_str(&quote_win_arg(a));
    }
    process_ops::run_new_task(&cmdline, true)
}

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

    fn set_demand(&mut self, demand: tm_core::demand::TelemetryDemand) {
        self.inner.set_demand(demand);
    }
}

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
            per_process_network: false,
            process_modules: true,
            unload_module: true,
            start_with_windows: true,
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
    fn kill_process(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        tree: bool,
    ) -> Result<()> {
        process_ops::kill_process(pid, expected_start_epoch_s, tree)
    }
    fn kill_single(&self, pid: u32) -> Result<()> {
        process_ops::kill_single(pid)
    }
    fn suspend_process(&self, pid: u32, suspend: bool) -> Result<()> {
        process_ops::suspend_process(pid, suspend)
    }
    fn suspend_process_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        suspend: bool,
    ) -> Result<()> {
        process_ops::suspend_process_checked(pid, expected_start_epoch_s, suspend)
    }
    fn set_priority(&self, pid: u32, priority: PriorityClass) -> Result<()> {
        process_ops::set_priority(pid, priority)
    }
    fn set_priority_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        priority: PriorityClass,
    ) -> Result<()> {
        process_ops::set_priority_checked(pid, expected_start_epoch_s, priority)
    }
    fn get_affinity_mask(&self, pid: u32) -> Result<u64> {
        process_ops::get_affinity(pid)
    }
    fn get_affinity_mask_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<u64> {
        process_ops::get_affinity_checked(pid, expected_start_epoch_s)
    }
    fn system_affinity_mask(&self) -> Result<u64> {
        process_ops::system_affinity()
    }
    fn set_affinity_mask(&self, pid: u32, mask: u64) -> Result<()> {
        process_ops::set_affinity(pid, mask)
    }
    fn set_affinity_mask_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        mask: u64,
    ) -> Result<()> {
        process_ops::set_affinity_checked(pid, expected_start_epoch_s, mask)
    }
    fn set_efficiency_mode(&self, pid: u32, on: bool) -> Result<()> {
        process_ops::set_efficiency_mode(pid, on)
    }
    fn set_efficiency_mode_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        on: bool,
    ) -> Result<()> {
        process_ops::set_efficiency_mode_checked(pid, expected_start_epoch_s, on)
    }
    fn set_uac_virtualization_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        enabled: bool,
    ) -> Result<()> {
        process_ops::set_uac_virtualization_checked(pid, expected_start_epoch_s, enabled)
    }
    fn list_process_modules(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<Vec<ProcessModule>> {
        process_ops::list_process_modules(pid, expected_start_epoch_s)
    }
    fn unload_process_module(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        base_address: u64,
        expected_path: &str,
    ) -> Result<()> {
        process_ops::unload_process_module(pid, expected_start_epoch_s, base_address, expected_path)
    }
    fn is_elevated(&self) -> bool {
        process_ops::is_elevated()
    }
    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        process_ops::run_new_task(command_line, elevate)
    }
    fn run_new_task_probe(&self, command_line: &str, elevate: bool) -> Result<()> {
        process_ops::run_new_task_probe(command_line, elevate)
    }
    fn relaunch_elevated(&self) -> Result<()> {
        process_ops::relaunch_elevated()
    }

    fn task_manager_replacement_state(&self) -> TaskManagerReplacementState {
        let Ok(exe) = std::env::current_exe() else {
            return TaskManagerReplacementState::Disabled;
        };
        task_manager_replacement_state_for(&exe)
    }

    fn set_task_manager_replacement(&self, enabled: bool) -> Result<()> {
        // HKLM writes need elevation. Keep the main GUI unelevated and launch
        // a short-lived helper through the existing ShellExecute("runas") path.
        if process_ops::is_elevated() {
            return taskmgr_replacement::set_direct(enabled);
        }
        let exe = std::env::current_exe()
            .map_err(|e| tm_core::TmError::platform("current_exe", e.to_string()))?;
        let action = if enabled { "enable" } else { "disable" };
        process_ops::run_new_task(
            &format!(
                "\"{}\" --taskmgr-integration={action}",
                exe.to_string_lossy()
            ),
            true,
        )
    }

    fn set_start_with_windows(&self, enabled: bool, start_minimized: bool) -> Result<()> {
        autostart::set_enabled(enabled, start_minimized)
    }

    fn create_dump_file(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        path: &std::path::Path,
    ) -> Result<()> {
        process_ops::create_dump_file(pid, expected_start_epoch_s, path)
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

pub fn create_collector() -> WinCollector {
    WinCollector {
        inner: sampler::Sampler::new(),
    }
}

pub fn create_actions() -> core_service::BrokeredActions {
    core_service::BrokeredActions::default()
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    /// Parse with the OS ground truth (CommandLineToArgvW — the same rules
    /// the elevated relaunch's target uses). The returned argv buffer is
    /// intentionally leaked: test-only, the process exits right after.
    fn parse_args(line: &str) -> Vec<String> {
        use windows::core::PCWSTR;
        let wide: Vec<u16> = line.encode_utf16().chain([0]).collect();
        let mut argc = 0i32;
        let argv = unsafe {
            windows::Win32::UI::Shell::CommandLineToArgvW(
                PCWSTR::from_raw(wide.as_ptr()),
                &mut argc,
            )
        };
        assert!(!argv.is_null(), "CommandLineToArgvW failed");
        (0..argc as usize)
            .map(|i| unsafe { (*argv.add(i)).to_string().unwrap_or_default() })
            .collect()
    }

    /// The quoting must survive the OS parser exactly: embedded quotes may
    /// neither terminate an argument nor splice extra ones in.
    #[test]
    fn relaunch_arg_quoting_round_trips_through_command_line_parsing() {
        let cases: Vec<Vec<String>> = vec![
            vec![],
            vec!["--tab=processes".into()],
            vec!["--tab=pro cesses".into()],
            vec!["with\"quote".into()],
            vec!["ends-with\\".into()],
            vec!["bs\\before\"quote".into(), String::new()],
            vec!["-v".into(), "--size=800x600".into(), "ünïcode".into()],
        ];
        for args in cases {
            // argv[0] is parsed in a special mode, so a plain marker stands
            // in for the exe; the ARGS are what the elevated taskman parses
            // in standard mode.
            let mut line = String::from("taskman");
            for a in &args {
                line.push(' ');
                line.push_str(&quote_win_arg(a));
            }
            let parsed = parse_args(&line);
            assert_eq!(&parsed[1..], args.as_slice(), "cmdline: {line}");
        }
    }
}
