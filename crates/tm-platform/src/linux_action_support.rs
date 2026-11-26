//! Pure construction and parsing for Linux privileged action requests.
//!
//! Keeping this contract platform-neutral lets the parser and security tests
//! run on the Windows development host as well as on Linux.

use crate::actions::ServiceAction;
use tm_core::model::PriorityClass;

pub(crate) const PRIVILEGED_ARG_PREFIX: &str = "--linux-privileged-";
pub(crate) const ACTION_ARG: &str = "--linux-privileged-action=";

const UNIT_ARG: &str = "--linux-privileged-unit=";
const PID_ARG: &str = "--linux-privileged-pid=";
const IDENTITY_ARG: &str = "--linux-privileged-identity=";
const PRIORITY_ARG: &str = "--linux-privileged-priority=";
const AFFINITY_ARG: &str = "--linux-privileged-affinity=";
const ARGUMENT_ARG: &str = "--linux-privileged-argument=";
const ENVIRONMENT_ARG: &str = "--linux-privileged-environment=";

const MAX_HELPER_ARGS: usize = 512;
const MAX_ARGUMENT_BYTES: usize = 16 * 1024;
const MAX_TOTAL_FIELD_BYTES: usize = 128 * 1024;
const MAX_SERVICE_NAME_BYTES: usize = 256;

pub(crate) const FORWARDED_ENVIRONMENT_NAMES: &[&str] = &[
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "DESKTOP_SESSION",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrivilegedAction {
    Service {
        unit: String,
        action: ServiceAction,
    },
    Kill {
        pid: u32,
        start_ticks: i64,
    },
    Suspend {
        pid: u32,
        start_ticks: i64,
    },
    Resume {
        pid: u32,
        start_ticks: i64,
    },
    SetPriority {
        pid: u32,
        start_ticks: i64,
        priority: PriorityClass,
    },
    SetAffinity {
        pid: u32,
        start_ticks: i64,
        mask: u64,
    },
    Relaunch {
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
    },
}

impl PrivilegedAction {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Service { .. } => "service control",
            Self::Kill { .. } => "kill process",
            Self::Suspend { .. } => "suspend process",
            Self::Resume { .. } => "resume process",
            Self::SetPriority { .. } => "set priority",
            Self::SetAffinity { .. } => "set affinity",
            Self::Relaunch { .. } => "elevated relaunch",
        }
    }
}

pub(crate) fn is_privileged_helper_arg(argument: &str) -> bool {
    argument.starts_with(PRIVILEGED_ARG_PREFIX)
}

/// Build the exact typed argv for a protected helper invocation.
///
/// No value is ever concatenated into a shell command. Every action field is
/// carried by its own typed flag, and relaunch arguments remain distinct argv
/// entries even when they contain spaces or shell metacharacters.
///
/// The installed core service speaks its own framed protocol instead, so this
/// argv form remains the contract of the hidden helper mode and is exercised
/// by the tests below; the GUI never passes it to `pkexec` with a user-writable
/// image, and a relaunch through it stays refused.
#[cfg(test)]
pub(crate) fn invocation_args(action: &PrivilegedAction) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    match action {
        PrivilegedAction::Service { unit, action } => {
            validate_service_unit(unit)?;
            let verb = match action {
                ServiceAction::Start => "service-start",
                ServiceAction::Stop => "service-stop",
                ServiceAction::Restart => "service-restart",
            };
            args.push(format!("{ACTION_ARG}{verb}"));
            args.push(format!("{UNIT_ARG}{unit}"));
        }
        PrivilegedAction::Kill { pid, start_ticks } => {
            push_process_fields(&mut args, "kill", *pid, *start_ticks);
        }
        PrivilegedAction::Suspend { pid, start_ticks } => {
            push_process_fields(&mut args, "suspend", *pid, *start_ticks);
        }
        PrivilegedAction::Resume { pid, start_ticks } => {
            push_process_fields(&mut args, "resume", *pid, *start_ticks);
        }
        PrivilegedAction::SetPriority {
            pid,
            start_ticks,
            priority,
        } => {
            push_process_fields(&mut args, "set-priority", *pid, *start_ticks);
            args.push(format!(
                "{PRIORITY_ARG}{}",
                priority_argument(*priority).ok_or_else(|| {
                    "priority must be High, AboveNormal, Normal, BelowNormal, Low, or Realtime"
                        .to_string()
                })?
            ));
        }
        PrivilegedAction::SetAffinity {
            pid,
            start_ticks,
            mask,
        } => {
            if *mask == 0 {
                return Err("affinity mask must not be empty".into());
            }
            push_process_fields(&mut args, "set-affinity", *pid, *start_ticks);
            args.push(format!("{AFFINITY_ARG}{mask:#x}"));
        }
        PrivilegedAction::Relaunch {
            arguments,
            environment,
        } => {
            if arguments
                .iter()
                .any(|argument| is_privileged_helper_arg(argument))
            {
                return Err(
                    "relaunch arguments cannot contain the privileged helper prefix".into(),
                );
            }
            args.push(format!("{ACTION_ARG}relaunch"));
            push_relaunch_arguments(&mut args, arguments, environment)?;
        }
    }
    Ok(args)
}

#[cfg(test)]
fn push_process_fields(args: &mut Vec<String>, kind: &str, pid: u32, start_ticks: i64) {
    args.push(format!("{ACTION_ARG}{kind}"));
    args.push(format!("{PID_ARG}{pid}"));
    args.push(format!("{IDENTITY_ARG}{start_ticks}"));
}

#[cfg(test)]
fn push_relaunch_arguments(
    args: &mut Vec<String>,
    arguments: &[String],
    environment: &[(String, String)],
) -> Result<(), String> {
    if args
        .len()
        .saturating_add(arguments.len())
        .saturating_add(environment.len())
        > MAX_HELPER_ARGS
    {
        return Err("too many relaunch arguments".into());
    }
    let mut total = 0usize;
    for argument in arguments {
        check_field(argument)?;
        total = total.saturating_add(argument.len());
        args.push(format!("{ARGUMENT_ARG}{argument}"));
    }
    let mut names = std::collections::HashSet::new();
    for (name, value) in environment {
        validate_environment_name(name)?;
        if !names.insert(name.as_str()) {
            return Err(format!("duplicate environment variable {name}"));
        }
        check_field(value)?;
        total = total.saturating_add(name.len()).saturating_add(value.len());
        args.push(format!("{ENVIRONMENT_ARG}{name}={value}"));
    }
    if total > MAX_TOTAL_FIELD_BYTES {
        return Err("relaunch fields exceed the request size limit".into());
    }
    Ok(())
}

/// Parse the hidden helper contract. The action marker must be the first
/// argument, every following argument must be a recognized typed field, and
/// fields not belonging to the selected action are rejected.
pub(crate) fn parse_privileged_action(args: &[String]) -> Result<PrivilegedAction, String> {
    if args.is_empty() || args.len() > MAX_HELPER_ARGS {
        return Err("invalid privileged helper argument count".into());
    }
    check_field(&args[0])?;
    let kind = args[0]
        .strip_prefix(ACTION_ARG)
        .ok_or("privileged helper action must be the first argument".to_string())?;

    let mut unit = None;
    let mut pid = None;
    let mut start_ticks = None;
    let mut priority = None;
    let mut affinity = None;
    let mut arguments = Vec::new();
    let mut environment: Vec<(String, String)> = Vec::new();
    let mut total_bytes = 0usize;

    for raw in &args[1..] {
        check_field(raw)?;
        total_bytes = total_bytes.saturating_add(raw.len());
        if total_bytes > MAX_TOTAL_FIELD_BYTES {
            return Err("privileged helper fields exceed the size limit".into());
        }
        let field = RawField::parse(raw)?;
        match (kind, field) {
            ("service-start" | "service-stop" | "service-restart", RawField::Unit(value)) => {
                set_once(&mut unit, value, "service unit")?;
            }
            (
                "kill" | "suspend" | "resume" | "set-priority" | "set-affinity",
                RawField::Pid(value),
            ) => set_once(&mut pid, value, "pid")?,
            (
                "kill" | "suspend" | "resume" | "set-priority" | "set-affinity",
                RawField::Identity(value),
            ) => set_once(&mut start_ticks, value, "process identity")?,
            ("set-priority", RawField::Priority(value)) => {
                set_once(&mut priority, value, "priority")?;
            }
            ("set-affinity", RawField::Affinity(value)) => {
                set_once(&mut affinity, value, "affinity")?;
            }
            ("relaunch", RawField::Argument(value)) => arguments.push(value),
            ("relaunch", RawField::Environment(name, value)) => {
                if environment.iter().any(|(existing, _)| existing == &name) {
                    return Err(format!("duplicate environment variable {name}"));
                }
                environment.push((name, value));
            }
            _ => return Err(format!("field is not valid for privileged action {kind}")),
        }
    }

    match kind {
        "service-start" => service(unit, ServiceAction::Start),
        "service-stop" => service(unit, ServiceAction::Stop),
        "service-restart" => service(unit, ServiceAction::Restart),
        "kill" => process_action(kind, pid, start_ticks)
            .map(|(pid, start_ticks)| PrivilegedAction::Kill { pid, start_ticks }),
        "suspend" => process_action(kind, pid, start_ticks)
            .map(|(pid, start_ticks)| PrivilegedAction::Suspend { pid, start_ticks }),
        "resume" => process_action(kind, pid, start_ticks)
            .map(|(pid, start_ticks)| PrivilegedAction::Resume { pid, start_ticks }),
        "set-priority" => {
            let (pid, start_ticks) = process_action(kind, pid, start_ticks)?;
            let priority = priority.ok_or("set-priority requires a priority".to_string())?;
            let priority = priority_from_argument(priority)?;
            Ok(PrivilegedAction::SetPriority {
                pid,
                start_ticks,
                priority,
            })
        }
        "set-affinity" => {
            let (pid, start_ticks) = process_action(kind, pid, start_ticks)?;
            let mask = affinity.ok_or("set-affinity requires a mask".to_string())?;
            if mask == 0 {
                return Err("affinity mask must not be empty".into());
            }
            Ok(PrivilegedAction::SetAffinity {
                pid,
                start_ticks,
                mask,
            })
        }
        "relaunch" => {
            if arguments
                .iter()
                .any(|argument| is_privileged_helper_arg(argument))
            {
                return Err(
                    "relaunch arguments cannot contain the privileged helper prefix".into(),
                );
            }
            Ok(PrivilegedAction::Relaunch {
                arguments,
                environment,
            })
        }
        _ => Err(format!("unknown privileged action {kind}")),
    }
}

enum RawField {
    Unit(String),
    Pid(u32),
    Identity(i64),
    Priority(String),
    Affinity(u64),
    Argument(String),
    Environment(String, String),
}

impl RawField {
    fn parse(raw: &str) -> Result<Self, String> {
        if let Some(value) = raw.strip_prefix(UNIT_ARG) {
            return Ok(Self::Unit(value.to_string()));
        }
        if let Some(value) = raw.strip_prefix(PID_ARG) {
            let pid = value
                .parse::<u32>()
                .map_err(|_| "pid is not a valid positive integer".to_string())?;
            if pid == 0 {
                return Err("pid must be greater than zero".into());
            }
            if i32::try_from(pid).is_err() {
                return Err("pid exceeds the Linux pid_t range".into());
            }
            return Ok(Self::Pid(pid));
        }
        if let Some(value) = raw.strip_prefix(IDENTITY_ARG) {
            let ticks = value
                .parse::<i64>()
                .map_err(|_| "process identity is not a valid integer".to_string())?;
            if ticks <= 0 {
                return Err("process identity must be positive".into());
            }
            return Ok(Self::Identity(ticks));
        }
        if let Some(value) = raw.strip_prefix(PRIORITY_ARG) {
            return Ok(Self::Priority(value.to_string()));
        }
        if let Some(value) = raw.strip_prefix(AFFINITY_ARG) {
            let value = value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
                .unwrap_or(value);
            let mask = u64::from_str_radix(value, 16)
                .map_err(|_| "affinity mask is not valid hexadecimal".to_string())?;
            return Ok(Self::Affinity(mask));
        }
        if let Some(value) = raw.strip_prefix(ARGUMENT_ARG) {
            return Ok(Self::Argument(value.to_string()));
        }
        if let Some(value) = raw.strip_prefix(ENVIRONMENT_ARG) {
            let (name, value) = value
                .split_once('=')
                .ok_or("environment field must be NAME=value".to_string())?;
            validate_environment_name(name)?;
            return Ok(Self::Environment(name.to_string(), value.to_string()));
        }
        Err("unknown privileged helper field".into())
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(format!("duplicate {name} field"));
    }
    Ok(())
}

fn service(unit: Option<String>, action: ServiceAction) -> Result<PrivilegedAction, String> {
    let unit = unit.ok_or("service action requires a unit".to_string())?;
    validate_service_unit(&unit)?;
    Ok(PrivilegedAction::Service { unit, action })
}

fn process_action(
    kind: &str,
    pid: Option<u32>,
    start_ticks: Option<i64>,
) -> Result<(u32, i64), String> {
    Ok((
        pid.ok_or_else(|| format!("{kind} requires a pid"))?,
        start_ticks.ok_or_else(|| format!("{kind} requires a process identity"))?,
    ))
}

fn check_field(value: &str) -> Result<(), String> {
    if value.len() > MAX_ARGUMENT_BYTES {
        return Err("privileged helper field is too long".into());
    }
    if value.contains('\0') {
        return Err("privileged helper field contains NUL".into());
    }
    Ok(())
}

pub(crate) fn validate_service_unit(unit: &str) -> Result<(), String> {
    if unit.is_empty() || unit.len() > MAX_SERVICE_NAME_BYTES || !unit.ends_with(".service") {
        return Err("invalid systemd service name".into());
    }
    if unit.starts_with('-')
        || !unit.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@' | b':' | b'\\')
        })
    {
        return Err("systemd service name contains unsupported characters".into());
    }
    Ok(())
}

pub(crate) fn validate_environment_name(name: &str) -> Result<(), String> {
    if FORWARDED_ENVIRONMENT_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(format!("environment variable {name} is not allowed"))
    }
}

#[cfg(test)]
fn priority_argument(priority: PriorityClass) -> Option<&'static str> {
    match priority {
        PriorityClass::High => Some("high"),
        PriorityClass::AboveNormal => Some("above-normal"),
        PriorityClass::Normal => Some("normal"),
        PriorityClass::BelowNormal => Some("below-normal"),
        PriorityClass::Low => Some("low"),
        PriorityClass::Realtime => Some("realtime"),
        PriorityClass::Unknown => None,
    }
}

fn priority_from_argument(value: String) -> Result<PriorityClass, String> {
    match value.as_str() {
        "high" => Ok(PriorityClass::High),
        "above-normal" => Ok(PriorityClass::AboveNormal),
        "normal" => Ok(PriorityClass::Normal),
        "below-normal" => Ok(PriorityClass::BelowNormal),
        "low" => Ok(PriorityClass::Low),
        "realtime" => Ok(PriorityClass::Realtime),
        _ => Err("unknown priority class".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<PrivilegedAction, String> {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        parse_privileged_action(&args)
    }

    #[test]
    fn service_actions_are_exact_and_typed() {
        assert_eq!(
            parse(&[
                "--linux-privileged-action=service-restart",
                "--linux-privileged-unit=dbus.service",
            ]),
            Ok(PrivilegedAction::Service {
                unit: "dbus.service".into(),
                action: ServiceAction::Restart,
            })
        );
        assert!(
            parse(&[
                "--linux-privileged-action=service-start",
                "--linux-privileged-unit=dbus.service",
                "--linux-privileged-pid=42",
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "--linux-privileged-action=service-start",
                "--linux-privileged-unit=dbus.service",
                "--linux-privileged-unit=ssh.service",
            ])
            .is_err()
        );
    }

    #[test]
    fn service_names_cannot_become_systemctl_options_or_units_outside_services() {
        assert!(validate_service_unit("dbus.service").is_ok());
        assert!(validate_service_unit("getty@tty1.service").is_ok());
        for invalid in [
            "--help.service",
            "dbus.service\n--now",
            "../dbus.service",
            "dbus.socket",
            "dbus service",
        ] {
            assert!(validate_service_unit(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn process_actions_require_positive_pid_and_identity() {
        assert_eq!(
            parse(&[
                "--linux-privileged-action=kill",
                "--linux-privileged-pid=42",
                "--linux-privileged-identity=1234",
            ]),
            Ok(PrivilegedAction::Kill {
                pid: 42,
                start_ticks: 1234,
            })
        );
        for invalid in [
            vec![
                "--linux-privileged-action=kill",
                "--linux-privileged-pid=0",
                "--linux-privileged-identity=1234",
            ],
            vec![
                "--linux-privileged-action=kill",
                "--linux-privileged-pid=42",
                "--linux-privileged-identity=0",
            ],
            vec![
                "--linux-privileged-action=suspend",
                "--linux-privileged-pid=42",
            ],
            vec![
                "--linux-privileged-action=kill",
                "--linux-privileged-pid=4294967295",
                "--linux-privileged-identity=1234",
            ],
        ] {
            let args: Vec<String> = invalid.into_iter().map(ToString::to_string).collect();
            assert!(parse_privileged_action(&args).is_err());
        }
    }

    #[test]
    fn priority_and_affinity_round_trip_through_the_cli_contract() {
        let action = PrivilegedAction::SetPriority {
            pid: 42,
            start_ticks: 1234,
            priority: PriorityClass::AboveNormal,
        };
        let args = invocation_args(&action).unwrap();
        assert_eq!(parse_privileged_action(&args), Ok(action));

        let action = PrivilegedAction::SetAffinity {
            pid: 42,
            start_ticks: 1234,
            mask: 0b1011,
        };
        let args = invocation_args(&action).unwrap();
        assert_eq!(parse_privileged_action(&args), Ok(action));
    }

    #[test]
    fn relaunch_cannot_reenter_the_helper_protocol() {
        assert!(
            parse(&[
                "--linux-privileged-action=relaunch",
                "--linux-privileged-argument=--linux-privileged-action=kill",
            ])
            .is_err()
        );
    }

    #[test]
    fn relaunch_preserves_each_argument_without_shell_joining() {
        let action = PrivilegedAction::Relaunch {
            arguments: vec![
                "--tab=users".into(),
                "two words".into(),
                "quote\"and\\slash".into(),
                "$HOME; rm -rf /".into(),
            ],
            environment: Vec::new(),
        };
        let args = invocation_args(&action).unwrap();
        assert_eq!(parse_privileged_action(&args), Ok(action));
        assert!(!args.iter().any(|arg| arg == "sh" || arg == "-c"));
    }

    #[test]
    fn environment_forwarding_is_an_allowlist_without_duplicates() {
        assert!(validate_environment_name("DISPLAY").is_ok());
        for rejected in ["PATH", "LD_PRELOAD", "HOME", "TASKMAN_CONFIG_DIR"] {
            assert!(validate_environment_name(rejected).is_err(), "{rejected}");
        }
        assert!(
            parse(&[
                "--linux-privileged-action=relaunch",
                "--linux-privileged-environment=DISPLAY=:0",
                "--linux-privileged-environment=DISPLAY=:1",
            ])
            .is_err()
        );
    }

    #[test]
    fn malformed_and_unknown_helper_fields_are_rejected() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--linux-privileged-unit=dbus.service"]).is_err());
        assert!(
            parse(&[
                "--linux-privileged-action=launch",
                "--linux-privileged-argument=/usr/bin/id",
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "--linux-privileged-action=resume",
                "--linux-privileged-pid=42",
                "--linux-privileged-identity=1234",
                "positional",
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "--linux-privileged-action=resume",
                "--linux-privileged-pid=42",
                "--linux-privileged-priority=high",
            ])
            .is_err()
        );
    }
}
