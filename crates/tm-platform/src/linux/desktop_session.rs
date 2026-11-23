//! Conservative Linux desktop-role evidence for native Wayland sessions.
//!
//! Native Wayland has no portable whole-desktop window inventory. Instead,
//! this module reads one validated systemd hierarchy from
//! `/proc/<pid>/cgroup`: standard user `app.slice` units identify launcher-
//! managed applications, while `background.slice`, `session.slice`, direct
//! logind session scopes and other members of a user manager identify
//! background/session work. `system.slice` and `init.scope` identify system
//! processes.
//!
//! Some applications bypass systemd scopes. For a process already proven to
//! be inside a user-session hierarchy, a capped `/proc/<pid>/environ` read may
//! supply launch metadata such as Flatpak, Snap, GIO or BAMF identity. Values
//! inherited by every process in the desktop session (`XDG_CURRENT_DESKTOP`
//! and similar) are never treated as per-process app evidence.
//!
//! This identifies launcher-managed applications, not compositor focus or an
//! exact window inventory. Malformed, escaped, ambiguous, inaccessible,
//! oversized, unrelated or generation-mismatched evidence remains Unknown.
//! That limitation applies equally to GNOME/KDE applications launched outside
//! these conventions and to WSL environments without a relevant systemd user
//! hierarchy; this module does not call GNOME Shell.Introspect or interactive
//! KWin APIs to fill the gap.

use std::io::Read;
use std::path::Path;
use tm_core::classify::DesktopRoleEvidence;

const MAX_CGROUP_BYTES: usize = 16 * 1024;
const MAX_CGROUP_PATH_BYTES: usize = 8 * 1024;
const MAX_UNIT_BYTES: usize = 512;
const MAX_ENVIRON_BYTES: usize = 64 * 1024;
const MAX_HINT_VALUE_BYTES: usize = 512;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DesktopEvidence {
    pub(super) role: Option<DesktopRoleEvidence>,
    /// Stable application-family identity. It is present only with an App
    /// role and never merges distinct systemd units merely because both live
    /// under `app.slice`.
    pub(super) app_group: Option<String>,
    pub(super) session_id: Option<u32>,
    user_session: bool,
    /// Canonical user session/user-manager prefix used to keep environment-
    /// derived app families from colliding across different users/sessions.
    user_scope: Option<String>,
}

/// Read role evidence after sysinfo refreshed, then verify that the PID still
/// has the same boot-relative start ticks. Any disappearance or reuse discards
/// every role, session and grouping fact read for the old generation.
pub(super) fn inspect(pid: u32, expected_start_ticks: i64) -> DesktopEvidence {
    if super::linux_proc_start_ticks(pid) != Some(expected_start_ticks) {
        return DesktopEvidence::default();
    }
    let Some(raw) = read_bounded_file(&format!("/proc/{pid}/cgroup"), MAX_CGROUP_BYTES) else {
        return DesktopEvidence::default();
    };
    let Some(mut evidence) = parse_cgroup(&raw) else {
        return DesktopEvidence::default();
    };

    // App scopes already carry a stronger group identity. Environment reads
    // are only useful for user-session processes without one, and never for
    // unrelated/malformed cgroups (whose parsed evidence is empty).
    if evidence.user_session
        && evidence.app_group.is_none()
        && let Some(hints) = read_environment_hints(pid)
    {
        apply_environment_hints(&mut evidence, hints);
    }

    if super::linux_proc_start_ticks(pid) != Some(expected_start_ticks) {
        return DesktopEvidence::default();
    }
    evidence
}

fn read_bounded_file(path: &str, max_bytes: usize) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    file.take((max_bytes as u64).checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= max_bytes).then_some(bytes)
}

fn parse_cgroup(raw: &[u8]) -> Option<DesktopEvidence> {
    let text = std::str::from_utf8(raw).ok()?;
    let path = systemd_cgroup_path(text)?;
    parse_systemd_path(path)
}

/// cgroup v2 publishes one `0::/path` hierarchy. A v1/hybrid process exposes
/// the systemd named hierarchy separately. Multiple candidates are ambiguous:
/// never choose one and guess which membership is authoritative.
fn systemd_cgroup_path(raw: &str) -> Option<&str> {
    let mut selected = None;
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next()?;
        let controllers = fields.next()?;
        let path = fields.next()?;
        let _: u32 = hierarchy.parse().ok()?;
        if path.contains('\0') {
            return None;
        }

        let systemd_line = if hierarchy == "0" {
            if !controllers.is_empty() {
                return None;
            }
            true
        } else {
            controllers
                .split(',')
                .any(|controller| controller == "name=systemd")
        };
        if systemd_line {
            if selected.replace(path).is_some() {
                return None;
            }
        }
    }
    selected
}

fn parse_systemd_path(path: &str) -> Option<DesktopEvidence> {
    let components = canonical_components(path)?;
    if components.is_empty() {
        return Some(DesktopEvidence::default());
    }
    if components.first() == Some(&"system.slice") || components.first() == Some(&"init.scope") {
        return Some(DesktopEvidence {
            role: Some(DesktopRoleEvidence::System),
            ..DesktopEvidence::default()
        });
    }
    if components.first() != Some(&"user.slice") {
        return Some(DesktopEvidence::default());
    }

    let Some(uid) = components
        .get(1)
        .and_then(|component| parse_user_slice(*component))
    else {
        return Some(DesktopEvidence::default());
    };
    let Some(session_or_manager) = components.get(2) else {
        return Some(DesktopEvidence::default());
    };

    if let Some(session_id) = parse_session_scope(*session_or_manager) {
        return Some(DesktopEvidence {
            role: Some(DesktopRoleEvidence::Background),
            session_id: Some(session_id),
            user_session: true,
            user_scope: Some(components[..=2].join("/")),
            ..DesktopEvidence::default()
        });
    }

    let Some(manager_uid) = parse_user_manager(*session_or_manager) else {
        return None;
    };
    if manager_uid != uid {
        // Mismatched user identities cannot be attributed to one desktop
        // session. Do not even use the environment as a weaker fallback.
        return None;
    }

    let rest = &components[3..];
    let mut evidence = DesktopEvidence {
        role: Some(DesktopRoleEvidence::Background),
        user_session: true,
        user_scope: Some(components[..=2].join("/")),
        ..DesktopEvidence::default()
    };
    if rest.first() == Some(&"app.slice") {
        let Some(unit) = rest.get(1) else {
            // A process directly in app.slice is not an app scope/service.
            return None;
        };
        if !valid_application_unit(unit) {
            return None;
        }
        let unit_index = 4;
        evidence.role = Some(DesktopRoleEvidence::App);
        evidence.app_group = Some(format!("systemd:/{}", components[..=unit_index].join("/")));
    }
    Some(evidence)
}

fn canonical_components(path: &str) -> Option<Vec<&str>> {
    if path.len() > MAX_CGROUP_PATH_BYTES
        || !path.starts_with('/')
        || path.contains('\\')
        || !path.is_ascii()
    {
        return None;
    }
    if path == "/" {
        return Some(Vec::new());
    }
    let body = path.strip_prefix('/')?;
    if body.is_empty() || body.ends_with('/') {
        return None;
    }
    let components: Vec<&str> = body.split('/').collect();
    if components.iter().any(|component| {
        component.is_empty()
            || *component == "."
            || *component == ".."
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'\\' && byte != b'/')
    }) {
        return None;
    }
    Some(components)
}

fn parse_user_slice(component: &str) -> Option<u32> {
    let uid = component.strip_prefix("user-")?.strip_suffix(".slice")?;
    parse_decimal(uid)
}

fn parse_user_manager(component: &str) -> Option<u32> {
    let uid = component.strip_prefix("user@")?.strip_suffix(".service")?;
    parse_decimal(uid)
}

fn parse_session_scope(component: &str) -> Option<u32> {
    let id = component.strip_prefix("session-")?.strip_suffix(".scope")?;
    parse_decimal(id)
}

fn parse_decimal(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn valid_application_unit(component: &str) -> bool {
    if component.len() > MAX_UNIT_BYTES {
        return false;
    }
    let stem = component
        .strip_suffix(".scope")
        .or_else(|| component.strip_suffix(".service"));
    stem.is_some_and(|stem| {
        !stem.is_empty()
            && component.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@' | b':')
            })
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DesktopHints {
    flatpak_id: Option<String>,
    snap_instance: Option<String>,
    desktop_file: Option<String>,
}

fn read_environment_hints(pid: u32) -> Option<DesktopHints> {
    let raw = read_bounded_file(&format!("/proc/{pid}/environ"), MAX_ENVIRON_BYTES)?;
    parse_environment(&raw)
}

fn parse_environment(raw: &[u8]) -> Option<DesktopHints> {
    if raw.len() > MAX_ENVIRON_BYTES {
        return None;
    }
    if raw.is_empty() {
        return Some(DesktopHints::default());
    }
    // execve stores NUL-terminated entries. A short read without that final
    // terminator is partial evidence and must not drive classification.
    if raw.last() != Some(&0) {
        return None;
    }
    let body = &raw[..raw.len() - 1];
    if body.is_empty() {
        return None;
    }

    let mut hints = DesktopHints::default();
    for entry in body.split(|byte| *byte == 0) {
        if entry.is_empty() {
            return None;
        }
        let equals = entry.iter().position(|byte| *byte == b'=')?;
        let (key, value_with_equals) = entry.split_at(equals);
        if key.is_empty() {
            return None;
        }
        let value = value_with_equals.get(1..)?;
        match key {
            b"FLATPAK_ID" => assign_identity(&mut hints.flatpak_id, value)?,
            b"SNAP_INSTANCE_NAME" => assign_identity(&mut hints.snap_instance, value)?,
            b"GIO_LAUNCHED_DESKTOP_FILE" | b"BAMF_DESKTOP_FILE_HINT" => {
                assign_desktop_file(&mut hints.desktop_file, value)?
            }
            _ => {}
        }
    }
    Some(hints)
}

fn assign_identity(slot: &mut Option<String>, raw: &[u8]) -> Option<()> {
    let value = std::str::from_utf8(raw).ok()?;
    if value.is_empty()
        || value.len() > MAX_HINT_VALUE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return None;
    }
    assign_unique(slot, value.to_owned())
}

fn assign_desktop_file(slot: &mut Option<String>, raw: &[u8]) -> Option<()> {
    let value = std::str::from_utf8(raw).ok()?.trim();
    if value.is_empty() || value.len() > MAX_HINT_VALUE_BYTES || value.chars().any(char::is_control)
    {
        return None;
    }
    let file_name = Path::new(value).file_name()?.to_str()?;
    if file_name.is_empty() || !file_name.ends_with(".desktop") {
        return None;
    }
    assign_unique(slot, file_name.to_owned())
}

fn assign_unique(slot: &mut Option<String>, value: String) -> Option<()> {
    match slot {
        Some(existing) if existing.as_str() != value.as_str() => None,
        Some(_) => Some(()),
        None => {
            *slot = Some(value);
            Some(())
        }
    }
}

impl DesktopHints {
    fn app_group(&self) -> Option<String> {
        if let Some(id) = &self.flatpak_id {
            Some(format!("flatpak:{id}"))
        } else if let Some(instance) = &self.snap_instance {
            Some(format!("snap:{instance}"))
        } else {
            self.desktop_file
                .as_ref()
                .map(|desktop_file| format!("desktop:{desktop_file}"))
        }
    }
}

fn apply_environment_hints(evidence: &mut DesktopEvidence, hints: DesktopHints) {
    if !evidence.user_session || evidence.role == Some(DesktopRoleEvidence::App) {
        return;
    }
    if let Some(app_group) = hints.app_group() {
        let Some(user_scope) = evidence.user_scope.as_deref() else {
            return;
        };
        evidence.role = Some(DesktopRoleEvidence::App);
        evidence.app_group = Some(format!("systemd:/{user_scope}:{app_group}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_path(rest: &str) -> String {
        format!("/user.slice/user-1000.slice/user@1000.service/{rest}")
    }

    fn parse_v2(path: &str) -> Option<DesktopEvidence> {
        parse_cgroup(format!("0::{path}\n").as_bytes())
    }

    #[test]
    fn gnome_kde_flatpak_and_snap_app_units_are_apps_with_distinct_families() {
        let units = [
            "app.slice/app-gnome-org.gnome.Terminal-42.scope",
            "app.slice/app-org.gnome.Nautilus-11.scope",
            "app.slice/app-org.kde.konsole-17.scope",
            "app.slice/app-flatpak-org.gnome.Builder-123.scope",
            "app.slice/snap.firefox.firefox.service",
        ];
        let mut groups = std::collections::HashSet::new();
        for unit in units {
            let path = user_path(unit);
            let evidence = parse_v2(&path).expect("valid user app cgroup");
            assert_eq!(
                evidence.role,
                Some(DesktopRoleEvidence::App),
                "{unit} must be an app"
            );
            assert!(evidence.user_session);
            let group = evidence.app_group.expect("stable app family");
            assert!(group.ends_with(unit), "{group} must retain {unit}");
            assert!(
                groups.insert(group),
                "unrelated units must not share a family"
            );
        }
    }

    #[test]
    fn descendants_of_one_app_unit_share_its_family_identity() {
        let scope = user_path("app.slice/app-org.kde.konsole-17.scope");
        let direct = parse_v2(&scope).expect("scope");
        let nested = parse_v2(&format!("{scope}/child")).expect("nested child");
        assert_eq!(direct.role, Some(DesktopRoleEvidence::App));
        assert_eq!(direct.app_group, nested.app_group);
    }

    #[test]
    fn background_session_and_direct_session_paths_are_background() {
        for path in [
            user_path("background.slice/org.example.worker.service"),
            user_path("session.slice/org.gnome.Shell@wayland.service"),
            user_path("init.scope"),
            "/user.slice/user-1000.slice/session-4.scope".to_owned(),
            "/user.slice/user-1000.slice/session-4.scope/org.example.worker".to_owned(),
        ] {
            let evidence = parse_v2(&path).expect("valid user-session cgroup");
            assert_eq!(
                evidence.role,
                Some(DesktopRoleEvidence::Background),
                "{path}"
            );
        }
        let evidence =
            parse_v2("/user.slice/user-1000.slice/session-4.scope").expect("session scope");
        assert_eq!(evidence.session_id, Some(4));
    }

    #[test]
    fn system_cgroups_are_system_but_only_inside_the_system_hierarchy() {
        for path in [
            "/system.slice/org.example.service",
            "/system.slice/org.example.service/worker",
            "/init.scope",
            "/init.scope/worker",
        ] {
            let evidence = parse_v2(path).expect("valid system cgroup");
            assert_eq!(evidence.role, Some(DesktopRoleEvidence::System), "{path}");
        }
        let unrelated = parse_v2("/machine.slice/app.slice/app-org.example-1.scope")
            .expect("well-formed unrelated cgroup");
        assert_eq!(unrelated.role, None);
        assert!(!unrelated.user_session);
    }

    #[test]
    fn malformed_escaped_ambiguous_and_unrelated_paths_do_not_guess() {
        for path in [
            "user.slice/user-1000.slice/user@1000.service/app.slice/app-org.example-1.scope",
            "/user.slice/../system.slice/org.example.service",
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.example\\x2d1.scope",
            "/user.slice/user-1000.slice/user@1001.service/app.slice/app-org.example-1.scope",
            "/user.slice/user-1000.slice/app.slice/app-org.example-1.scope",
            "/user.slice/user-1000.slice/user@1000.service/app.slice/not-a-unit",
        ] {
            assert_eq!(parse_v2(path), None, "{path}");
        }
        let unrelated = parse_v2("/foo.slice/thing.service").expect("well-formed path");
        assert_eq!(unrelated, DesktopEvidence::default());
    }

    #[test]
    fn multiple_systemd_hierarchies_are_ambiguous() {
        let raw = "\
0::/user.slice/user-1000.slice/session-4.scope
1:name=systemd:/system.slice/org.example.service
";
        assert_eq!(parse_cgroup(raw.as_bytes()), None);
    }

    #[test]
    fn cgroup_v1_named_systemd_hierarchy_is_supported() {
        let evidence = parse_cgroup(
            b"2:cpu,cpuacct:/\n1:name=systemd:/user.slice/user-1000.slice/session-7.scope\n",
        )
        .expect("v1 systemd path");
        assert_eq!(evidence.role, Some(DesktopRoleEvidence::Background));
        assert_eq!(evidence.session_id, Some(7));
    }

    #[test]
    fn desktop_launch_hints_upgrade_only_a_user_session_and_ignore_desktop_wide_keys() {
        let raw = b"PATH=/usr/bin\0XDG_CURRENT_DESKTOP=KDE\0GIO_LAUNCHED_DESKTOP_FILE=/usr/share/applications/org.gnome.Terminal.desktop\0";
        let hints = parse_environment(raw).expect("complete environment");
        assert_eq!(
            hints.app_group().as_deref(),
            Some("desktop:org.gnome.Terminal.desktop")
        );

        let mut user = DesktopEvidence {
            role: Some(DesktopRoleEvidence::Background),
            user_session: true,
            user_scope: Some("/user.slice/user-1000.slice/session-4.scope".into()),
            ..DesktopEvidence::default()
        };
        apply_environment_hints(&mut user, hints.clone());
        assert_eq!(user.role, Some(DesktopRoleEvidence::App));
        assert_eq!(
            user.app_group.as_deref(),
            Some(
                "systemd:/user.slice/user-1000.slice/session-4.scope:desktop:org.gnome.Terminal.desktop"
            )
        );
        let mut other_user = DesktopEvidence {
            role: Some(DesktopRoleEvidence::Background),
            user_session: true,
            user_scope: Some("/user.slice/user-1001.slice/session-5.scope".into()),
            ..DesktopEvidence::default()
        };
        apply_environment_hints(&mut other_user, hints.clone());
        assert_ne!(
            user.app_group, other_user.app_group,
            "launcher hints must not merge across user sessions"
        );

        let mut background = parse_v2(&user_path("background.slice/org.example.launcher.service"))
            .expect("background user unit");
        apply_environment_hints(&mut background, hints.clone());
        assert_eq!(
            background.role,
            Some(DesktopRoleEvidence::App),
            "an App hint outranks a positive Background role"
        );

        let mut unrelated = DesktopEvidence::default();
        apply_environment_hints(&mut unrelated, hints);
        assert_eq!(unrelated, DesktopEvidence::default());

        let desktop_wide_only =
            parse_environment(b"XDG_CURRENT_DESKTOP=GNOME\0").expect("complete environment");
        let mut user = DesktopEvidence {
            role: Some(DesktopRoleEvidence::Background),
            user_session: true,
            user_scope: Some("/user.slice/user-1000.slice/session-4.scope".into()),
            ..DesktopEvidence::default()
        };
        apply_environment_hints(&mut user, desktop_wide_only);
        assert_eq!(user.role, Some(DesktopRoleEvidence::Background));
        assert_eq!(user.app_group, None);
    }

    #[test]
    fn flatpak_and_snap_hints_supply_fallback_identity() {
        let flatpak = parse_environment(b"FLATPAK_ID=org.gnome.Builder\0").expect("environment");
        assert_eq!(
            flatpak.app_group().as_deref(),
            Some("flatpak:org.gnome.Builder")
        );
        let snap = parse_environment(b"SNAP_INSTANCE_NAME=firefox_foo\0").expect("environment");
        assert_eq!(snap.app_group().as_deref(), Some("snap:firefox_foo"));
    }

    #[test]
    fn partial_oversized_and_conflicting_environments_are_rejected() {
        let partial = b"FLATPAK_ID=org.example.App";
        assert_eq!(parse_environment(partial), None);
        let evidence = DesktopEvidence {
            user_session: true,
            user_scope: Some("/user.slice/user-1000.slice/session-4.scope".into()),
            ..DesktopEvidence::default()
        };
        assert_eq!(
            tm_core::classify::classify(tm_core::classify::ClassifyInput {
                pid: 3000,
                name: "native-wayland-app",
                has_window: None,
                critical: None,
                windows_owned: None,
                desktop_role: evidence.role,
            }),
            tm_core::model::ProcCategory::Unknown
        );
        let oversized = vec![b'A'; MAX_ENVIRON_BYTES + 1];
        assert_eq!(parse_environment(&oversized), None);
        assert_eq!(
            parse_environment(
                b"GIO_LAUNCHED_DESKTOP_FILE=a.desktop\0GIO_LAUNCHED_DESKTOP_FILE=b.desktop\0"
            ),
            None
        );
        assert_eq!(
            parse_environment(b"FLATPAK_ID=bad/value\0"),
            None,
            "unsafe hint values must not become app identity"
        );
    }

    #[test]
    fn malformed_cgroup_and_unavailable_environment_remain_unknown() {
        assert_eq!(parse_cgroup(b"0::not-absolute\n"), None);
        assert_eq!(read_environment_hints(u32::MAX), None);
        let evidence = DesktopEvidence {
            role: parse_cgroup(b"0::not-absolute\n").and_then(|evidence| evidence.role),
            ..DesktopEvidence::default()
        };
        assert_eq!(
            tm_core::classify::classify(tm_core::classify::ClassifyInput {
                pid: 3000,
                name: "native-wayland-app",
                has_window: None,
                critical: None,
                windows_owned: None,
                desktop_role: evidence.role,
            }),
            tm_core::model::ProcCategory::Unknown
        );
    }

    #[test]
    fn unreadable_environ_does_not_override_positive_cgroup_evidence_or_guess() {
        assert_eq!(read_environment_hints(u32::MAX), None);
        let app = parse_v2(&user_path("app.slice/app-org.example-1.scope")).expect("app cgroup");
        assert_eq!(app.role, Some(DesktopRoleEvidence::App));
        assert!(app.app_group.is_some());
    }

    #[test]
    fn evidence_from_an_old_process_generation_is_discarded() {
        let pid = std::process::id();
        let ticks = super::super::linux_proc_start_ticks(pid).expect("own start ticks");
        assert_eq!(inspect(pid, ticks + 1), DesktopEvidence::default());
    }
}
