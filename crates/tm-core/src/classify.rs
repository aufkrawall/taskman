//! Process classification into Windows-Task-Manager-like groups: Apps /
//! Background / System, plus Unknown when the platform cannot supply positive
//! ownership or desktop-role evidence. Pure logic, unit-tested.
//!
//! The native rule is deliberately conservative (Raymond Chen, "How does
//! Task Manager categorize processes as App, Background Process, or Windows
//! Process?"): a visible window makes an App, a *critical* process or one of
//! a short list of core OS images is a Windows process, and everything else
//! is Background. Microsoft-signed background machinery (WMI Provider Host,
//! SmartScreen, shell brokers, windowless PowerShell) is therefore a
//! Background process on a stock system, not a Windows process.

use crate::model::ProcCategory;

/// Kernel pseudo-process names that are not image files at all.
const KERNEL_NAMES: &[&str] = &[
    "system",
    "registry",
    "memory compression",
    "secure system",
    "system idle process",
    "[system process]",
];

/// Core Windows session/OS images. This is the conservative native set:
/// it is combined with `IsProcessCritical`, but NOT with "any Microsoft
/// binary under %SystemRoot%". Shell brokers (`RuntimeBroker`, `dllhost`,
/// `taskhostw`, `sihost`, ...), WMI hosts, SmartScreen and user-launchable
/// tools (`powershell.exe`, `cmd.exe`, `notepad.exe`) are Background unless
/// they own a visible window.
const WINDOWS_CORE_IMAGES: &[&str] = &[
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "services.exe",
    "lsass.exe",
    "lsaiso.exe",
    "svchost.exe",
    "winlogon.exe",
    "dwm.exe",
    "conhost.exe",
];

/// Linux/macOS equivalents that are clearly kernel-side or session
/// infrastructure.
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

fn is_kernel_name(name_lower: &str) -> bool {
    KERNEL_NAMES.contains(&name_lower)
}

fn matches_core(name_lower: &str) -> bool {
    if is_kernel_name(name_lower) || WINDOWS_CORE_IMAGES.contains(&name_lower) {
        return true;
    }
    // Linux kernel threads are bracketed ("[kworker/0:1]").
    if name_lower.starts_with('[') {
        return true;
    }
    SYSTEM_UNIX
        .iter()
        .any(|n| name_lower.starts_with(n) || name_lower == *n)
}

/// True for a kernel pseudo-process or a core OS image, by name alone.
///
/// The presentation layer uses the same set as process-tree boundaries, so
/// the platform and the UI cannot drift apart on what "a system process" is.
/// Name matches are never evidence by themselves for actions: the classifier
/// additionally requires Windows ownership (see [`ClassifyInput`]).
pub fn is_core_os_image(name: &str) -> bool {
    matches_core(&normalize(name))
}

/// Positive desktop-session role evidence supplied by the platform.
///
/// This is deliberately only a role, not a window inventory. Linux derives it
/// from validated systemd cgroup membership and conservative per-process
/// desktop-launch metadata; native Wayland has no portable whole-window list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopRoleEvidence {
    App,
    Background,
    System,
}

/// Classification input gathered from a snapshot's raw fields.
#[derive(Debug, Clone)]
pub struct ClassifyInput<'a> {
    pub pid: u32,
    pub name: &'a str,
    /// Window ownership when the platform could observe it. `None` means the
    /// platform cannot answer for this process; it must not become a negative
    /// "no window" fact.
    pub has_window: Option<bool>,
    /// Windows `IsProcessCritical`. `None` when it could not be queried;
    /// unknown must never be read as "critical".
    pub critical: Option<bool>,
    /// Positive signal that the executable is Microsoft-published from a
    /// Windows-owned path. Used ONLY to reject a spoofed core image name
    /// (a third-party `svchost.exe` in a user directory must not become a
    /// Windows process). `None` = metadata unavailable, so a known core name
    /// is trusted; `Some(false)` rejects the name match.
    pub windows_owned: Option<bool>,
    /// Positive desktop-session role evidence. `None` means the platform has
    /// no authoritative role for this process, never that it is windowless.
    pub desktop_role: Option<DesktopRoleEvidence>,
}

/// Precedence: hard System evidence, an observed window, complete no-window
/// evidence, positive desktop App evidence, positive desktop Background
/// evidence, then Unknown. A missing window observation is never a negative
/// window fact.
pub fn classify(input: ClassifyInput<'_>) -> ProcCategory {
    let name = normalize(input.name);

    if input.pid <= 4 || is_kernel_name(&name) {
        return ProcCategory::System;
    }

    if input.critical == Some(true) {
        return ProcCategory::System;
    }

    if matches_core(&name) && input.windows_owned.unwrap_or(true) {
        return ProcCategory::System;
    }

    // system.slice/init.scope membership is positive system evidence. It
    // must win over a window: a system service may own a window without
    // becoming an application.
    if input.desktop_role == Some(DesktopRoleEvidence::System) {
        return ProcCategory::System;
    }

    match input.has_window {
        Some(true) => ProcCategory::App,
        Some(false) => ProcCategory::Background,
        None => match input.desktop_role {
            Some(DesktopRoleEvidence::App) => ProcCategory::App,
            Some(DesktopRoleEvidence::Background) => ProcCategory::Background,
            Some(DesktopRoleEvidence::System) | None => ProcCategory::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_win(
        pid: u32,
        name: &str,
        has_window: bool,
        critical: Option<bool>,
        windows_owned: Option<bool>,
    ) -> ProcCategory {
        classify(ClassifyInput {
            pid,
            name,
            has_window: Some(has_window),
            critical,
            windows_owned,
            desktop_role: None,
        })
    }

    #[test]
    fn windowed_process_is_app() {
        assert_eq!(
            classify_win(4000, "firefox.exe", true, None, Some(false)),
            ProcCategory::App
        );
    }

    #[test]
    fn windowed_windows_tool_is_still_an_app() {
        // Native: a visible window wins. System32 tools the user launched
        // (regedit, mmc, notepad) are Apps, not Windows processes.
        assert_eq!(
            classify_win(4000, "regedit.exe", true, None, Some(true)),
            ProcCategory::App
        );
    }

    #[test]
    fn svchost_is_system_even_without_window() {
        assert_eq!(
            classify_win(1200, "svchost.exe", false, None, Some(true)),
            ProcCategory::System
        );
    }

    #[test]
    fn core_os_image_without_metadata_is_still_system() {
        // Protected processes often refuse metadata queries; the name is
        // enough when ownership is genuinely unknown.
        assert_eq!(
            classify_win(700, "csrss.exe", false, None, None),
            ProcCategory::System
        );
    }

    #[test]
    fn spoofed_core_image_name_is_not_system() {
        // A third-party binary named svchost.exe in a user directory has
        // readable metadata that says it is NOT Windows-owned.
        assert_eq!(
            classify_win(9000, "svchost.exe", false, None, Some(false)),
            ProcCategory::Background
        );
    }

    #[test]
    fn critical_process_is_system_even_with_unknown_name() {
        assert_eq!(
            classify_win(3000, "vendor-protector.exe", false, Some(true), Some(false)),
            ProcCategory::System
        );
    }

    #[test]
    fn unknown_critical_state_is_not_critical() {
        assert_eq!(
            classify_win(3000, "vendor-protector.exe", false, None, Some(false)),
            ProcCategory::Background
        );
    }

    #[test]
    fn unknown_window_ownership_is_not_a_negative_window_fact() {
        assert_eq!(
            classify(ClassifyInput {
                pid: 3000,
                name: "app",
                has_window: None,
                critical: None,
                windows_owned: None,
                desktop_role: None,
            }),
            ProcCategory::Unknown
        );
    }

    #[test]
    fn desktop_roles_classify_only_when_window_evidence_is_absent() {
        let category = |has_window, desktop_role| {
            classify(ClassifyInput {
                pid: 3000,
                name: "desktop-app",
                has_window,
                critical: None,
                windows_owned: None,
                desktop_role,
            })
        };
        assert_eq!(
            category(None, Some(DesktopRoleEvidence::App)),
            ProcCategory::App
        );
        assert_eq!(
            category(None, Some(DesktopRoleEvidence::Background)),
            ProcCategory::Background
        );
        assert_eq!(
            category(Some(true), Some(DesktopRoleEvidence::Background)),
            ProcCategory::App
        );
        assert_eq!(
            category(Some(false), Some(DesktopRoleEvidence::App)),
            ProcCategory::Background
        );
    }

    #[test]
    fn system_role_wins_over_window_evidence() {
        let category = |has_window, desktop_role| {
            classify(ClassifyInput {
                pid: 3000,
                name: "desktop-infrastructure",
                has_window,
                critical: None,
                windows_owned: None,
                desktop_role,
            })
        };
        assert_eq!(
            category(Some(true), Some(DesktopRoleEvidence::System)),
            ProcCategory::System
        );
        assert_eq!(
            category(Some(false), Some(DesktopRoleEvidence::System)),
            ProcCategory::System
        );
        assert_eq!(
            classify(ClassifyInput {
                pid: 3000,
                name: "svchost.exe",
                has_window: Some(true),
                critical: None,
                windows_owned: Some(true),
                desktop_role: Some(DesktopRoleEvidence::App),
            }),
            ProcCategory::System
        );
    }

    #[test]
    fn classification_precedence_is_system_window_negative_app_background_unknown() {
        let category = |has_window, desktop_role| {
            classify(ClassifyInput {
                pid: 3000,
                name: "desktop-process",
                has_window,
                critical: None,
                windows_owned: None,
                desktop_role,
            })
        };
        assert_eq!(
            category(None, Some(DesktopRoleEvidence::System)),
            ProcCategory::System
        );
        assert_eq!(
            category(Some(true), Some(DesktopRoleEvidence::App)),
            ProcCategory::App
        );
        assert_eq!(
            category(Some(false), Some(DesktopRoleEvidence::App)),
            ProcCategory::Background
        );
        assert_eq!(
            category(None, Some(DesktopRoleEvidence::App)),
            ProcCategory::App
        );
        assert_eq!(
            category(None, Some(DesktopRoleEvidence::Background)),
            ProcCategory::Background
        );
        assert_eq!(category(None, None), ProcCategory::Unknown);
    }

    #[test]
    fn microsoft_background_machinery_is_background() {
        // Observed in native Windows 11 Task Manager's Background processes
        // list; the old "Microsoft + %SystemRoot%" rule mislabelled all of
        // these as Windows processes.
        for name in [
            "WmiPrvSE.exe",
            "smartscreen.exe",
            "fontdrvhost.exe",
            "TextInputHost.exe",
            "WUDFHost.exe",
            "RuntimeBroker.exe",
            "dllhost.exe",
            "taskhostw.exe",
            "sihost.exe",
            "powershell.exe",
            "cmd.exe",
        ] {
            assert_eq!(
                classify_win(3000, name, false, None, Some(true)),
                ProcCategory::Background,
                "{name} must stay Background"
            );
        }
    }

    #[test]
    fn third_party_service_is_background() {
        assert_eq!(
            classify_win(3000, "vendor-service.exe", false, None, Some(false)),
            ProcCategory::Background
        );
    }

    #[test]
    fn low_pids_are_system() {
        assert_eq!(
            classify_win(4, "System", false, Some(true), None),
            ProcCategory::System
        );
        assert_eq!(
            classify_win(0, "System Idle Process", false, None, None),
            ProcCategory::System
        );
    }

    #[test]
    fn unix_kernel_threads_are_system() {
        assert_eq!(
            classify_win(55, "kworker/u16:2", false, None, None),
            ProcCategory::System
        );
        assert_eq!(
            classify_win(1, "systemd", false, None, None),
            ProcCategory::System
        );
        assert_eq!(
            classify_win(66, "[kworker/0:1]", false, None, None),
            ProcCategory::System
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(
            classify_win(700, "SVCHOST.EXE", false, None, Some(true)),
            ProcCategory::System
        );
        assert_eq!(
            classify_win(700, "Svchost.Exe", false, None, Some(false)),
            ProcCategory::Background
        );
    }

    #[test]
    fn core_image_set_matches_the_classifier() {
        assert!(is_core_os_image("SERVICES.EXE"));
        assert!(is_core_os_image("conhost.exe"));
        assert!(is_core_os_image("Memory Compression"));
        assert!(!is_core_os_image("fontdrvhost.exe"));
        assert!(!is_core_os_image("WmiPrvSE.exe"));
        assert!(!is_core_os_image("explorer.exe"));
    }
}
