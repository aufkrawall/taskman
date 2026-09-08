from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one replacement, found {count}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


p = Path("crates/tm-platform/src/win/taskmgr_replacement.rs")
text = p.read_text(encoding="utf-8")
start = text.index("/// Launch the built-in Windows Task Manager without changing the IFEO")
end = text.index("\nfn read_debugger() -> DebuggerValue {", start)
new = r'''/// Start the built-in Task Manager normally. The caller must ensure our IFEO
/// debugger entry is absent first; otherwise Windows will intentionally route
/// this launch back into TaskMan.
pub(crate) fn launch_native_task_manager_plain() -> Result<()> {
    let path = system_task_manager_path()?;
    let command = format!("\"{}\"", path.to_string_lossy());
    super::process_ops::run_new_task_probe(&command, false)
}

fn launch_then_restore(original: &str) -> Result<()> {
    let launch = launch_native_task_manager_plain();
    let restore = write_debugger(original);
    match (launch, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(launch), Ok(())) => Err(launch),
        (Ok(()), Err(restore)) => Err(TmError::platform(
            "Windows Task Manager",
            format!("Task Manager was launched, but TaskMan's interception could not be restored: {restore}"),
        )),
        (Err(launch), Err(restore)) => Err(TmError::platform(
            "Windows Task Manager",
            format!("launch failed: {launch}; restoring TaskMan's interception also failed: {restore}"),
        )),
    }
}

/// Elevated implementation of the native-Task-Manager escape hatch.
///
/// IFEO debugger selection happens synchronously during process creation. For
/// a TaskMan-owned registration, delete only that value, create Taskmgr.exe in
/// the interactive session, then restore the exact original debugger command
/// before returning. This is more reliable on Windows 11 than trying to use a
/// debug-created Taskmgr process as an IFEO bypass.
pub fn launch_native_task_manager_direct() -> Result<()> {
    match read_debugger() {
        DebuggerValue::Absent => launch_native_task_manager_plain(),
        DebuggerValue::Text(original) if is_owned_command(&original) => {
            delete_debugger()?;
            launch_then_restore(&original)
        }
        DebuggerValue::Text(value) => Err(TmError::platform(
            "Windows Task Manager",
            format!("another IFEO debugger is registered for Taskmgr.exe: {value}"),
        )),
        DebuggerValue::Invalid => Err(TmError::platform(
            "Windows Task Manager",
            "the Taskmgr.exe IFEO debugger registration is unreadable or malformed",
        )),
    }
}

/// Launch the built-in Windows Task Manager even while TaskMan owns the IFEO
/// replacement. A disabled registration needs no elevation. When the owned
/// HKLM value must be suspended, an elevated short-lived copy performs the
/// complete delete/launch/restore transaction and this call waits for its real
/// exit status, so the UI cannot report success merely because a helper was
/// spawned.
pub fn launch_native_task_manager() -> Result<()> {
    match read_debugger() {
        DebuggerValue::Absent => launch_native_task_manager_plain(),
        DebuggerValue::Text(value) if is_owned_command(&value) => {
            if super::process_ops::is_elevated() {
                return launch_native_task_manager_direct();
            }
            let exe = std::env::current_exe()
                .map_err(|error| TmError::platform("current_exe", error.to_string()))?;
            let command = format!("\"{}\" --native-taskmgr-helper", exe.to_string_lossy());
            super::process_ops::run_new_task_wait(
                &command,
                true,
                std::time::Duration::from_secs(30),
            )
        }
        DebuggerValue::Text(value) => Err(TmError::platform(
            "Windows Task Manager",
            format!("another IFEO debugger is registered for Taskmgr.exe: {value}"),
        )),
        DebuggerValue::Invalid => Err(TmError::platform(
            "Windows Task Manager",
            "the Taskmgr.exe IFEO debugger registration is unreadable or malformed",
        )),
    }
}
'''
p.write_text(text[:start] + new + text[end:], encoding="utf-8")

replace_once(
    "crates/tm-platform/src/win/mod.rs",
    '''pub fn set_task_manager_replacement_direct(enabled: bool) -> Result<()> {
    taskmgr_replacement::set_direct(enabled)
}
''',
    '''pub fn set_task_manager_replacement_direct(enabled: bool) -> Result<()> {
    taskmgr_replacement::set_direct(enabled)
}

/// Entry point used only by the short-lived elevated helper that opens the
/// built-in Task Manager while preserving TaskMan's IFEO replacement.
pub fn launch_native_task_manager_direct() -> Result<()> {
    taskmgr_replacement::launch_native_task_manager_direct()
}
''',
)

replace_once(
    "crates/tm-app/src/main.rs",
    '''    // Elevated install/remove helper for the protected core service. Like the
    // IFEO helper above, this path performs no GUI or renderer initialization.
''',
    '''    // Short-lived elevated helper for opening the real Windows Task Manager
    // while our IFEO replacement is enabled. The platform routine performs
    // the full delete/launch/restore transaction before this process exits.
    #[cfg(target_os = "windows")]
    if args.iter().any(|argument| argument == "--native-taskmgr-helper") {
        let code = match tm_platform::win::launch_native_task_manager_direct() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("taskman: native Task Manager launch failed: {error}");
                1
            }
        };
        std::process::exit(code);
    }

    // Elevated install/remove helper for the protected core service. Like the
    // IFEO helper above, this path performs no GUI or renderer initialization.
''',
)
