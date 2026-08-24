//! Per-process control operations: kill, suspend, priority, affinity,
//! efficiency mode, elevation, launching.

use tm_core::error::{Result, TmError};
use tm_core::model::PriorityClass;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading as th;
use windows::Win32::UI::Shell::ShellExecuteExW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;


fn open_process(pid: u32, access: th::PROCESS_ACCESS_RIGHTS) -> Result<HANDLE> {
    unsafe {
        let h = th::OpenProcess(access, false, pid).map_err(|e| {
            if e.code().0 == 87 {
                TmError::ProcessNotFound { pid }
            } else {
                TmError::platform("OpenProcess", e.to_string())
            }
        })?;
        Ok(h)
    }
}

// ------------------------------------------------------------------ status queries

pub fn session_id_of(pid: u32) -> Option<u32> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut sid: u32 = 0;
    unsafe { ProcessIdToSessionId(pid, &mut sid).ok()? };
    Some(sid)
}

pub fn handle_count(pid: u32) -> Option<u32> {
    unsafe {
        let h = open_process(pid, th::PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
        let mut count: u32 = 0;
        let ok = th::GetProcessHandleCount(h, &mut count).is_ok();
        let _ = CloseHandle(h);
        if ok { Some(count) } else { None }
    }
}

pub fn is_wow64(pid: u32) -> Option<bool> {
    unsafe {
        let h = open_process(pid, th::PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
        let mut wow: windows::core::BOOL = Default::default();
        let ok = th::IsWow64Process(h, &mut wow).is_ok();
        let _ = CloseHandle(h);
        if ok { Some(wow.as_bool()) } else { None }
    }
}

pub fn priority_class_of(pid: u32) -> PriorityClass {
    unsafe {
        let Ok(h) = open_process(pid, th::PROCESS_QUERY_LIMITED_INFORMATION) else {
            return PriorityClass::Unknown;
        };
        let class = th::GetPriorityClass(h);
        let _ = CloseHandle(h);
        map_priority(class)
    }
}

fn map_priority(class: u32) -> PriorityClass {
    if class == th::REALTIME_PRIORITY_CLASS.0 {
        PriorityClass::Realtime
    } else if class == th::HIGH_PRIORITY_CLASS.0 {
        PriorityClass::High
    } else if class == th::ABOVE_NORMAL_PRIORITY_CLASS.0 {
        PriorityClass::AboveNormal
    } else if class == th::BELOW_NORMAL_PRIORITY_CLASS.0 {
        PriorityClass::BelowNormal
    } else if class == th::IDLE_PRIORITY_CLASS.0 {
        PriorityClass::Low
    } else if class == th::NORMAL_PRIORITY_CLASS.0 {
        PriorityClass::Normal
    } else {
        PriorityClass::Unknown
    }
}

pub fn unmap_priority(p: PriorityClass) -> th::PROCESS_CREATION_FLAGS {
    match p {
        PriorityClass::Realtime => th::REALTIME_PRIORITY_CLASS,
        PriorityClass::High => th::HIGH_PRIORITY_CLASS,
        PriorityClass::AboveNormal => th::ABOVE_NORMAL_PRIORITY_CLASS,
        PriorityClass::BelowNormal => th::BELOW_NORMAL_PRIORITY_CLASS,
        PriorityClass::Low | PriorityClass::Unknown => th::IDLE_PRIORITY_CLASS,
        PriorityClass::Normal => th::NORMAL_PRIORITY_CLASS,
    }
}

// ------------------------------------------------------------------ termination

pub fn kill_single(pid: u32) -> Result<()> {
    unsafe {
        let h = open_process(pid, th::PROCESS_TERMINATE)?;
        // Exit code 1 mirrors Task Manager behavior.
        let r = th::TerminateProcess(h, 1).map_err(|e| TmError::platform("TerminateProcess", e.to_string()));
        let _ = CloseHandle(h);
        r
    }
}

pub fn kill_process(pid: u32, tree: bool) -> Result<()> {
    if !tree {
        return kill_single(pid);
    }
    let children = collect_children(pid);
    // Kill descendants depth-first (deepest last in BFS order → reverse).
    let mut err: Option<TmError> = None;
    for c in children.iter().rev() {
        if *c != std::process::id() {
            if let Err(e) = kill_single(*c) {
                tracing::debug!(child = c, error = %e, "tree-kill child failed");
                err.get_or_insert(e);
            }
        }
    }
    match kill_single(pid) {
        Ok(()) => err.map_or(Ok(()), Err),
        Err(e) => Err(err.unwrap_or(e)),
    }
}

/// BFS all descendant pids of `pid` via a full process snapshot.
fn collect_children(root: u32) -> Vec<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
        PROCESSENTRY32W,
    };

    let mut parent_map: std::collections::HashMap<u32, Vec<u32>> = Default::default();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Vec::new();
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                parent_map
                    .entry(entry.th32ParentProcessID)
                    .or_default()
                    .push(entry.th32ProcessID);
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }

    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([root]);
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::from([root]);
    while let Some(p) = queue.pop_front() {
        if let Some(kids) = parent_map.get(&p) {
            for k in kids.clone() {
                if seen.insert(k) {
                    out.push(k);
                    queue.push_back(k);
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------ suspend / resume

type NtSuspendFn = unsafe extern "system" fn(HANDLE) -> i32;

fn ntdll_fn(name: &str) -> Option<NtSuspendFn> {
    unsafe {
        static NTDLL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let base = *NTDLL.get_or_init(|| {
            let wide: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
            windows::Win32::System::LibraryLoader::GetModuleHandleW(PCWSTR::from_raw(wide.as_ptr()))
                .map(|m| m.0 as usize)
                .unwrap_or(0)
        });
        if base == 0 {
            return None;
        }
        let cname = std::ffi::CString::new(name).ok()?;
        let proc_addr =
            windows::Win32::System::LibraryLoader::GetProcAddress(
                windows::Win32::Foundation::HMODULE(base as *mut _),
                windows::core::PCSTR(cname.as_ptr() as *const u8),
            )?;
        Some(std::mem::transmute::<usize, NtSuspendFn>(proc_addr as usize))
    }
}

pub fn suspend_process(pid: u32, suspend: bool) -> Result<()> {
    let f = ntdll_fn(if suspend { "NtSuspendProcess" } else { "NtResumeProcess" })
        .ok_or_else(|| TmError::Unsupported("ntdll suspend/resume"))?;
    unsafe {
        let h = open_process(pid, th::PROCESS_SUSPEND_RESUME)?;
        let rc = f(h);
        let _ = CloseHandle(h);
        if rc == 0 {
            Ok(())
        } else {
            Err(TmError::platform("suspend/resume", format!("NTSTATUS {rc:#x}")))
        }
    }
}

// ------------------------------------------------------------------ priority / affinity

pub fn set_priority(pid: u32, priority: PriorityClass) -> Result<()> {
    unsafe {
        let h = open_process(pid, th::PROCESS_SET_INFORMATION)?;
        let r = th::SetPriorityClass(h, unmap_priority(priority))
            .map_err(|e| TmError::platform("SetPriorityClass", e.to_string()));
        let _ = CloseHandle(h);
        r
    }
}

pub fn get_affinity(pid: u32) -> Result<u64> {
    unsafe {
        let h = open_process(pid, th::PROCESS_QUERY_LIMITED_INFORMATION)?;
        let mut proc_mask: usize = 0;
        let mut sys_mask: usize = 0;
        let r = th::GetProcessAffinityMask(h, &mut proc_mask, &mut sys_mask)
            .map_err(|e| TmError::platform("GetProcessAffinityMask", e.to_string()))
            .map(|_| proc_mask as u64);
        let _ = CloseHandle(h);
        r
    }
}

pub fn system_affinity() -> Result<u64> {
    let pid = std::process::id();
    get_affinity(pid).map(|mask| {
        // Our own process mask may be restricted; fall back to kernel mask via any process.
        mask
    })
}

pub fn set_affinity(pid: u32, mask: u64) -> Result<()> {
    unsafe {
        let h = open_process(pid, th::PROCESS_SET_INFORMATION)?;
        let r = th::SetProcessAffinityMask(h, mask as usize)
            .map_err(|e| TmError::platform("SetProcessAffinityMask", e.to_string()));
        let _ = CloseHandle(h);
        r
    }
}

// ------------------------------------------------------------------ efficiency mode (EcoQoS)

pub fn set_efficiency_mode(pid: u32, on: bool) -> Result<()> {
    use windows::Win32::System::Threading::{
        SetProcessInformation, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling,
    };
    unsafe {
        let h = open_process(pid, th::PROCESS_SET_INFORMATION)?;
        // State=0 + Control set → throttling ON; State=flag → throttling OFF.
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: 1,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: if on { 0u32 } else { PROCESS_POWER_THROTTLING_EXECUTION_SPEED },
        };
        let r = SetProcessInformation(
            h,
            ProcessPowerThrottling,
            &state as *const _ as *const _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .map_err(|e| TmError::platform("SetProcessInformation(EcoQoS)", e.to_string()));
        let _ = CloseHandle(h);
        r
    }
}

// ------------------------------------------------------------------ elevation / launch

pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::OpenProcessToken;
    unsafe {
        let Ok(process) = th::OpenProcess(th::PROCESS_QUERY_LIMITED_INFORMATION, false, std::process::id())
        else {
            return false;
        };
        let mut token = HANDLE::default();
        let elevated = OpenProcessToken(process, TOKEN_QUERY, &mut token).map(|()| {
            let mut elev = TOKEN_ELEVATION::default();
            let mut ret_len: u32 = 0;
            GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elev as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            )
            .is_ok()
                && elev.TokenIsElevated != 0
        });
        let _ = CloseHandle(token);
        let _ = CloseHandle(process);
        matches!(elevated, Ok(true))
    }
}

pub fn run_new_task(command_line: &str, elevate: bool) -> Result<()> {
    let (file, params) = split_command(command_line);
    if file.is_empty() {
        return Err(TmError::platform("run_new_task", "empty command"));
    }
    shell_execute(&file, params.as_deref(), elevate, true)
}

pub fn relaunch_elevated() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| TmError::platform("current_exe", e.to_string()))?;
    shell_execute(&exe.to_string_lossy(), None, true, false)
}

fn shell_execute(file: &str, params: Option<&str>, elevate: bool, wait: bool) -> Result<()> {
    use windows::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

    let file_w: Vec<u16> = file.encode_utf16().chain([0]).collect();
    let params_w: Option<Vec<u16>> =
        params.map(|p| p.encode_utf16().chain([0]).collect());
    let verb_w: Option<Vec<u16>> = elevate.then(|| "runas\0".encode_utf16().collect());

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb_w
            .as_ref()
            .map(|v| PCWSTR::from_raw(v.as_ptr()))
            .unwrap_or(PCWSTR::null()),
        lpFile: PCWSTR::from_raw(file_w.as_ptr()),
        lpParameters: params_w
            .as_ref()
            .map(|p| PCWSTR::from_raw(p.as_ptr()))
            .unwrap_or(PCWSTR::null()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| {
            TmError::platform("ShellExecuteExW", format!("{e} (elevation denied?)"))
        })?;
        if wait && !info.hProcess.is_invalid() {
            // Brief wait so failures surface quickly; don't block forever.
            let _ = th::WaitForSingleObject(info.hProcess, 500);
            let _ = CloseHandle(info.hProcess);
        }
    }
    Ok(())
}

/// Split "prog args..." respecting quotes into (file, params).
fn split_command(cmd: &str) -> (String, Option<String>) {
    let cmd = cmd.trim();
    if cmd.starts_with('"') {
        if let Some(end) = cmd[1..].find('"') {
            let file = cmd[1..1 + end].to_string();
            let rest = cmd[2 + end..].trim();
            return (file, (!rest.is_empty()).then(|| rest.to_string()));
        }
    }
    match cmd.split_once(' ') {
        Some((f, p)) => (f.to_string(), Some(p.trim().to_string())),
        None => (cmd.to_string(), None),
    }
}
