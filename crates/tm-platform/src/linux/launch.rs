//! Detached process-group launches and trust checks for privileged execution,
//! plus the bounded child wait the `systemctl`/`loginctl` probes share.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tm_core::error::{Result, TmError};

/// Start a program in its own process group and reap it from a short helper
/// thread without keeping the caller's action lane occupied for the program's
/// lifetime.
pub(super) fn spawn_detached_reaped(
    program: &OsStr,
    arguments: &[OsString],
    environment: &[(String, String)],
) -> Result<()> {
    let child = build_command(program, arguments, environment)
        .spawn()
        .map_err(|error| {
            TmError::platform(
                "launch",
                format!("could not start {}: {error}", program.to_string_lossy()),
            )
        })?;
    reaping_thread(child)
}

fn build_command(
    program: &OsStr,
    arguments: &[OsString],
    environment: &[(String, String)],
) -> Command {
    let mut command = Command::new(program);
    command.args(arguments);
    if !environment.is_empty() {
        command.env_clear();
        command.envs(environment.iter().map(|(name, value)| (name, value)));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command
}

fn reaping_thread(child: Child) -> Result<()> {
    let child = Arc::new(Mutex::new(Some(child)));
    let waiter_child = child.clone();
    let spawned = std::thread::Builder::new()
        .name("tm-linux-launch-wait".into())
        .spawn(move || {
            let child = waiter_child.lock().ok().and_then(|mut child| child.take());
            if let Some(mut child) = child {
                let _ = child.wait();
            }
        });
    match spawned {
        Ok(_) => Ok(()),
        Err(error) => {
            if let Some(mut child) = child.lock().ok().and_then(|mut child| child.take()) {
                let _ = child.kill();
                let _ = child.wait();
            }
            Err(TmError::platform(
                "launch",
                format!("could not start the child-reaping thread: {error}"),
            ))
        }
    }
}

/// Wait for `child` to exit before `deadline`.
///
/// The kernel's `pidfd` is preferred: it is readable as soon as the child
/// exits, so no polling is needed. A kernel or seccomp profile that refuses
/// `pidfd_open` falls back to a bounded `try_wait` poll. `what` names the
/// program in the timeout diagnostic. A `TimedOut` return leaves the child
/// running on purpose: only the caller knows whether the work is still wanted,
/// so it decides to kill and reap.
pub(super) fn wait_for_exit(
    child: &mut Child,
    deadline: Instant,
    what: &str,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
        if fd >= 0 {
            let result = wait_pidfd(fd as libc::c_int, deadline, what);
            unsafe {
                libc::close(fd as libc::c_int);
            }
            return result;
        }
    }

    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(deadline_error(what));
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
}

#[cfg(target_os = "linux")]
fn wait_pidfd(fd: libc::c_int, deadline: Instant, what: &str) -> std::io::Result<()> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(deadline_error(what));
        }
        let remaining_ms = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut pollfd, 1, remaining_ms) };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn deadline_error(what: &str) -> std::io::Error {
    let detail = format!("{what} exceeded its deadline");
    std::io::Error::new(std::io::ErrorKind::TimedOut, detail)
}

/// Require a canonical system-tool path and every parent directory to be
/// root-owned and not group/other writable. This prevents a writable PATH or
/// replaced helper from participating in the elevation request.
pub(super) fn trusted_system_program(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        Err(TmError::platform(
            "trusted system program",
            "program path must be absolute",
        ))
    } else {
        trust_executable(path)
    }
}

fn trust_executable(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        TmError::platform(
            "trusted system program",
            format!("cannot resolve {}: {error}", path.display()),
        )
    })?;
    for component in canonical.ancestors() {
        let metadata = std::fs::metadata(component).map_err(|error| {
            TmError::platform(
                "trusted system program",
                format!("cannot inspect {}: {error}", component.display()),
            )
        })?;
        let mode = metadata.permissions().mode();
        if metadata.uid() != 0 || mode & 0o022 != 0 {
            return Err(TmError::platform(
                "trusted system program",
                format!(
                    "refusing to run a program from a non-root-owned or writable path: {}",
                    component.display()
                ),
            ));
        }
    }
    if !is_executable_file(&canonical) {
        return Err(TmError::platform(
            "trusted system program",
            format!("{} is not an executable regular file", canonical.display()),
        ));
    }
    Ok(canonical)
}

fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_system_programs_must_be_absolute() {
        assert!(trusted_system_program(Path::new("./local-program")).is_err());
        assert!(trusted_system_program(Path::new("../local-program")).is_err());
    }
}
