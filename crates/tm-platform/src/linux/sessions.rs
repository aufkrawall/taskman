//! Login sessions and their process association.
//!
//! Session enumeration uses `loginctl` only when systemd is the running
//! service manager. Disconnect/logoff controls remain unavailable: safely
//! terminating a session is not a read-only metadata operation.

use std::path::Path;
use std::process::Command;
use tm_core::error::{Result, TmError};
use tm_core::model::{UserSession, UserSessionState};

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionListRow {
    id: u32,
    user: String,
}

#[derive(Debug, Default)]
struct SessionProperties {
    name: Option<String>,
    state: Option<String>,
    timestamp_monotonic_us: Option<u64>,
}

pub fn available() -> bool {
    Path::new("/run/systemd/system").is_dir() && command_on_path("loginctl")
}

pub fn list_sessions() -> Result<Vec<UserSession>> {
    if !available() {
        return Err(TmError::Unsupported("user sessions (systemd/loginctl)"));
    }
    let output = Command::new("loginctl")
        .args(["list-sessions", "--no-legend", "--no-pager"])
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| TmError::platform("loginctl list-sessions", error.to_string()))?;
    if !output.status.success() {
        return Err(TmError::platform(
            "loginctl list-sessions",
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let rows = parse_session_list(&String::from_utf8_lossy(&output.stdout));
    let boot_epoch_s = system_boot_epoch_s();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        // A session can disappear between list-sessions and show-session.
        // Keep the stable list fields and report its state as unknown rather
        // than dropping a real session or failing the entire page.
        let properties = show_session(row.id).unwrap_or_default();
        let user = properties
            .name
            .filter(|name| !name.is_empty())
            .unwrap_or(row.user);
        if user.is_empty() {
            continue;
        }
        out.push(UserSession {
            id: row.id,
            user,
            domain: None,
            state: properties
                .state
                .as_deref()
                .map(map_state)
                .unwrap_or(UserSessionState::Unknown),
            logon_epoch_s: logon_epoch_s(boot_epoch_s, properties.timestamp_monotonic_us),
            cpu_pct: 0.0,
            mem_bytes: 0,
            process_count: 0,
        });
    }
    Ok(out)
}

fn show_session(id: u32) -> Option<SessionProperties> {
    let output = Command::new("loginctl")
        .arg("show-session")
        .args(["--no-pager", "--property=Name,State,TimestampMonotonic"])
        .arg(id.to_string())
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_session_properties(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_session_list(text: &str) -> Vec<SessionListRow> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_ascii_whitespace().collect();
            let id = fields.first()?.parse().ok()?;
            Some(SessionListRow {
                id,
                user: fields.get(2).copied().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

fn parse_session_properties(text: &str) -> SessionProperties {
    let mut out = SessionProperties::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "Name" => out.name = Some(value.to_string()),
            "State" => out.state = Some(value.to_string()),
            "TimestampMonotonic" => out.timestamp_monotonic_us = value.parse().ok(),
            _ => {}
        }
    }
    out
}

fn map_state(state: &str) -> UserSessionState {
    match state {
        "active" => UserSessionState::Active,
        "online" => UserSessionState::Connected,
        "offline" => UserSessionState::Disconnected,
        "closing" => UserSessionState::Down,
        _ => UserSessionState::Unknown,
    }
}

/// Exact `session-<id>.scope` cgroup membership is the reliable process →
/// logind-session join. `/proc/<pid>/stat` field 6 is an unrelated POSIX
/// session id and must not be used for the Users page.
pub fn process_session_id(pid: u32) -> Option<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()
        .and_then(|text| parse_session_scope(&text))
}

fn parse_session_scope(cgroup: &str) -> Option<u32> {
    cgroup.lines().find_map(|line| {
        line.split('/').find_map(|component| {
            let id = component.strip_prefix("session-")?.strip_suffix(".scope")?;
            id.parse().ok()
        })
    })
}

fn system_boot_epoch_s() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines().find_map(|line| {
        line.strip_prefix("btime ")
            .and_then(|value| value.split_ascii_whitespace().next())
            .and_then(|value| value.parse().ok())
    })
}

fn logon_epoch_s(boot_epoch_s: Option<u64>, monotonic_us: Option<u64>) -> Option<i64> {
    let boot = i64::try_from(boot_epoch_s?).ok()?;
    let monotonic_us = monotonic_us.filter(|micros| *micros > 0)?;
    let elapsed = i64::try_from(monotonic_us.checked_div(1_000_000)?).ok()?;
    boot.checked_add(elapsed)
}

fn command_on_path(command: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(command);
        candidate
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_list_parses_ids_and_users() {
        let rows = parse_session_list(
            "\
 12 1000 julian seat0 tty2
  3 0 root - -
",
        );
        assert_eq!(
            rows,
            vec![
                SessionListRow {
                    id: 12,
                    user: "julian".into(),
                },
                SessionListRow {
                    id: 3,
                    user: "root".into(),
                },
            ]
        );
    }

    #[test]
    fn show_session_fields_map_to_user_session() {
        let properties = parse_session_properties(
            "\
Name=julian@example.test
State=online
TimestampMonotonic=1234567890
",
        );
        assert_eq!(properties.name.as_deref(), Some("julian@example.test"));
        assert_eq!(properties.state.as_deref(), Some("online"));
        assert_eq!(properties.timestamp_monotonic_us, Some(1_234_567_890));
    }

    #[test]
    fn logind_states_map_to_the_shared_model_without_guessing() {
        assert_eq!(map_state("active"), UserSessionState::Active);
        assert_eq!(map_state("online"), UserSessionState::Connected);
        assert_eq!(map_state("offline"), UserSessionState::Disconnected);
        assert_eq!(map_state("closing"), UserSessionState::Down);
        assert_eq!(map_state("something-new"), UserSessionState::Unknown);
    }

    #[test]
    fn logon_time_is_boot_time_plus_monotonic_seconds() {
        assert_eq!(
            logon_epoch_s(Some(1_700_000_000), Some(12_345_678)),
            Some(1_700_000_012)
        );
        assert_eq!(logon_epoch_s(Some(1_700_000_000), Some(0)), None);
        assert_eq!(logon_epoch_s(None, Some(1)), None);
    }

    #[test]
    fn process_session_uses_an_exact_systemd_scope_component() {
        let cgroup = "\
0::/user.slice/user-1000.slice/session-12.scope
";
        assert_eq!(parse_session_scope(cgroup), Some(12));
        assert_eq!(
            parse_session_scope("0::/user.slice/user-1000.slice/app.slice/session-7.scope/app"),
            Some(7)
        );
        assert_eq!(
            parse_session_scope("0::/user.slice/user-1000.slice/session-12.scope.extra"),
            None
        );
        assert_eq!(
            parse_session_scope("1:name=systemd:/user.slice/user-1000.slice/session-9.scope"),
            Some(9)
        );
    }
}
