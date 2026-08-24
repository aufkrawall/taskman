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

    let text = String::from_utf8_lossy(&out.stdout);
    let mut units = Vec::new();
    for line in text.lines() {
        // unit load active sub description  OR  "0 loaded units" summary lines
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 || !fields[0].ends_with(".service") {
            continue;
        }
        let unit = fields[0].to_string();
        let _load = fields.get(1).copied().unwrap_or("");
        let active = fields.get(2).copied().unwrap_or("");
        let sub = fields.get(3).copied().unwrap_or("");
        let desc_start = line.find(sub).map(|i| i + sub.len()).unwrap_or(0);
        let description = line[desc_start..].trim().to_string();

        units.push(ServiceInfo {
            name: unit,
            display_name: String::new(),
            description,
            pid: None,
            status: map_status(active, sub),
            group: String::new(),
            startup_type: String::new(),
            account: String::new(),
        });
    }
    Ok(units)
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
