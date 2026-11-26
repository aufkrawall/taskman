//! systemd services via `systemctl` (no dbus dependency; graceful fallback).

use super::launch;
use crate::actions::ServiceAction;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tm_core::error::{Result, TmError};
use tm_core::model::{ServiceInfo, ServiceStatus};

/// The read-only unit query is one daemon round trip, not work systemd
/// performs, so it gets the same order of bound as `loginctl`.
const QUERY_DEADLINE: Duration = Duration::from_secs(5);
/// Service control waits for a job the daemon runs, so it needs room for a
/// slow unit. The bound only exists so a wedged systemd or D-Bus cannot park
/// one of the two action lanes forever; systemd itself reports the job failure.
const CONTROL_DEADLINE: Duration = Duration::from_secs(30);

pub fn available() -> bool {
    Path::new("/run/systemd/system").is_dir() && systemctl_path().is_ok()
}

pub fn list_systemd_units() -> Result<Vec<ServiceInfo>> {
    let out = run_systemctl(
        &systemctl_path()?,
        &[
            "list-units",
            "--type=service",
            "--all",
            "--no-pager",
            "--no-legend",
            "--plain",
        ],
        QUERY_DEADLINE,
        "systemctl list-units",
    )?;
    // Without systemd as PID 1 (containers, WSL without systemd, other
    // inits) systemctl fails with empty stdout; that is "unavailable", not
    // an empty service list.
    check_exit(out.status, &out.stderr)?;
    let mut units = parse_units(&String::from_utf8_lossy(&out.stdout));
    attach_unit_properties(&mut units);
    Ok(units)
}

/// Properties `list-units` does not print: the main PID, the enablement state
/// and the account. One `systemctl show` for every listed unit, so the cost is
/// a second daemon round trip, not one per unit.
///
/// A failed query keeps the list it enriches: the PID column then reads "—"
/// for every row, which is true (it was not read), instead of losing the
/// whole service list over an optional column.
fn attach_unit_properties(units: &mut [ServiceInfo]) {
    if units.is_empty() {
        return;
    }
    let Ok(systemctl) = systemctl_path() else {
        return;
    };
    let mut arguments: Vec<&str> = vec![
        "show",
        "--no-pager",
        // One `--property` per field, which every systemctl accepts (the
        // comma form is not understood by every systemd tool).
        "--property=Id",
        "--property=MainPID",
        "--property=UnitFileState",
        "--property=User",
        "--property=DynamicUser",
        "--",
    ];
    arguments.extend(units.iter().map(|unit| unit.name.as_str()));
    let output = match run_systemctl(&systemctl, &arguments, QUERY_DEADLINE, "systemctl show") {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            tracing::debug!(status = %output.status, "systemctl show failed; service PIDs stay unknown");
            return;
        }
        Err(error) => {
            tracing::debug!(%error, "systemctl show failed; service PIDs stay unknown");
            return;
        }
    };
    let properties = parse_show(&String::from_utf8_lossy(&output.stdout));
    for unit in units.iter_mut() {
        if let Some(props) = properties.get(unit.name.as_str()) {
            apply_properties(unit, props);
        }
    }
}

/// The `systemctl show` properties this backend reads for one unit.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct UnitProperties {
    main_pid: Option<u32>,
    unit_file_state: String,
    user: String,
    dynamic_user: bool,
}

/// `systemctl show` output: one `Key=Value` block per unit, separated by
/// blank lines, each carrying its `Id=`.
fn parse_show(text: &str) -> std::collections::HashMap<String, UnitProperties> {
    let mut out = std::collections::HashMap::new();
    let mut id: Option<String> = None;
    let mut props = UnitProperties::default();
    let mut flush = |id: &mut Option<String>, props: &mut UnitProperties| {
        if let Some(id) = id.take() {
            out.insert(id, std::mem::take(props));
        } else {
            *props = UnitProperties::default();
        }
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&mut id, &mut props);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Id" => id = Some(value.to_string()),
            // systemd reports 0 for a unit without a running main process.
            "MainPID" => props.main_pid = value.parse().ok().filter(|pid: &u32| *pid != 0),
            "UnitFileState" => props.unit_file_state = value.to_string(),
            "User" => props.user = value.to_string(),
            "DynamicUser" => props.dynamic_user = value == "yes",
            _ => {}
        }
    }
    flush(&mut id, &mut props);
    out
}

fn apply_properties(unit: &mut ServiceInfo, props: &UnitProperties) {
    unit.pid = props.main_pid;
    unit.startup_type = props.unit_file_state.clone();
    // A system unit without `User=` runs as root; a `DynamicUser=` unit gets
    // an ephemeral account whose name is only known while it runs, so it is
    // left blank rather than guessed.
    unit.account = if !props.user.is_empty() {
        props.user.clone()
    } else if props.dynamic_user {
        String::new()
    } else {
        "root".to_string()
    };
}

/// Run one `systemctl` invocation under an absolute deadline.
///
/// `Command::output()` waits for the child without a bound, and `systemctl`
/// blocks while its D-Bus connection waits for a daemon that may be wedged.
/// The shared pidfd/`poll` wait turns that into a bounded failure: a child
/// that outlives its deadline is killed and reaped, and the error names the
/// program and context instead of freezing an action lane. Its output is
/// collected under the same deadline, so a process that inherited the helper's
/// pipes cannot hold the read open either.
fn run_systemctl(
    systemctl: &Path,
    arguments: &[&str],
    deadline: Duration,
    context: &'static str,
) -> Result<std::process::Output> {
    let mut child = Command::new(systemctl)
        .args(arguments)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| TmError::platform(context, format!("could not start: {error}")))?;
    let deadline = Instant::now() + deadline;
    let result = launch::wait_with_output(&mut child, deadline, "systemctl");
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result.map_err(|error| TmError::platform(context, error.to_string()))
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
            // The unit's `Description=` is what Windows calls the display
            // name ("D-Bus System Message Bus"), and that is the column the
            // Services page labels Description.
            display_name: description.to_string(),
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

pub(crate) fn control_unit_direct(unit: &str, action: ServiceAction) -> Result<()> {
    use ServiceAction::*;
    if let Err(error) = crate::linux_action_support::validate_service_unit(unit) {
        return Err(TmError::platform("systemctl control service", error));
    }
    let verb = match action {
        Start => "start",
        Stop => "stop",
        Restart => "restart",
    };
    let systemctl = systemctl_path()?;
    let out = run_systemctl(
        &systemctl,
        &["--no-ask-password", verb, unit],
        CONTROL_DEADLINE,
        "systemctl control service",
    )?;
    if out.status.success() {
        tracing::info!(unit, verb, "systemd unit controlled");
        Ok(())
    } else {
        Err(control_failure(&out, verb, unit))
    }
}

/// Absolute directories that hold the systemd client tools this backend uses.
///
/// The two profile paths are the NixOS locations, the same ones
/// `PKEXEC_CANDIDATES` and `fc-match` resolution carry: without them a NixOS
/// host resolves its Polkit helper but reports the whole service surface as
/// unavailable because `/usr/bin` and `/bin` do not hold `systemctl` there.
/// `systemctl` and `loginctl` both resolve from this one list, so the two cannot
/// drift apart the way their separate copies did.
pub(super) const SYSTEM_TOOL_DIRS: [&str; 4] = [
    "/usr/bin",
    "/bin",
    "/run/current-system/sw/bin",
    "/nix/var/nix/profiles/default/bin",
];

/// Resolve a systemd client tool from `SYSTEM_TOOL_DIRS`.
///
/// Every candidate still has to pass the rule in `launch::trusted_system_program`:
/// an executable regular file whose own path and every ancestor are root-owned
/// and not group/other writable, so the added locations cannot widen what may be
/// executed or elevated.
pub(super) fn trusted_system_tool(tool: &str) -> Result<PathBuf> {
    for directory in SYSTEM_TOOL_DIRS {
        let candidate = Path::new(directory).join(tool);
        if candidate.is_file()
            && let Ok(path) = launch::trusted_system_program(&candidate)
        {
            return Ok(path);
        }
    }
    Err(TmError::Unsupported(
        "required system utility is not installed at a trusted path",
    ))
}

fn systemctl_path() -> Result<PathBuf> {
    trusted_system_tool("systemctl")
        .map_err(|_| TmError::Unsupported("systemd service control (trusted systemctl path)"))
}

fn control_failure(output: &std::process::Output, verb: &str, unit: &str) -> TmError {
    let detail = first_line(&output.stderr)
        .or_else(|| first_line(&output.stdout))
        .unwrap_or_else(|| output.status.to_string());
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    if is_permission_failure(&combined) {
        TmError::platform(
            "systemctl control service",
            format!("{verb} {unit} failed: {detail}; authentication is required"),
        )
    } else {
        TmError::platform(
            "systemctl control service",
            format!("{verb} {unit} failed: {detail}"),
        )
    }
}

fn first_line(bytes: &[u8]) -> Option<String> {
    bytes
        .split(|byte| *byte == b'\n')
        .map(|line| String::from_utf8_lossy(line).trim().to_string())
        .find(|line| !line.is_empty())
}

fn is_permission_failure(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "access denied",
        "permission denied",
        "operation not permitted",
        "interactive authentication required",
        "authentication is required",
        "not authorized",
        "must be root",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
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
    fn listed_description_doubles_as_the_display_name() {
        let units = parse_units("dbus.service loaded active running D-Bus System Message Bus\n");
        assert_eq!(units[0].display_name, "D-Bus System Message Bus");
    }

    #[test]
    fn show_blocks_supply_pid_state_and_account_per_unit() {
        let props = parse_show(
            "Id=dbus-broker.service\nUnitFileState=disabled\nMainPID=704\nUser=\nDynamicUser=no\n\n\
             Id=sshd.service\nUnitFileState=enabled\nMainPID=0\nUser=\nDynamicUser=no\n\n\
             Id=systemd-resolved.service\nMainPID=512\nUser=systemd-resolve\nDynamicUser=no\n\n\
             Id=dyn.service\nMainPID=9\nUser=\nDynamicUser=yes\n",
        );
        let mut unit = parse_units("dbus-broker.service loaded active running Bus\n").remove(0);
        apply_properties(&mut unit, &props["dbus-broker.service"]);
        assert_eq!(unit.pid, Some(704));
        assert_eq!(unit.startup_type, "disabled");
        assert_eq!(unit.account, "root");

        // A stopped unit reports MainPID=0, which is "no process", not PID 0.
        assert_eq!(props["sshd.service"].main_pid, None);
        assert_eq!(props["systemd-resolved.service"].user, "systemd-resolve");
        apply_properties(&mut unit, &props["dyn.service"]);
        assert_eq!(unit.account, "", "a dynamic user is not guessed");
    }

    #[test]
    fn systemctl_runner_enforces_its_absolute_deadline() {
        let started = Instant::now();
        let error = run_systemctl(
            Path::new("/bin/sh"),
            &["-c", "while :; do :; done"],
            Duration::from_millis(100),
            "systemctl test",
        )
        .expect_err("the busy child must hit the deadline");
        assert!(
            error
                .to_string()
                .contains("systemctl exceeded its deadline"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
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

    #[test]
    fn permission_failures_are_typed_for_the_polkit_fallback() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"Interactive authentication required.\n".to_vec(),
        };
        let error = control_failure(&output, "restart", "dbus.service");
        assert!(super::super::polkit::is_permission_denied(&error));
        assert!(error.to_string().contains("authentication is required"));
        let later_line = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"Failed to restart dbus.service.\nAccess denied\n".to_vec(),
        };
        let error = control_failure(&later_line, "restart", "dbus.service");
        assert!(super::super::polkit::is_permission_denied(&error));
        assert!(!is_permission_failure(
            "Unit missing.service could not be found."
        ));
    }
}
