//! On-demand Linux elevation through the desktop's Polkit `pkexec` helper.
//!
//! Normal startup never elevates. A denied direct syscall or service control
//! asks `pkexec` to re-enter this same executable in a short-lived, typed
//! helper mode. The helper accepts only the actions built here; it never runs
//! a privileged shell command.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tm_core::error::{Result, TmError};

use super::launch;
use super::services;
use crate::command_line;
use crate::linux_action_support::{
    self, PrivilegedAction, is_privileged_helper_arg, invocation_args, parse_privileged_action,
};

const PKEXEC_CANDIDATES: [&str; 5] = [
    "/usr/bin/pkexec",
    "/bin/pkexec",
    "/usr/sbin/pkexec",
    "/run/wrappers/bin/pkexec",
    "/run/current-system/sw/bin/pkexec",
];
const MAX_PKEXEC_DIAGNOSTIC_CHARS: usize = 4 * 1024;

pub fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Handle the hidden helper mode before logging, locale, settings, or any GUI
/// initialization. Any argument carrying the private prefix enters this path;
/// malformed requests fail instead of falling through to an ordinary launch.
pub fn handle_helper(args: &[String]) -> Option<i32> {
    if !args.iter().any(|arg| is_privileged_helper_arg(arg)) {
        return None;
    }
    if !is_elevated() {
        eprintln!("taskman: Linux privileged helper requires effective uid 0");
        return Some(1);
    }

    let action = match parse_privileged_action(args) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("taskman: invalid Linux privileged helper request: {error}");
            return Some(2);
        }
    };
    match execute_privileged_action(action) {
        Ok(()) => Some(0),
        Err(error) => {
            eprintln!("taskman: Linux privileged helper failed: {error}");
            Some(1)
        }
    }
}

/// Re-run the current executable through `pkexec`, preserving its original
/// argv entries as separate arguments and forwarding only allowlisted desktop
/// variables. The process is authenticated and typed; no command text reaches
/// a privileged shell.
pub fn relaunch_elevated() -> Result<()> {
    if is_elevated() {
        return Ok(());
    }
    let mut arguments = Vec::new();
    for argument in std::env::args_os().skip(1) {
        let argument = argument.into_string().map_err(|_| {
            TmError::platform(
                "elevated relaunch",
                "the current command line is not valid UTF-8",
            )
        })?;
        if is_privileged_helper_arg(&argument) {
            return Err(TmError::platform(
                "elevated relaunch",
                "an internal Linux privileged helper argument cannot be forwarded",
            ));
        }
        arguments.push(argument);
    }
    run_privileged_action(&PrivilegedAction::Relaunch {
        arguments,
        environment: desktop_environment(),
    })
}

/// Split the Run dialog's command line into argv and launch it directly. The
/// child gets its own process group and is reaped asynchronously, so neither a
/// long-lived application nor a terminal signal leaves the action lane stuck.
///
/// The Linux `run_new_task_probe` path is intentionally not overridden: it
/// must remain non-launching. Callers that need the actual result use this
/// method on an action worker.
pub fn run_new_task(command_line: &str, elevate: bool) -> Result<()> {
    let argv = command_line::split(command_line).map_err(|error| {
        TmError::platform("run new task", format!("invalid command line: {error}"))
    })?;
    let program = argv
        .first()
        .ok_or_else(|| TmError::platform("run new task", "empty command"))?;

    if elevate && !is_elevated() {
        return Err(TmError::platform(
            "run new task",
            "elevated arbitrary task launch is disabled; run the program unelevated or use \
             the matching desktop service",
        ));
    }

    let arguments: Vec<OsString> = argv[1..].iter().map(OsString::from).collect();
    launch::spawn_detached_reaped(OsStr::new(program), &arguments, &[])
}

pub(super) fn run_privileged_action(action: &PrivilegedAction) -> Result<()> {
    if is_elevated() {
        return Err(TmError::platform(
            action.label(),
            "the current process already has effective uid 0",
        ));
    }
    let executable = std::env::current_exe()
        .map_err(|error| TmError::platform("current_exe", error.to_string()))?;
    let args = invocation_args(action)
        .map_err(|error| TmError::platform(action.label(), error))?;
    run_pkexec(&executable, &args, action.label())
}

/// Escalate only a permission failure. Every other error is returned intact,
/// so a missing unit/process or malformed target cannot turn into a spurious
/// authentication prompt.
pub(super) fn direct_or_elevated(direct: Result<()>, action: PrivilegedAction) -> Result<()> {
    match direct {
        Ok(()) => Ok(()),
        Err(error) if is_permission_denied(&error) && !is_elevated() => {
            tracing::info!(action = action.label(), "requesting Linux Polkit authorization");
            run_privileged_action(&action)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn is_permission_denied(error: &TmError) -> bool {
    match error {
        TmError::Io(error) => {
            error.kind() == std::io::ErrorKind::PermissionDenied
                || matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES))
        }
        TmError::Platform { detail, .. } => {
            let detail = detail.to_ascii_lowercase();
            [
                "operation not permitted",
                "permission denied",
                "access denied",
                "insufficient privileges",
                "interactive authentication required",
                "authentication is required",
                "not authorized",
            ]
            .iter()
            .any(|needle| detail.contains(needle))
        }
        _ => false,
    }
}

fn run_pkexec(executable: &Path, args: &[String], action: &'static str) -> Result<()> {
    let pkexec = pkexec_path()?;
    let output = Command::new(&pkexec)
        .arg(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            TmError::platform(
                "pkexec",
                format!("Polkit's pkexec is unavailable or could not start: {error}"),
            )
        })?;
    if output.status.success() {
        return Ok(());
    }

    let mut detail = first_diagnostic(&output.stdout, &output.stderr)
        .unwrap_or_else(|| output.status.to_string());
    if let Some((index, _)) = detail.char_indices().nth(MAX_PKEXEC_DIAGNOSTIC_CHARS) {
        detail.truncate(index);
    }
    let guidance = match output.status.code() {
        Some(126) => "Polkit authorization was dismissed",
        Some(127) => "Polkit denied authorization",
        Some(125) => {
            "no Polkit authentication agent was available; start the desktop's Polkit agent"
        }
        _ => "Polkit authorization or the requested privileged action failed",
    };
    Err(TmError::platform(action, format!("{guidance}: {detail}")))
}

fn pkexec_path() -> Result<PathBuf> {
    for candidate in PKEXEC_CANDIDATES {
        let candidate = Path::new(candidate);
        if candidate.is_file()
            && let Ok(path) = launch::trusted_system_program(candidate)
        {
            return Ok(path);
        }
    }
    Err(TmError::Unsupported(
        "Linux privilege escalation: pkexec is not installed at a trusted system path",
    ))
}

fn first_diagnostic(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    stderr
        .split(|byte| *byte == b'\n')
        .chain(stdout.split(|byte| *byte == b'\n'))
        .map(|line| String::from_utf8_lossy(line).trim().to_string())
        .find(|line| !line.is_empty())
}

fn execute_privileged_action(action: PrivilegedAction) -> Result<()> {
    match action {
        PrivilegedAction::Service { unit, action } => {
            services::control_unit_direct(&unit, action)
        }
        PrivilegedAction::Kill { pid, start_ticks } => {
            validate_privileged_target(pid)?;
            super::send_signal_checked(pid, Some(start_ticks), libc::SIGTERM)
        }
        PrivilegedAction::Suspend { pid, start_ticks } => {
            validate_privileged_target(pid)?;
            super::send_signal_checked(pid, Some(start_ticks), libc::SIGSTOP)
        }
        PrivilegedAction::Resume { pid, start_ticks } => {
            validate_privileged_target(pid)?;
            super::send_signal_checked(pid, Some(start_ticks), libc::SIGCONT)
        }
        PrivilegedAction::SetPriority {
            pid,
            start_ticks,
            priority,
        } => {
            validate_privileged_target(pid)?;
            super::set_priority_direct(pid, Some(start_ticks), priority)
        }
        PrivilegedAction::SetAffinity {
            pid,
            start_ticks,
            mask,
        } => {
            validate_privileged_target(pid)?;
            super::set_affinity_direct(pid, Some(start_ticks), mask)
        }
        PrivilegedAction::Relaunch {
            arguments,
            environment,
        } => {
            if arguments.iter().any(|arg| is_privileged_helper_arg(arg)) {
                return Err(TmError::platform(
                    "elevated relaunch",
                    "refusing a relaunch argument that re-enters the privileged helper",
                ));
            }
            let executable = std::env::current_exe()
                .map_err(|error| TmError::platform("current_exe", error.to_string()))?;
            let arguments: Vec<OsString> = arguments.into_iter().map(OsString::from).collect();
            launch::spawn_detached(executable.as_os_str(), &arguments, &environment)
        }
    }
}

fn validate_privileged_target(pid: u32) -> Result<()> {
    if pid <= 1 || pid == std::process::id() {
        return Err(TmError::platform(
            "privileged process action",
            format!("pid {pid} is a protected helper or init target"),
        ));
    }
    Ok(())
}

fn desktop_environment() -> Vec<(String, String)> {
    linux_action_support::FORWARDED_ENVIRONMENT_NAMES
        .iter()
        .filter_map(|name| {
            let value = std::env::var(*name).ok()?;
            if value.is_empty() {
                return None;
            }
            linux_action_support::validate_environment_name(name).ok()?;
            Some(((*name).to_string(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_classification_does_not_prompt_for_missing_targets() {
        assert!(is_permission_denied(&TmError::Io(
            std::io::Error::from_raw_os_error(libc::EPERM)
        )));
        assert!(is_permission_denied(&TmError::platform(
            "systemctl",
            "Access denied"
        )));
        assert!(is_permission_denied(&TmError::platform(
            "systemctl",
            "authentication is required"
        )));
        assert!(!is_permission_denied(&TmError::platform(
            "systemctl",
            "Unit missing.service could not be found"
        )));
    }

    #[test]
    fn diagnostics_use_the_first_nonempty_line() {
        assert_eq!(
            first_diagnostic(
                b"",
                b"\nAuthentication failed: request dismissed\nmore detail"
            )
            .as_deref(),
            Some("Authentication failed: request dismissed")
        );
    }

    #[test]
    fn privileged_process_actions_refuse_init_and_the_helper_itself() {
        assert!(validate_privileged_target(1).is_err());
        assert!(validate_privileged_target(std::process::id()).is_err());
        assert!(validate_privileged_target(123_456_789).is_ok());
    }

    #[test]
    fn empty_run_command_is_rejected_before_any_launch() {
        let error = run_new_task("   ", false).unwrap_err();
        assert!(error.to_string().contains("empty command"), "{error}");
    }

    #[test]
    fn ordinary_gui_arguments_do_not_enter_the_helper() {
        assert_eq!(
            handle_helper(&["--size=1280x800".to_string(), "--verbose".to_string()]),
            None
        );
    }
}
