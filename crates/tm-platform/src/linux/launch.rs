//! Detached process-group launches and trust checks for privileged execution.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use tm_core::error::{Result, TmError};

pub(super) fn spawn_detached(
    program: &OsStr,
    arguments: &[OsString],
    environment: &[(String, String)],
) -> Result<()> {
    build_command(program, arguments, environment)
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            TmError::platform(
                "launch",
                format!("could not start {}: {error}", program.to_string_lossy()),
            )
        })
}

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
    command
        .args(arguments)
        .envs(environment.iter().map(|(name, value)| (name, value)))
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
