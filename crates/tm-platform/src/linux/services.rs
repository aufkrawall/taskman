//! systemd services via `systemctl` (no dbus dependency; graceful fallback).

use std::process::Command;
use tm_core::error::{Result, TmError};
use tm_core::model::{ServiceInfo, ServiceStatus};

pub fn list_systemd_units() -> Result<Vec<ServiceInfo>> {
    let out = Command::new("systemctl")
        .args([
            "list-units",
            "--type=service",
            "--all",
            "--no-pager",
            "--no-legend",
            "--plain",
        ])
        .output()
        .map_err(|e| TmError::platform("spawn systemctl", e.to_string()))?;
    // Without systemd as PID 1 (containers, WSL without systemd, other
    // inits) systemctl fails with empty stdout; that is "unavailable", not
    // an empty service list.
    check_exit(out.status, &out.stderr)?;
    Ok(parse_units(&String::from_utf8_lossy(&out.stdout)))
}

fn check_exit(status: std::process::ExitStatus, stderr: &[u8]) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(stderr);
    let detail = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(|| status.to_string(), str::to_string);
    Err(TmError::platform("systemctl list-units", detail))
}

fn parse_units(text: &str) -> Vec<ServiceInfo> {
    let mut units = Vec::new();
    for line in text.lines() {
        // unit load active sub description  OR  "0 loaded units" summary lines
        let Some(([unit, _load, active, sub], description)) = split_columns(line) else {
            continue;
        };
        if !unit.ends_with(".service") {
            continue;
        }
        units.push(ServiceInfo {
            name: unit.to_string(),
            display_name: String::new(),
            description: description.to_string(),
            pid: None,
            status: map_status(active, sub),
            group: String::new(),
            startup_type: String::new(),
            account: String::new(),
        });
    }
    units
}

/// The four leading whitespace-separated columns and the free-text rest.
/// Splitting positionally keeps a SUB value that also occurs inside the
/// unit name (e.g. `running-check.service ... running`) from cutting the
/// description in the wrong place.
fn split_columns(line: &str) -> Option<([&str; 4], &str)> {
    let mut rest = line.trim_start();
    let mut cols = [""; 4];
    for col in &mut cols {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        *col = &rest[..end];
        rest = rest[end..].trim_start();
    }
    Some((cols, rest.trim_end()))
}

fn map_status(active: &str, sub: &str) -> ServiceStatus {
    match (active, sub) {
        ("active", "running") => ServiceStatus::Running,
        ("active", _) => ServiceStatus::Running,
        ("activating", _) => ServiceStatus::StartPending,
        ("deactivating", _) => ServiceStatus::StopPending,
        ("inactive", _) | ("failed", _) => ServiceStatus::Stopped,
        _ => ServiceStatus::Unknown,
    }
}

pub fn control_unit(unit: &str, action: super::super::actions::ServiceAction) -> Result<()> {
    use super::super::actions::ServiceAction::*;
    let verb = match action {
        Start => "start",
        Stop => "stop",
        Restart => "restart",
    };
    let out = Command::new("systemctl")
        .arg(verb)
        .arg(unit)
        .status()
        .map_err(|e| TmError::platform("spawn systemctl", e.to_string()))?;
    if out.success() {
        tracing::info!(unit, verb, "systemd unit controlled");
        Ok(())
    } else {
        Err(TmError::platform(
            "systemctl",
            format!("{verb} {unit} failed — need root?"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn description_is_taken_after_the_sub_column() {
        let units = parse_units(
            "running-check.service loaded active running Check the running thing\n\
             dbus.service          loaded active running D-Bus System Message Bus\n\
             idle.service          loaded inactive dead\n\
             \n\
             3 loaded units listed.\n",
        );
        let got: Vec<_> = units
            .iter()
            .map(|u| (u.name.as_str(), u.description.as_str(), u.status))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "running-check.service",
                    "Check the running thing",
                    ServiceStatus::Running
                ),
                (
                    "dbus.service",
                    "D-Bus System Message Bus",
                    ServiceStatus::Running
                ),
                ("idle.service", "", ServiceStatus::Stopped),
            ]
        );
    }

    #[test]
    fn failing_systemctl_is_an_error_not_an_empty_list() {
        assert!(check_exit(std::process::ExitStatus::from_raw(0), b"").is_ok());
        let err = check_exit(
            std::process::ExitStatus::from_raw(1 << 8),
            b"System has not been booted with systemd as init system (PID 1). Can't operate.\n",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not been booted with systemd"),
            "{err}"
        );
    }
}
