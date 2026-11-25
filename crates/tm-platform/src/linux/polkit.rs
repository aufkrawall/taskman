//! On-demand Linux elevation through the desktop's Polkit `pkexec` helper.
//!
//! Normal startup never elevates. systemd service control is retried through
//! a trusted absolute `systemctl` path after a real authorization failure.
//! The portable TaskMan image is deliberately never executed as root: privileged
//! process actions and GUI relaunch fail closed until a protected helper ships.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tm_core::error::{Result, TmError};

use super::launch;
use super::services;
use crate::command_line;
use crate::linux_action_support::{
    PrivilegedAction, is_privileged_helper_arg, parse_privileged_action,
};

const PKEXEC_CANDIDATES: [&str; 5] = [
    "/usr/bin/pkexec",
    "/bin/pkexec",
    "/usr/sbin/pkexec",
    "/run/wrappers/bin/pkexec",
    "/run/current-system/sw/bin/pkexec",
];
const MAX_PKEXEC_DIAGNOSTIC_CHARS: usize = 4 * 1024;
/// Generous on purpose: a human answers the Polkit prompt. The bound only
/// exists so a wedged agent or an unanswered dialog cannot park an action lane.
const PKEXEC_DEADLINE: Duration = Duration::from_secs(120);

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

/// Elevating the portable, user-writable TaskMan image would turn the Polkit
/// prompt into privilege escalation for mutable code, so this path fails
/// closed until a protected helper image exists. The refusal text is the same
/// constructor the hidden helper's own arm uses.
pub fn relaunch_elevated() -> Result<()> {
    Err(unsupported_relaunch())
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
        // The capability is false on this backend, so the Run dialog never
        // offers the toggle. A programmatic request fails closed with the same
        // typed error macOS uses instead of degrading to an unelevated launch.
        return Err(TmError::Unsupported("elevated arbitrary task launch"));
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
    match action {
        PrivilegedAction::Service { unit, action } => {
            let systemctl = trusted_program(&services::SYSTEMCTL_CANDIDATES)?;
            let verb = match action {
                crate::actions::ServiceAction::Start => "start",
                crate::actions::ServiceAction::Stop => "stop",
                crate::actions::ServiceAction::Restart => "restart",
            };
            run_pkexec(
                &systemctl,
                &[
                    "--no-ask-password".to_string(),
                    verb.to_string(),
                    unit.clone(),
                ],
                action_label_service(*action),
            )
        }
        // A portable GUI is user-writable. Never ask Polkit to execute that
        // mutable image as root merely to mirror an in-process syscall.
        PrivilegedAction::Kill { .. }
        | PrivilegedAction::Suspend { .. }
        | PrivilegedAction::Resume { .. }
        | PrivilegedAction::SetPriority { .. }
        | PrivilegedAction::SetAffinity { .. } => Err(unsupported_process_action()),
        PrivilegedAction::Relaunch { .. } => Err(unsupported_relaunch()),
    }
}

/// The one request-side and root-side policy for actions this installation
/// cannot elevate. Both halves call these so the hidden helper entry point
/// cannot perform what the GUI refuses to ask for.
fn unsupported_process_action() -> TmError {
    TmError::Unsupported(
        "privileged process action in a portable installation; run TaskMan from a protected install",
    )
}

fn unsupported_relaunch() -> TmError {
    TmError::Unsupported("elevated GUI relaunch from a portable installation")
}

/// Escalate only a permission failure. Every other error is returned intact,
/// so a missing unit/process or malformed target cannot turn into a spurious
/// authentication prompt.
pub(super) fn direct_or_elevated(direct: Result<()>, action: PrivilegedAction) -> Result<()> {
    match direct {
        Ok(()) => Ok(()),
        Err(error) if is_permission_denied(&error) && !is_elevated() => {
            tracing::info!(
                action = action.label(),
                "requesting Linux Polkit authorization"
            );
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

fn run_pkexec(program: &Path, args: &[String], action: &'static str) -> Result<()> {
    let pkexec = pkexec_path()?;
    let mut child = Command::new(&pkexec)
        .arg(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            TmError::platform(
                "pkexec",
                format!("Polkit's pkexec is unavailable or could not start: {error}"),
            )
        })?;
    // Neither pipe may wait for the exit: `pkexec` writes its diagnostics while
    // it authenticates, and a full pipe would block the helper we are timing.
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(TmError::platform(
            "pkexec",
            "could not capture the authorization helper's output",
        ));
    };
    let stdout = drain_pipe(stdout);
    let stderr = drain_pipe(stderr);
    // An interactive prompt is expected to take a while, but a hung agent or an
    // unanswered dialog must not park one of the two action lanes (and then its
    // queue) for the rest of the session.
    if let Err(error) =
        launch::wait_for_exit(&mut child, Instant::now() + PKEXEC_DEADLINE, "pkexec")
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(TmError::platform(
            action,
            format!(
                "Polkit authorization did not complete within {} seconds and was cancelled: {error}",
                PKEXEC_DEADLINE.as_secs()
            ),
        ));
    }
    let status = child
        .wait()
        .map_err(|error| TmError::platform("pkexec", error.to_string()))?;
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }

    let mut detail = first_diagnostic(&stdout, &stderr).unwrap_or_else(|| status.to_string());
    if let Some((index, _)) = detail.char_indices().nth(MAX_PKEXEC_DIAGNOSTIC_CHARS) {
        detail.truncate(index);
    }
    let guidance = match status.code() {
        Some(126) => "Polkit authorization was dismissed",
        Some(127) => "Polkit denied authorization",
        Some(125) => "Polkit authentication failed or no authentication agent was available",
        _ => "Polkit authorization or the requested privileged action failed",
    };
    Err(TmError::platform(action, format!("{guidance}: {detail}")))
}

/// Read one of the helper's pipes to its end on its own thread, so the deadline
/// wait and a chatty helper cannot block each other. The handle is deliberately
/// dropped, never joined, on the timeout path: a descendant that inherited the
/// pipe would make the join unbounded, which is the failure this bound exists
/// to prevent.
fn drain_pipe(pipe: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

fn pkexec_path() -> Result<PathBuf> {
    trusted_program(&PKEXEC_CANDIDATES)
}

fn trusted_program(candidates: &[&str]) -> Result<PathBuf> {
    for candidate in candidates {
        let candidate = Path::new(candidate);
        if candidate.is_file()
            && let Ok(path) = launch::trusted_system_program(candidate)
        {
            return Ok(path);
        }
    }
    Err(TmError::Unsupported(
        "required system utility is not installed at a trusted path",
    ))
}

fn action_label_service(action: crate::actions::ServiceAction) -> &'static str {
    match action {
        crate::actions::ServiceAction::Start => "service start",
        crate::actions::ServiceAction::Stop => "service stop",
        crate::actions::ServiceAction::Restart => "service restart",
    }
}

fn first_diagnostic(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    stderr
        .split(|byte| *byte == b'\n')
        .chain(stdout.split(|byte| *byte == b'\n'))
        .map(|line| String::from_utf8_lossy(line).trim().to_string())
        .find(|line| !line.is_empty())
}

/// Execute the action this process was entered as root for. The process
/// controls are the protected-helper half of the design and stay unreachable
/// from the GUI, which refuses them at request time; a relaunch is refused here
/// as well, because it would run this user-writable image as root.
fn execute_privileged_action(action: PrivilegedAction) -> Result<()> {
    match action {
        PrivilegedAction::Service { unit, action } => services::control_unit_direct(&unit, action),
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
        // The root side of the same policy: being entered as root does not make
        // this image a protected one. Re-executing the portable, user-writable
        // TaskMan as root is exactly what the request side refuses, so the
        // hidden entry point must not do it either.
        PrivilegedAction::Relaunch { .. } => Err(unsupported_relaunch()),
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
    fn the_helper_refuses_to_execute_its_own_image_as_root() {
        let error = execute_privileged_action(PrivilegedAction::Relaunch {
            arguments: vec!["--tab=services".into()],
            environment: vec![("DISPLAY".into(), ":0".into())],
        })
        .expect_err("a portable image must never be re-executed as root");
        assert!(matches!(error, TmError::Unsupported(_)), "{error}");
        assert!(error.to_string().contains("relaunch"), "{error}");
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
