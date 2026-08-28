//! Per-process control operations: kill, suspend, priority, affinity,
//! efficiency mode, elevation, launching.

use tm_core::error::{Result, TmError};
use tm_core::model::PriorityClass;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Threading as th;
use windows::Win32::UI::Shell::ShellExecuteExW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

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

/// Creation time of the process a HANDLE refers to, in Unix seconds. Read
/// through an already-open handle, so the answer describes the SAME process
/// object no matter whether the pid was recycled after the handle was
/// opened — this is what makes the identity check below race-free.
fn creation_epoch_from_handle(h: HANDLE) -> Option<i64> {
    unsafe {
        let mut create = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if th::GetProcessTimes(h, &mut create, &mut exit, &mut kernel, &mut user).is_err() {
            return None;
        }
        let raw = (create.dwHighDateTime as i64) << 32 | create.dwLowDateTime as i64;
        Some((raw - 116_444_736_000_000_000) / 10_000_000)
    }
}

/// Tolerance (seconds) when comparing creation times. sysinfo derives
/// start_epoch_s with the same truncation, but a small slack keeps harmless
/// rounding differences from failing a legitimate kill while still being
/// orders of magnitude below any realistic pid-reuse delay.
const CREATION_MATCH_TOLERANCE_S: i64 = 2;

fn creation_matches(expected: i64, actual: Option<i64>) -> bool {
    match actual {
        Some(t) => (t - expected).abs() <= CREATION_MATCH_TOLERANCE_S,
        // Cannot verify the identity of a destructive operation → fail
        // closed. GetProcessTimes only fails on handles that cannot
        // terminate anyway (protected processes).
        None => false,
    }
}

/// Open `pid` for an operation and verify its identity first. When
/// `expected_start_epoch_s` is `Some`, the process creation time is read
/// through the freshly opened handle and compared BEFORE returning it; on a
/// mismatch (pid recycled between snapshot and action) the handle is closed
/// and an error is returned instead of acting on an unrelated process.
fn open_process_verified(
    pid: u32,
    access: th::PROCESS_ACCESS_RIGHTS,
    expected_start_epoch_s: Option<i64>,
) -> Result<HANDLE> {
    // Query access is needed for the creation-time check; it is grantable
    // wherever the destructive rights below are.
    let h = open_process(pid, access | th::PROCESS_QUERY_LIMITED_INFORMATION)?;
    if let Some(expected) = expected_start_epoch_s {
        let actual = creation_epoch_from_handle(h);
        if !creation_matches(expected, actual) {
            unsafe {
                let _ = CloseHandle(h);
            }
            return Err(TmError::ProcessNotFound { pid });
        }
    }
    Ok(h)
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

/// Full command line via ntdll!NtQueryInformationProcess with
/// ProcessCommandLineInformation (windows-rs PROCESSINFOCLASS = 60, verified
/// against this machine's ntdll — older references claiming class 92 do not
/// match current Windows builds). Works with only
/// PROCESS_QUERY_LIMITED_INFORMATION because the kernel serves the string
/// from its cached process parameters; elevated/protected processes simply
/// fail to open and yield None (Details renders "—").
pub fn command_line_of(pid: u32) -> Option<String> {
    use windows::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessCommandLineInformation,
    };
    use windows::Win32::Foundation::UNICODE_STRING;

    /// Command lines are bounded by the NT RTL (UNICODE_STRING max is 64 KiB);
    /// anything claiming more is a bogus ReturnLength.
    const MAX_QUERY_BYTES: u32 = 64 * 1024;
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;

    unsafe {
        let Ok(h) = open_process(pid, th::PROCESS_QUERY_LIMITED_INFORMATION) else {
            return None;
        };
        let out = (|| {
            // Size probe, then an exact-sized buffer (standard NT pattern).
            let mut needed: u32 = 0;
            let status = NtQueryInformationProcess(
                h,
                ProcessCommandLineInformation,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
            if status.0 != STATUS_INFO_LENGTH_MISMATCH || needed == 0 {
                return None;
            }
            let needed = needed.min(MAX_QUERY_BYTES);
            let mut buf = vec![0u8; needed as usize];
            let mut written: u32 = 0;
            let status = NtQueryInformationProcess(
                h,
                ProcessCommandLineInformation,
                buf.as_mut_ptr() as _,
                needed,
                &mut written,
            );
            if status.0 != 0 {
                return None;
            }
            // The kernel writes a UNICODE_STRING whose Buffer points into our
            // buffer; map it back to an offset and bounds-check strictly.
            if buf.len() < std::mem::size_of::<UNICODE_STRING>() {
                return None;
            }
            let us = std::ptr::read_unaligned(buf.as_ptr() as *const UNICODE_STRING);
            let buf_ptr = us.Buffer.0;
            let len = us.Length as usize;
            if buf_ptr.is_null() || len == 0 || !len.is_multiple_of(2) {
                return None;
            }
            let base = buf_ptr as usize - buf.as_ptr() as usize;
            if base + len > buf.len() {
                return None;
            }
            let chars = std::slice::from_raw_parts(buf.as_ptr().add(base) as *const u16, len / 2);
            let s = String::from_utf16_lossy(chars);
            let s = s.trim_end_matches('\0');
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        })();
        let _ = CloseHandle(h);
        out
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

/// Terminate without an identity check (`expected_start_epoch_s = None`).
/// Kept for callers that hold no snapshot identity (tests, trait default).
pub fn kill_single(pid: u32) -> Result<()> {
    terminate_verified(pid, None)
}

/// Terminate `pid` after verifying, THROUGH the terminate handle, that its
/// creation time still matches `expected_start_epoch_s` (see
/// [`open_process_verified`]). A recycled pid is refused instead of killing
/// an unrelated process.
pub fn terminate_verified(pid: u32, expected_start_epoch_s: Option<i64>) -> Result<()> {
    let h = open_process_verified(pid, th::PROCESS_TERMINATE, expected_start_epoch_s)?;
    unsafe {
        // Exit code 1 mirrors Task Manager behavior.
        let r = th::TerminateProcess(h, 1)
            .map_err(|e| TmError::platform("TerminateProcess", e.to_string()));
        let _ = CloseHandle(h);
        r
    }
}

/// Terminate `pid` (and, with `tree`, all descendants). When
/// `expected_start_epoch_s` is `Some`, every targeted process must still
/// have the expected creation time at the moment its terminate handle is
/// opened — the same identity discipline the UI applies, moved down to the
/// one place where it can be enforced race-free.
pub fn kill_process(pid: u32, expected_start_epoch_s: Option<i64>, tree: bool) -> Result<()> {
    if !tree {
        return terminate_verified(pid, expected_start_epoch_s);
    }
    // Descendants: capture each child's creation time right at enumeration;
    // terminate_verified re-checks it through the handle before firing.
    let children = collect_children_with_births(pid);
    // Kill descendants depth-first (deepest last in BFS order → reverse).
    let mut err: Option<TmError> = None;
    for (c, birth) in children.iter().rev() {
        if *c != std::process::id()
            // A child whose birth could not be fingerprinted falls back to
            // the previous unverified kill (its creation time has nothing to
            // do with the root's identity, so substituting that would fail
            // every such child).
            && let Err(e) = terminate_verified(*c, *birth)
        {
            tracing::debug!(child = c, error = %e, "tree-kill child failed");
            err.get_or_insert(e);
        }
    }
    match terminate_verified(pid, expected_start_epoch_s) {
        Ok(()) => err.map_or(Ok(()), Err),
        Err(e) => Err(err.unwrap_or(e)),
    }
}

/// BFS all descendant pids of `pid` via a full process snapshot, capturing
/// each child's creation time as observed AT ENUMERATION TIME. The later
/// handle-bound re-check compares against these values, so a child that
/// exits and whose pid is recycled before termination is refused.
fn collect_children_with_births(root: u32) -> Vec<(u32, Option<i64>)> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
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
                    // Birth captured now, verified again via the handle later.
                    let birth = open_process(k, th::PROCESS_QUERY_LIMITED_INFORMATION)
                        .ok()
                        .and_then(|h| {
                            let t = creation_epoch_from_handle(h);
                            unsafe {
                                let _ = CloseHandle(h);
                            }
                            t
                        });
                    out.push((k, birth));
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
                .map_or(0, |m| m.0 as usize)
        });
        if base == 0 {
            return None;
        }
        let cname = std::ffi::CString::new(name).ok()?;
        let proc_addr = windows::Win32::System::LibraryLoader::GetProcAddress(
            windows::Win32::Foundation::HMODULE(base as *mut _),
            windows::core::PCSTR(cname.as_ptr() as *const u8),
        )?;
        Some(std::mem::transmute::<usize, NtSuspendFn>(
            proc_addr as usize,
        ))
    }
}

pub fn suspend_process(pid: u32, suspend: bool) -> Result<()> {
    let f = ntdll_fn(if suspend {
        "NtSuspendProcess"
    } else {
        "NtResumeProcess"
    })
    .ok_or(TmError::Unsupported("ntdll suspend/resume"))?;
    unsafe {
        let h = open_process(pid, th::PROCESS_SUSPEND_RESUME)?;
        let rc = f(h);
        let _ = CloseHandle(h);
        if rc == 0 {
            Ok(())
        } else {
            Err(TmError::platform(
                "suspend/resume",
                format!("NTSTATUS {rc:#x}"),
            ))
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
    get_affinity(pid)
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
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
    };
    unsafe {
        let h = open_process(pid, th::PROCESS_SET_INFORMATION)?;
        // State=0 + Control set → throttling ON; State=flag → throttling OFF.
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: 1,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: if on {
                0u32
            } else {
                PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            },
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

/// Query the current EcoQoS / power-throttling state of `pid` so externally
/// applied efficiency states are reflected correctly (implement.md §11.6).
pub fn efficiency_mode_state(pid: u32) -> Option<bool> {
    use windows::Win32::System::Threading::{
        GetProcessInformation, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling,
    };
    unsafe {
        let h = open_process(pid, th::PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
        let mut info = PROCESS_POWER_THROTTLING_STATE::default();
        let ok = GetProcessInformation(
            h,
            ProcessPowerThrottling,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        // ControlMask says whether throttling is managed at all; StateMask
        // carries the enabled flag.
        Some(
            info.ControlMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED != 0
                && info.StateMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED == 0,
        )
    }
}

/// Per-process token security facts (implement.md §12.2/§12.3):
/// real `TokenElevation` plus UAC virtualization state from
/// `TokenVirtualizationAllowed`/`TokenVirtualizationEnabled`. Access-denied
/// yields `None` fields — never fabricated values.
pub struct TokenSecurity {
    pub elevated: Option<bool>,
    pub virtualization: Option<tm_core::model::UacVirtualization>,
}

pub fn token_security(pid: u32) -> TokenSecurity {
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
        TokenVirtualizationAllowed, TokenVirtualizationEnabled,
    };
    use windows::Win32::System::Threading::OpenProcessToken;

    let mut out = TokenSecurity {
        elevated: None,
        virtualization: None,
    };
    unsafe {
        let Ok(h) = open_process(pid, th::PROCESS_QUERY_LIMITED_INFORMATION) else {
            return out;
        };
        let mut token = HANDLE::default();
        if OpenProcessToken(h, TOKEN_QUERY, &mut token).is_ok() {
            // --- elevation ---
            let mut elev = TOKEN_ELEVATION::default();
            let mut ret: u32 = 0;
            if GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elev as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret,
            )
            .is_ok()
            {
                out.elevated = Some(elev.TokenIsElevated != 0);
            }

            // --- UAC virtualization ---
            let query_dword =
                |cls: windows::Win32::Security::TOKEN_INFORMATION_CLASS| -> Option<u32> {
                    let mut val: u32 = 0;
                    let mut ret: u32 = 0;
                    GetTokenInformation(
                        token,
                        cls,
                        Some(&mut val as *mut u32 as *mut _),
                        std::mem::size_of::<u32>() as u32,
                        &mut ret,
                    )
                    .ok()?;
                    Some(val)
                };
            let allowed = query_dword(TokenVirtualizationAllowed);
            let enabled = query_dword(TokenVirtualizationEnabled);
            out.virtualization = match (allowed, enabled) {
                (Some(a), Some(e)) => Some(if a == 0 {
                    tm_core::model::UacVirtualization::NotAllowed
                } else if e != 0 {
                    tm_core::model::UacVirtualization::Enabled
                } else {
                    tm_core::model::UacVirtualization::Disabled
                }),
                _ => None,
            };

            let _ = CloseHandle(token);
        }
        let _ = CloseHandle(h);
    }
    out
}

// ------------------------------------------------------------------ elevation / launch

pub fn is_elevated() -> bool {
    token_security(std::process::id()).elevated.unwrap_or(false)
}

pub fn run_new_task(command_line: &str, elevate: bool) -> Result<()> {
    let (file, params) = split_command(command_line);
    if file.is_empty() {
        return Err(TmError::platform("run_new_task", "empty command"));
    }
    // Never wait on the caller's thread: ShellExecuteExW itself is quick,
    // but the old 500 ms failure-probe wait blocked UI-thread callers such
    // as "services.msc" / "ms-settings:" jumps for up to half a second
    // (implement.md §18.1). Failure probing is opt-in via
    // [`run_new_task_probe`] and belongs on worker threads only.
    shell_execute(&file, params.as_deref(), elevate, false)
}

/// Like [`run_new_task`] but waits up to 500 ms to surface immediate
/// launch failures. Must be called off the UI thread.
pub fn run_new_task_probe(command_line: &str, elevate: bool) -> Result<()> {
    let (file, params) = split_command(command_line);
    if file.is_empty() {
        return Err(TmError::platform("run_new_task", "empty command"));
    }
    shell_execute(&file, params.as_deref(), elevate, true)
}

pub fn relaunch_elevated() -> Result<()> {
    let exe =
        std::env::current_exe().map_err(|e| TmError::platform("current_exe", e.to_string()))?;
    shell_execute(&exe.to_string_lossy(), None, true, false)
}

// ------------------------------------------------------------------ shell helpers

/// Reveal `path` in Explorer with the file preselected.
pub fn open_file_location(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(TmError::platform("open_file_location", "no path"));
    }
    shell_execute(
        "explorer.exe",
        Some(&format!("/select,\"{path}\"")),
        false,
        false,
    )
}

/// Open the Explorer Properties dialog for `path` (SEE_MASK_INVOKEIDLIST
/// with the "properties" verb).
pub fn open_properties(path: &str) -> Result<()> {
    use windows::Win32::UI::Shell::{
        SEE_MASK_INVOKEIDLIST, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    let file_w: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let verb_w: Vec<u16> = "properties\0".encode_utf16().collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_INVOKEIDLIST,
        lpVerb: PCWSTR::from_raw(verb_w.as_ptr()),
        lpFile: PCWSTR::from_raw(file_w.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info)
            .map_err(|e| TmError::platform("ShellExecuteExW(properties)", e.to_string()))?;
        if !info.hProcess.is_invalid() {
            let _ = CloseHandle(info.hProcess);
        }
    }
    Ok(())
}

/// Open a URL / document with the default handler.
pub fn open_url(url: &str) -> Result<()> {
    shell_execute(url, None, false, false)
}

/// Write a minidump of `pid` to `path` via dbghelp's MiniDumpWriteDump.
pub fn create_dump_file(pid: u32, path: &std::path::Path) -> Result<()> {
    use windows::Win32::Storage::FileSystem::{
        CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows::Win32::System::Diagnostics::Debug::{MINIDUMP_TYPE, MiniDumpWriteDump};

    let hproc = open_process(
        pid,
        th::PROCESS_QUERY_INFORMATION | th::PROCESS_VM_READ | th::PROCESS_DUP_HANDLE,
    )?;
    let result = (|| {
        let wide: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        let hfile = unsafe {
            CreateFileW(
                PCWSTR::from_raw(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                // CREATE_ALWAYS truncates an existing dump; OPEN_ALWAYS could
                // leave stale trailing bytes when the new dump is shorter.
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| TmError::platform("CreateFileW(dump)", e.to_string()))?;
        let dump_result =
            unsafe { MiniDumpWriteDump(hproc, pid, hfile, MINIDUMP_TYPE(0), None, None, None) };
        let _ = unsafe { CloseHandle(hfile) };
        dump_result.map_err(|e| TmError::platform("MiniDumpWriteDump", e.to_string()))
    })();
    let _ = unsafe { CloseHandle(hproc) };
    result
}

fn shell_execute(file: &str, params: Option<&str>, elevate: bool, wait: bool) -> Result<()> {
    use windows::Win32::UI::Shell::{
        SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };

    let file_w: Vec<u16> = file.encode_utf16().chain([0]).collect();
    let params_w: Option<Vec<u16>> = params.map(|p| p.encode_utf16().chain([0]).collect());
    let verb_w: Option<Vec<u16>> = elevate.then(|| "runas\0".encode_utf16().collect());

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb_w
            .as_ref()
            .map_or(PCWSTR::null(), |v| PCWSTR::from_raw(v.as_ptr())),
        lpFile: PCWSTR::from_raw(file_w.as_ptr()),
        lpParameters: params_w
            .as_ref()
            .map_or(PCWSTR::null(), |p| PCWSTR::from_raw(p.as_ptr())),
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
    if cmd.starts_with('"')
        && let Some(end) = cmd[1..].find('"')
    {
        let file = cmd[1..1 + end].to_string();
        let rest = cmd[2 + end..].trim();
        return (file, (!rest.is_empty()).then(|| rest.to_string()));
    }
    match cmd.split_once(' ') {
        Some((f, p)) => (f.to_string(), Some(p.trim().to_string())),
        None => (cmd.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_of_own_process_is_the_exe_path() {
        // The test binary itself is always queryable. Argument retrieval is
        // covered by the spawned-child test; here only plausibility matters,
        // because the harness invokes the binary with varying arguments.
        let cmdline = command_line_of(std::process::id()).expect("own command line");
        assert!(
            cmdline.to_ascii_lowercase().contains("tm_platform"),
            "unexpected: {cmdline}"
        );
    }

    #[test]
    fn command_line_of_spawned_child_contains_arguments() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child");
        let pid = child.id();
        std::thread::sleep(std::time::Duration::from_millis(150));
        let cmdline = command_line_of(pid).expect("command line retrievable");
        assert!(
            cmdline.to_ascii_lowercase().contains("ping -n 30"),
            "unexpected: {cmdline}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn command_line_of_invalid_pid_is_none() {
        // PID reuse makes a fixed "unused" pid impossible, but a pid this
        // large cannot exist and OpenProcess must fail cleanly.
        assert_eq!(command_line_of(u32::MAX - 8), None);
    }
}
