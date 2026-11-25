//! Detached process-group launches and trust checks for privileged execution,
//! plus the bounded child wait the `systemctl`/`loginctl` probes share.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tm_core::error::{Result, TmError};

/// Largest single stdio stream this module collects from a helper.
///
/// `systemctl` and `loginctl` answer in a few hundred bytes; the cap only keeps a
/// runaway child from allocating without bound inside its deadline.
const HELPER_OUTPUT_LIMIT: usize = 1024 * 1024;

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
/// `pidfd_open`, or one whose `pidfd` never becomes readable, falls back to a
/// bounded `try_wait` poll. `what` names the program in the timeout diagnostic.
/// A `TimedOut` return leaves the child running on purpose: only the caller
/// knows whether the work is still wanted, so it decides to kill and reap.
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
            // Only a pidfd that became readable proves the exit. A readiness
            // that is not `POLLIN` — `poll` reports an unusable descriptor as
            // `POLLNVAL` instead of failing — proves nothing, so the bounded
            // `try_wait` loop below decides instead. Claiming an exit nobody
            // observed would let the caller's `wait_with_output` block past
            // this deadline.
            if result? {
                return Ok(());
            }
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

/// Poll the kernel's `pidfd` for the child's exit before `deadline`.
///
/// `Ok(true)` means the descriptor became readable, which is the kernel's exit
/// signal. `Ok(false)` means it cannot be polled for exit at all, and the caller
/// must prove the exit itself. An error is a passed deadline or a failed poll.
#[cfg(target_os = "linux")]
fn wait_pidfd(fd: libc::c_int, deadline: Instant, what: &str) -> std::io::Result<bool> {
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
            return Ok(pollfd.revents & libc::POLLIN != 0);
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

/// Run `child` to completion and collect its output, all before `deadline`.
///
/// `Child::wait_with_output` drains the pipes to EOF with no deadline of its own:
/// a process that inherited the helper's descriptors keeps them open after the
/// helper itself exits, so that read outlived the deadline the wait had just
/// established. Both streams are therefore drained under the same absolute
/// deadline — `poll` plus `read`, like the `fc-match` runner — and the rest of the
/// budget then proves the exit. A `TimedOut` return leaves the child, its streams
/// and its reap to the caller, which owns the kill.
pub(super) fn wait_with_output(
    child: &mut Child,
    deadline: Instant,
    what: &str,
) -> std::io::Result<Output> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    drain_streams(&mut stdout, &mut stderr, &mut out, &mut err, deadline, what)?;
    wait_for_exit(child, deadline, what)?;
    // The exit is proven now, either by a readable pidfd or by the `try_wait`
    // loop that already reaped it, so this only collects a status the kernel is
    // holding.
    let status = child.wait()?;
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

/// Read both helper streams until each is closed or `deadline` passes.
fn drain_streams<O: Read + AsRawFd, E: Read + AsRawFd>(
    stdout: &mut Option<O>,
    stderr: &mut Option<E>,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    deadline: Instant,
    what: &str,
) -> std::io::Result<()> {
    while stdout.is_some() || stderr.is_some() {
        let now = Instant::now();
        if now >= deadline {
            return Err(deadline_error(what));
        }
        let remaining_ms = (deadline - now).as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut pollfds = [
            libc::pollfd {
                fd: stdout.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stderr.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `pollfds` holds two initialized descriptors for the duration of
        // the call (a negative entry is the closed stream, which `poll` ignores),
        // and the timeout is bounded.
        let ready = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, remaining_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        if pollfds[0].revents != 0 {
            read_ready(stdout, out, what)?;
        }
        if pollfds[1].revents != 0 {
            read_ready(stderr, err, what)?;
        }
    }
    Ok(())
}

/// Read one stream that `poll` reported ready, or close it at end of file.
fn read_ready<T: Read>(
    stream: &mut Option<T>,
    buffer: &mut Vec<u8>,
    what: &str,
) -> std::io::Result<()> {
    let Some(pipe) = stream.as_mut() else {
        return Ok(());
    };
    if buffer.len() >= HELPER_OUTPUT_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{what} wrote more than {HELPER_OUTPUT_LIMIT} bytes to one stream"),
        ));
    }
    let mut chunk = [0u8; 4096];
    let want = (HELPER_OUTPUT_LIMIT - buffer.len()).min(chunk.len());
    let read = pipe.read(&mut chunk[..want])?;
    if read == 0 {
        *stream = None;
        return Ok(());
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(())
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
