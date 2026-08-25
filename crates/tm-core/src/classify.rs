//! Process classification into the three Windows-Task-Manager-like groups:
//! Apps / Background / System. Pure logic, unit-tested.

use crate::model::ProcCategory;

/// Names of processes that belong to the "Windows/system" group even without windows.
const SYSTEM_CRITICAL: &[&str] = &[
    "system",
    "registry",
    "memory compression",
    "secure system",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "services.exe",
    "lsass.exe",
    "svchost.exe",
    "winlogon.exe",
    "dwm.exe",
    "fontdrvhost.exe",
    "system idle process",
];

/// Linux/macOS equivalents that are clearly kernel-side or session infrastructure.
const SYSTEM_UNIX: &[&str] = &[
    "kthreadd",
    "ksoftirqd",
    "kworker",
    "rcu_",
    "migration",
    "systemd",
    "systemd-journal",
    "systemd-udevd",
    "systemd-logind",
    "dbus-daemon",
    "dbus-broker",
    "launchd",
    "loginwindow",
    "windowserver",
    "kernel_task",
];

fn normalize(name: &str) -> String {
    name.trim_start_matches('/').to_ascii_lowercase()
}

fn is_system_name(name_lower: &str) -> bool {
    SYSTEM_CRITICAL.contains(&name_lower)
        || SYSTEM_UNIX
            .iter()
            .any(|n| name_lower.starts_with(n) || name_lower == *n)
}

/// Classification input gathered from a snapshot's raw fields.
#[derive(Debug, Clone)]
pub struct ClassifyInput<'a> {
    pub pid: u32,
    pub name: &'a str,
    /// Chain from direct parent up to root (ppid, ppid-of-ppid, ...).
    pub ancestor_names: &'a [&'a str],
    pub has_window: bool,
    /// Session 0 on Windows / uid 0 root daemon context.
    pub system_session: bool,
}

pub fn classify(input: ClassifyInput<'_>) -> ProcCategory {
    let name = normalize(input.name);

    if input.pid <= 4 || is_system_name(&name) {
        return ProcCategory::System;
    }

    // Anything in the services/session-infrastructure ancestry is a Windows process.
    let ancestor_is_system = input
        .ancestor_names
        .iter()
        .any(|a| is_system_name(&normalize(a)));
    if ancestor_is_system {
        return ProcCategory::System;
    }

    if input.has_window {
        return ProcCategory::App;
    }

    if input.system_session {
        return ProcCategory::System;
    }

    ProcCategory::Background
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windowed_process_is_app() {
        let anc = ["explorer.exe"];
        assert_eq!(
            classify(ClassifyInput {
                pid: 4000,
                name: "firefox.exe",
                ancestor_names: &anc,
                has_window: true,
                system_session: false,
            }),
            ProcCategory::App
        );
    }

    #[test]
    fn svchost_is_system_even_without_window() {
        assert_eq!(
            classify(ClassifyInput {
                pid: 1200,
                name: "svchost.exe",
                ancestor_names: &[],
                has_window: false,
                system_session: false,
            }),
            ProcCategory::System
        );
    }

    #[test]
    fn child_of_services_is_system() {
        let anc = ["services.exe"];
        assert_eq!(
            classify(ClassifyInput {
                pid: 3000,
                name: "spoolsv.exe",
                ancestor_names: &anc,
                has_window: false,
                system_session: false,
            }),
            ProcCategory::System
        );
    }

    #[test]
    fn plain_background() {
        assert_eq!(
            classify(ClassifyInput {
                pid: 9000,
                name: "updatehelper.exe",
                ancestor_names: &[],
                has_window: false,
                system_session: false,
            }),
            ProcCategory::Background
        );
    }

    #[test]
    fn low_pids_are_system() {
        assert_eq!(
            classify(ClassifyInput {
                pid: 4,
                name: "System",
                ancestor_names: &[],
                has_window: false,
                system_session: true,
            }),
            ProcCategory::System
        );
    }

    #[test]
    fn unix_kernel_threads_are_system() {
        assert_eq!(
            classify(ClassifyInput {
                pid: 55,
                name: "kworker/u16:2",
                ancestor_names: &[],
                has_window: false,
                system_session: true,
            }),
            ProcCategory::System
        );
        assert_eq!(
            classify(ClassifyInput {
                pid: 1,
                name: "systemd",
                ancestor_names: &[],
                has_window: false,
                system_session: false,
            }),
            ProcCategory::System
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(
            classify(ClassifyInput {
                pid: 700,
                name: "SVCHOST.EXE",
                ancestor_names: &[],
                has_window: false,
                system_session: false,
            }),
            ProcCategory::System
        );
    }
}
