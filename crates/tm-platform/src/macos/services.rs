//! launchd service listing via `launchctl` (user domain, best-effort).

use std::process::Command;
use tm_core::error::{Result, TmError};
use tm_core::model::{ServiceInfo, ServiceStatus};

pub fn list_launchctl() -> Result<Vec<ServiceInfo>> {
    let out = Command::new("launchctl")
        .args(["list"])
        .output()
        .map_err(|e| TmError::platform("spawn launchctl", e.to_string()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        // PID  Status  Label
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let pid = fields[0].parse::<u32>().ok();
        let label = fields[2].to_string();
        out.push(ServiceInfo {
            name: label.clone(),
            display_name: label,
            description: format!("status {}", fields[1]),
            pid,
            status: if pid.is_some() {
                ServiceStatus::Running
            } else {
                ServiceStatus::Stopped
            },
            group: String::new(),
            startup_type: String::new(),
            account: String::new(),
        });
    }
    Ok(out)
}

pub fn control_label(label: &str, action: super::super::actions::ServiceAction) -> Result<()> {
    use super::super::actions::ServiceAction::*;
    let verb = match action {
        Start => "start",
        Stop => "stop",
        Restart => "kickstart",
    };
    let mut cmd = Command::new("launchctl");
    if action == Restart {
        cmd.args(["kickstart", "-k", &format!("gui/$(id -u)/{label}")]);
    } else {
        cmd.arg(verb).arg(label);
    }
    let status = cmd
        .status()
        .map_err(|e| TmError::platform("spawn launchctl", e.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(TmError::platform(
            "launchctl",
            format!("{verb} {label} failed"),
        ))
    }
}
