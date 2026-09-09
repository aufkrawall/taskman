//! Restart the interactive Windows Explorer shell.
//!
//! Windows Task Manager exposes Restart for `explorer.exe`, but this is not a
//! generic process restart: the replacement shell must be launched in the
//! interactive user's session. Keep the operation local to the GUI instead of
//! routing it through the LocalSystem core-service broker.

use tm_core::error::{Result, TmError};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading as th;
use windows::core::PWSTR;

fn creation_epoch_from_handle(process: HANDLE) -> Option<i64> {
    unsafe {
        let mut create = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        th::GetProcessTimes(process, &mut create, &mut exit, &mut kernel, &mut user).ok()?;
        let raw = (u64::from(create.dwHighDateTime) << 32) | u64::from(create.dwLowDateTime);
        i64::try_from(raw.saturating_sub(116_444_736_000_000_000) / 10_000_000).ok()
    }
}

fn ensure_explorer_image(process: HANDLE) -> Result<()> {
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    unsafe {
        th::QueryFullProcessImageNameW(
            process,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(|error| {
        TmError::platform(
            "QueryFullProcessImageNameW(restart Explorer)",
            error.to_string(),
        )
    })?;
    buffer.truncate(length as usize);
    let path = String::from_utf16_lossy(&buffer);
    let explorer = std::path::Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"));
    if !explorer {
        return Err(TmError::platform(
            "restart Windows Explorer",
            "target is not explorer.exe",
        ));
    }
    Ok(())
}

/// Terminate one exact `explorer.exe` generation and start a replacement shell.
///
/// The creation time and executable image are both checked through the same
/// handle used to terminate the process. This preserves TaskMan's normal
/// PID-reuse safety and also makes a UI eligibility bug fail closed here.
pub fn restart(pid: u32, expected_start_epoch_s: Option<i64>) -> Result<()> {
    if pid <= 4 {
        return Err(TmError::platform(
            "restart Windows Explorer",
            "system process is protected",
        ));
    }

    let access =
        th::PROCESS_TERMINATE | th::PROCESS_QUERY_LIMITED_INFORMATION | th::PROCESS_SYNCHRONIZE;
    let process = unsafe { th::OpenProcess(access, false, pid) }.map_err(|error| {
        if error.code().0 == 87 {
            TmError::ProcessNotFound { pid }
        } else {
            TmError::platform("OpenProcess(restart Explorer)", error.to_string())
        }
    })?;

    let result = (|| -> Result<()> {
        if let Some(expected) = expected_start_epoch_s
            && creation_epoch_from_handle(process) != Some(expected)
        {
            return Err(TmError::ProcessNotFound { pid });
        }

        // Use the existing machine-safety policy too. The already-open handle
        // pins the exact object that will be terminated, so this second PID
        // lookup cannot redirect the destructive operation to another process.
        super::process_ops::refuse_critical_process(pid)?;
        ensure_explorer_image(process)?;

        unsafe { th::TerminateProcess(process, 1) }
            .map_err(|error| TmError::platform("TerminateProcess(explorer.exe)", error.to_string()))?;

        // Do not deliberately create two shell generations. Wait until the
        // selected Explorer has actually exited before asking the user-session
        // shell to start its replacement.
        let wait = unsafe { th::WaitForSingleObject(process, 10_000) };
        if wait == WAIT_TIMEOUT {
            return Err(TmError::platform(
                "restart Windows Explorer",
                "timed out waiting for explorer.exe to exit",
            ));
        }
        if wait != WAIT_OBJECT_0 {
            return Err(TmError::platform(
                "WaitForSingleObject(restart Explorer)",
                std::io::Error::last_os_error().to_string(),
            ));
        }

        // IMPORTANT: launch locally under the GUI's interactive user token.
        // The core service runs as LocalSystem and must never own the new shell.
        super::process_ops::run_new_task("explorer.exe", false)
    })();

    let _ = unsafe { CloseHandle(process) };
    result
}
