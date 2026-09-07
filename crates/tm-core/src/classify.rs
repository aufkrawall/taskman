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
    pub has_window: bool,
    /// Positive platform signal that this executable is OS-owned system
    /// infrastructure. Privilege, Session 0, or a system-process ancestor
    /// alone are deliberately not enough.
    pub system_process: bool,
}

pub fn classify(input: ClassifyInput<'_>) -> ProcCategory {
    let name = normalize(input.name);

    if input.pid <= 4 || is_system_name(&name) {
        return ProcCategory::System;
    }

    if input.system_process {
        return ProcCategory::System;
    }

    if input.has_window {
        return ProcCategory::App;
    }

    ProcCategory::Background
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windowed_process_is_app() {
        assert_eq!(
            classify(ClassifyInput {
                pid: 4000,
                name: "firefox.exe",
                has_window: true,
                system_process: false,
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
                has_window: false,
                system_process: false,
            }),
            ProcCategory::System
        );
    }

    #[test]
    fn ancestry_does_not_turn_a_third_party_service_into_windows() {
        assert_eq!(
            classify(ClassifyInput {
                pid: 3000,
                name: "vendor-service.exe",
                has_window: false,
                system_process: false,
            }),
            ProcCategory::Background
        );
    }

    #[test]
    fn positive_os_component_signal_is_system() {
        assert_eq!(
            classify(ClassifyInput {
                pid: 3000,
                name: "WmiPrvSE.exe",
                has_window: false,
                system_process: true,
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
                has_window: false,
                system_process: false,
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
                has_window: false,
                system_process: true,
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
                has_window: false,
                system_process: true,
            }),
            ProcCategory::System
        );
        assert_eq!(
            classify(ClassifyInput {
                pid: 1,
                name: "systemd",
                has_window: false,
                system_process: false,
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
                has_window: false,
                system_process: false,
            }),
            ProcCategory::System
        );
    }
}
