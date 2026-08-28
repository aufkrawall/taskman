//! Windows backend.

mod cpu_info;
pub(crate) mod cpu_load;
mod gpu;
pub mod icons;
pub mod locale;
pub mod memory_info;
mod net_info;
mod perfcounters;
mod process_ops;
mod sampler;
mod services;
mod startup;
mod taskmgr_replacement;
mod threads_map;
pub(crate) mod users;
mod version;
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

/// Whether THIS process runs with an elevated (admin) token.
pub fn is_elevated() -> bool {
    process_ops::is_elevated()
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
    fn run_new_task_probe(&self, command_line: &str, elevate: bool) -> Result<()> {
        process_ops::run_new_task_probe(command_line, elevate)
    }
    fn relaunch_elevated(&self) -> Result<()> {
        process_ops::relaunch_elevated()
    }

    fn task_manager_replacement_state(&self) -> TaskManagerReplacementState {
        match taskmgr_replacement::state() {
            taskmgr_replacement::State::Disabled => TaskManagerReplacementState::Disabled,
            taskmgr_replacement::State::Enabled => TaskManagerReplacementState::Enabled,
            taskmgr_replacement::State::Stale(v) => TaskManagerReplacementState::Stale(v),
            taskmgr_replacement::State::Conflict(v) => TaskManagerReplacementState::Conflict(v),
        }
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

pub fn create_collector() -> WinCollector {
    WinCollector {
        inner: sampler::Sampler::new(),
    }
}

pub fn create_actions() -> WinActions {
    WinActions
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
