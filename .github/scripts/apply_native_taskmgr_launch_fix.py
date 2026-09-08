from pathlib import Path

path = Path("crates/tm-platform/src/win/taskmgr_replacement.rs")
text = path.read_text(encoding="utf-8")
start = text.index("pub fn launch_native_task_manager() -> Result<()> {")
end = text.index("\nfn read_debugger()", start)
replacement = r'''pub fn launch_native_task_manager() -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::Debug::{
        ContinueDebugEvent, DBG_CONTINUE, DEBUG_EVENT, DebugActiveProcessStop,
        DebugSetProcessKillOnExit, WaitForDebugEvent,
    };
    use windows::Win32::System::Threading::{
        CreateProcessW, DEBUG_ONLY_THIS_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
        TerminateProcess,
    };
    use windows::core::{PCWSTR, PWSTR};

    let path = system_task_manager_path()?;
    let application: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // CreateProcessW is allowed to mutate its command-line buffer.
    let mut command_line = application.clone();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR::from_raw(application.as_ptr()),
            Some(PWSTR::from_raw(command_line.as_mut_ptr())),
            None,
            None,
            false,
            DEBUG_ONLY_THIS_PROCESS,
            None,
            PCWSTR::null(),
            &startup,
            &mut process,
        )
    }
    .map_err(|error| TmError::platform("CreateProcessW(Taskmgr.exe)", error.to_string()))?;

    // DEBUG_ONLY_THIS_PROCESS is what bypasses IFEO, but a debug-created
    // process is stopped at its initial CREATE_PROCESS_DEBUG_EVENT. Detaching
    // immediately, before acknowledging that event, is not sufficient on all
    // supported Windows builds (notably current Task Manager can remain stuck
    // before user code ever runs). Consume and continue the initial event
    // first, then detach while there is no outstanding debug event.
    if let Err(error) = unsafe { DebugSetProcessKillOnExit(false) } {
        unsafe {
            let _ = TerminateProcess(process.hProcess, 1);
            let _ = CloseHandle(process.hThread);
            let _ = CloseHandle(process.hProcess);
        }
        return Err(TmError::platform(
            "DebugSetProcessKillOnExit(Taskmgr.exe)",
            error.to_string(),
        ));
    }

    let mut event = DEBUG_EVENT::default();
    if let Err(error) = unsafe { WaitForDebugEvent(&mut event, 5_000) } {
        unsafe {
            let _ = TerminateProcess(process.hProcess, 1);
            let _ = CloseHandle(process.hThread);
            let _ = CloseHandle(process.hProcess);
        }
        return Err(TmError::platform(
            "WaitForDebugEvent(Taskmgr.exe)",
            error.to_string(),
        ));
    }

    if let Err(error) = unsafe {
        ContinueDebugEvent(event.dwProcessId, event.dwThreadId, DBG_CONTINUE)
    } {
        unsafe {
            let _ = TerminateProcess(process.hProcess, 1);
            let _ = CloseHandle(process.hThread);
            let _ = CloseHandle(process.hProcess);
        }
        return Err(TmError::platform(
            "ContinueDebugEvent(Taskmgr.exe)",
            error.to_string(),
        ));
    }

    let detached = unsafe { DebugActiveProcessStop(process.dwProcessId) };
    if let Err(error) = detached {
        // Never leave Task Manager attached to one of the executor's
        // long-lived worker threads: without a debugger loop it would remain
        // suspended at the next debug event indefinitely.
        unsafe {
            let _ = TerminateProcess(process.hProcess, 1);
            let _ = CloseHandle(process.hThread);
            let _ = CloseHandle(process.hProcess);
        }
        return Err(TmError::platform(
            "DebugActiveProcessStop(Taskmgr.exe)",
            error.to_string(),
        ));
    }
    unsafe {
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}
'''
path.write_text(text[:start] + replacement + text[end:], encoding="utf-8")
