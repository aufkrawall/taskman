//! Login sessions and their process association.
//!
//! Session enumeration uses `loginctl` only when systemd is the running
//! service manager. Disconnect/logoff controls remain unavailable: safely
//! terminating a session is not a read-only metadata operation. Per-process
//! session IDs come from the same validated cgroup read used by
//! `desktop_session`, so the collector does not reread procfs for them.

use super::launch;
use std::path::Path;
use std::process::{Child, Command, Output};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tm_core::error::{Result, TmError};
use tm_core::model::{UserSession, UserSessionState};

const LOGINCTL_DEADLINE: Duration = Duration::from_secs(3);
const LISTING_RETRY: Duration = Duration::from_secs(30);
static LISTING_RETRY_AFTER: Mutex<Option<Instant>> = Mutex::new(None);

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

/// Whether this host can enumerate logind sessions at all.
///
/// This is a capability answer, so it must not fold in the retry cooldown: a
/// single failed `loginctl` run would otherwise claim the platform cannot do
/// sessions and blank the unrelated per-process Session column that the
/// collector derives from systemd cgroups.
pub fn available() -> bool {
    Path::new("/run/systemd/system").is_dir()
        && Path::new("/run/dbus/system_bus_socket").exists()
        && loginctl_path().is_some()
}

pub fn list_sessions() -> Result<Vec<UserSession>> {
    if !available() {
        return Err(TmError::Unsupported("user sessions (systemd/loginctl)"));
    }
    if !listing_available(Instant::now()) {
        return Err(TmError::platform(
            "loginctl list-sessions",
            "a recent loginctl failure is still cooling down",
        ));
    }
    let loginctl =
        loginctl_path().ok_or_else(|| TmError::Unsupported("user sessions (loginctl path)"))?;
    let deadline = Instant::now() + LOGINCTL_DEADLINE;
    let output = run_loginctl(
        &loginctl,
        &["list-sessions", "--no-legend", "--no-pager"],
        deadline,
    )
    .map_err(|error| {
        note_listing_failure();
        TmError::platform("loginctl list-sessions", error.to_string())
    })?;
    if !output.status.success() {
        note_listing_failure();
        return Err(TmError::platform(
            "loginctl list-sessions",
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    note_listing_success();

    let rows = parse_session_list(&String::from_utf8_lossy(&output.stdout));
    let boot_epoch_s = system_boot_epoch_s();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        // A session can disappear between list-sessions and show-session.
        // Keep the stable list fields and report its state as unknown rather
        // than dropping a real session or failing the entire page.
        let properties = if Instant::now() < deadline {
            show_session(&loginctl, row.id, deadline).unwrap_or_default()
        } else {
            SessionProperties::default()
        };
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
            mem_bytes: 0,
            process_count: 0,
        });
    }
    Ok(out)
}

fn show_session(loginctl: &Path, id: u32, deadline: Instant) -> Option<SessionProperties> {
    let id = id.to_string();
    let output = run_loginctl(
        loginctl,
        &[
            "show-session",
            "--no-pager",
            "--property=Name,State,TimestampMonotonic",
            &id,
        ],
        deadline,
    )
    .ok()?;
    output
        .status
        .success()
        .then(|| parse_session_properties(&String::from_utf8_lossy(&output.stdout)))
}

fn run_loginctl(loginctl: &Path, args: &[&str], deadline: Instant) -> std::io::Result<Output> {
    let mut child = Command::new(loginctl)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Err(error) = wait_for_exit(&mut child, deadline) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    child.wait_with_output()
}

fn wait_for_exit(child: &mut Child, deadline: Instant) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
        if fd >= 0 {
            let result = wait_pidfd(fd as libc::c_int, deadline);
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
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "loginctl exceeded its deadline",
            ));
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
}

#[cfg(target_os = "linux")]
fn wait_pidfd(fd: libc::c_int, deadline: Instant) -> std::io::Result<()> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "loginctl exceeded its deadline",
            ));
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

fn listing_available(now: Instant) -> bool {
    listing_retry_available(
        *LISTING_RETRY_AFTER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        now,
    )
}

fn listing_retry_available(retry_after: Option<Instant>, now: Instant) -> bool {
    retry_after.is_none_or(|retry_after| now >= retry_after)
}

fn note_listing_failure() {
    *LISTING_RETRY_AFTER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now() + LISTING_RETRY);
}

fn note_listing_success() {
    *LISTING_RETRY_AFTER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
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

/// Wall-clock boot time used to turn logind's monotonic login timestamp into
/// the model's Unix timestamp. Process-to-session joins are collected from the
/// same validated cgroup evidence used for desktop-role classification.
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

/// `loginctl` at a trusted absolute path.
///
/// The caller executes this binary, so it is held to the same policy as
/// `systemctl` and `pkexec`: a canonical absolute path whose every ancestor is
/// root-owned and not group/other writable. A root-owned but writable tool, or
/// one reachable through a writable directory, is refused instead of run.
fn loginctl_path() -> Option<std::path::PathBuf> {
    for candidate in ["/usr/bin/loginctl", "/bin/loginctl"] {
        let candidate = Path::new(candidate);
        if candidate.is_file()
            && let Ok(path) = launch::trusted_system_program(candidate)
        {
            return Some(path);
        }
    }
    None
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
    fn failed_session_listing_has_a_bounded_retry_window() {
        let now = Instant::now();
        assert!(listing_retry_available(None, now));
        assert!(!listing_retry_available(Some(now + LISTING_RETRY), now));
        assert!(listing_retry_available(Some(now), now));
    }

    #[test]
    fn command_runner_enforces_its_absolute_deadline() {
        let started = Instant::now();
        let error = run_loginctl(
            Path::new("/bin/sh"),
            &["-c", "while :; do :; done"],
            started + Duration::from_millis(100),
        )
        .expect_err("the busy child must hit the deadline");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
