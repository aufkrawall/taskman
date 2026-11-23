//! Conservative Linux desktop-role evidence for native Wayland sessions.
//!
//! Native Wayland has no portable whole-desktop window inventory. Instead,
//! this module reads one validated systemd hierarchy from
//! `/proc/<pid>/cgroup`: a standard `app-*` unit or a Snap-launcher unit in
//! user `app.slice` can identify a launcher-managed application only when its
//! complete environment also carries a graphical-session display signal.
//! Arbitrary well-formed units under `app.slice` stay weak context and require
//! a validated Flatpak/Snap/GIO/BAMF launcher identity. `background.slice`,
//! `session.slice`, direct logind session scopes and user-manager
//! `init.scope` identify explicit background/session work; generic
//! `user@UID.service` membership is only weak context. Top-level
//! `system.slice`/`init.scope` identify System only when systemd is really
//! PID 1, so synthetic WSL cgroups stay Unknown.
//!
//! Some applications bypass systemd scopes. For a process already proven to
//! be inside a user-session hierarchy, a capped `/proc/<pid>/environ` read may
//! supply launch metadata such as Flatpak, Snap, GIO or BAMF identity. A
//! non-whitespace `WAYLAND_DISPLAY`, `WAYLAND_SOCKET`, or `DISPLAY` is a mandatory
//! graphical-session gate but never an app identity. Values inherited by
//! every process in the desktop session (`XDG_CURRENT_DESKTOP` and similar)
//! are never treated as per-process app evidence.
//!
//! This identifies launcher-managed applications, not compositor focus or an
//! exact window inventory. Malformed, escaped, ambiguous, inaccessible,
//! oversized, unrelated or generation-mismatched evidence remains Unknown.
//! That limitation applies equally to GNOME/KDE applications launched outside
//! these conventions and to WSL environments without a relevant systemd user
//! hierarchy; this module does not call GNOME Shell.Introspect or interactive
//! KWin APIs to fill the gap.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tm_core::classify::DesktopRoleEvidence;

const MAX_CGROUP_BYTES: usize = 16 * 1024;
const MAX_CGROUP_PATH_BYTES: usize = 8 * 1024;
const MAX_UNIT_BYTES: usize = 512;
const MAX_ENVIRON_BYTES: usize = 64 * 1024;
const MAX_HINT_VALUE_BYTES: usize = 512;
const MAX_COMM_BYTES: usize = 64;
pub(super) const DESKTOP_EVIDENCE_TTL: Duration = Duration::from_secs(5);
const DESKTOP_EVIDENCE_JITTER_MS: u64 = 1_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DesktopEvidence {
    pub(super) role: Option<DesktopRoleEvidence>,
    /// Stable application-family identity. It is present only with an App
    /// role and never merges distinct systemd units merely because both live
    /// under `app.slice`.
    pub(super) app_group: Option<String>,
    pub(super) session_id: Option<u32>,
    /// Validated user `app.slice` family awaiting the graphical-environment
    /// gate. It becomes `app_group` only when that gate is positive.
    app_unit_group: Option<String>,
    user_session: bool,
    /// Canonical user session/user-manager prefix, always with exactly one
    /// leading slash. It keeps app families from colliding across users and
    /// provides one canonical prefix for every app-family key.
    user_scope: Option<String>,
    /// False for malformed standard units or authoritative closed roles whose
    /// environment must not be allowed to turn an Unknown into an App.
    environment_eligible: bool,
}

impl DesktopEvidence {
    /// Linux always records the collector outcome, including Unknown, so the
    /// presentation layer can distinguish authoritative roles from Windows or
    /// legacy fallback snapshots.
    pub(super) fn role_marker(&self) -> DesktopRoleEvidence {
        self.role.unwrap_or(DesktopRoleEvidence::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedEvidence {
    observed_at: Instant,
    evidence: DesktopEvidence,
}

/// Collector-owned per-tick cache. The key includes the process generation,
/// and every hit revalidates that generation against `/proc/<pid>/stat`.
/// `execve` preserves that token and may replace the environment, while cgroup
/// movement preserves both, so those two facts can trail until the jittered
/// TTL expires. A short TTL avoids rereading both files on every tick.
#[derive(Debug, Default)]
pub(super) struct DesktopEvidenceCache {
    entries: HashMap<(u32, i64), CachedEvidence>,
    system_manager: Option<bool>,
}

impl DesktopEvidenceCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn evidence(&mut self, pid: u32, start_ticks: i64, now: Instant) -> DesktopEvidence {
        self.evidence_with(
            pid,
            start_ticks,
            now,
            super::linux_proc_start_ticks,
            systemd_system_manager_available,
            inspect,
        )
    }

    fn evidence_with<G, M, I>(
        &mut self,
        pid: u32,
        start_ticks: i64,
        now: Instant,
        current_start_ticks: G,
        system_manager_available: M,
        inspect_evidence: I,
    ) -> DesktopEvidence
    where
        G: FnOnce(u32) -> Option<i64>,
        M: FnOnce() -> bool,
        I: FnOnce(u32, i64, bool) -> DesktopEvidence,
    {
        if let Some(evidence) = self.cached(pid, start_ticks, now, current_start_ticks) {
            return evidence;
        }

        let system_manager = *self
            .system_manager
            .get_or_insert_with(system_manager_available);
        let evidence = inspect_evidence(pid, start_ticks, system_manager);
        self.remember(pid, start_ticks, now, evidence.clone());
        evidence
    }

    fn cached<G>(
        &mut self,
        pid: u32,
        start_ticks: i64,
        now: Instant,
        current_start_ticks: G,
    ) -> Option<DesktopEvidence>
    where
        G: FnOnce(u32) -> Option<i64>,
    {
        let key = (pid, start_ticks);
        let fresh = self.entries.get(&key).is_some_and(|entry| {
            now.saturating_duration_since(entry.observed_at) < evidence_ttl(pid, start_ticks)
        });
        if !fresh {
            self.entries.remove(&key);
            return None;
        }
        if current_start_ticks(pid) != Some(start_ticks) {
            self.entries.remove(&key);
            return None;
        }
        self.entries.get(&key).map(|entry| entry.evidence.clone())
    }

    fn remember(&mut self, pid: u32, start_ticks: i64, now: Instant, evidence: DesktopEvidence) {
        self.entries
            .retain(|(existing_pid, _), _| *existing_pid != pid);
        self.entries.insert(
            (pid, start_ticks),
            CachedEvidence {
                observed_at: now,
                evidence,
            },
        );
    }

    /// Keep the cache bounded by the processes observed in this tick.
    pub(super) fn retain_live(&mut self, live: &HashSet<u32>) {
        self.entries.retain(|(pid, _), _| live.contains(pid));
    }
}

/// Stagger refreshes across a one-second window around the five-second
/// nominal TTL. Otherwise every process cached during one sampling tick would
/// reread cgroup/environment in lockstep every five seconds.
fn evidence_ttl(pid: u32, start_ticks: i64) -> Duration {
    let mixed =
        u64::from(pid).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (start_ticks as u64).rotate_left(23);
    let jitter_ms = mixed % (DESKTOP_EVIDENCE_JITTER_MS + 1);
    DESKTOP_EVIDENCE_TTL - Duration::from_millis(DESKTOP_EVIDENCE_JITTER_MS / 2)
        + Duration::from_millis(jitter_ms)
}

/// Read role evidence after the collector validated the expected generation.
/// One final post-read identity check discards every role, session and
/// grouping fact if the PID was reused while cgroup/environ were read.
fn inspect(pid: u32, expected_start_ticks: i64, system_manager: bool) -> DesktopEvidence {
    let Some(raw) = read_bounded_file(&format!("/proc/{pid}/cgroup"), MAX_CGROUP_BYTES) else {
        return DesktopEvidence::default();
    };
    let Some(mut evidence) = parse_cgroup(&raw, system_manager) else {
        return DesktopEvidence::default();
    };

    // Environment reads are only useful for a weak user-session role, a
    // validated app.slice unit awaiting its graphical gate, or a fallback
    // launcher identity. Authoritative Background/System roles never read it.
    if evidence.user_session
        && evidence.environment_eligible
        && evidence.role.is_none()
        && let Some(hints) = read_environment_hints(pid)
    {
        apply_environment_hints(&mut evidence, hints);
    }

    if super::linux_proc_start_ticks(pid) != Some(expected_start_ticks) {
        return DesktopEvidence::default();
    }
    evidence
}

fn systemd_system_manager_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Path::new("/run/systemd/system").is_dir()
            && read_bounded_file("/proc/1/comm", MAX_COMM_BYTES)
                .as_deref()
                .is_some_and(pid1_comm_is_systemd)
    })
}

fn pid1_comm_is_systemd(raw: &[u8]) -> bool {
    std::str::from_utf8(raw)
        .ok()
        .is_some_and(|comm| comm.trim_end_matches(|c| matches!(c, '\r' | '\n')) == "systemd")
}

fn read_bounded_file(path: &str, max_bytes: usize) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    file.take((max_bytes as u64).checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= max_bytes).then_some(bytes)
}

fn parse_cgroup(raw: &[u8], system_manager: bool) -> Option<DesktopEvidence> {
    let text = std::str::from_utf8(raw).ok()?;
    let path = systemd_cgroup_path(text)?;
    parse_systemd_path(path, system_manager)
}

/// cgroup v2 publishes one `0::/path` hierarchy. A v1/hybrid process may also
/// expose the compatibility `name=systemd` hierarchy. Prefer a useful unified
/// v2 path, but a root `0::/` carries no evidence and must not hide a useful
/// named-systemd path. Within one authority, disagreeing candidates are
/// ambiguous and must not be guessed.
fn systemd_cgroup_path(raw: &str) -> Option<&str> {
    let mut unified = None;
    let mut named = None;
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

        let target = if hierarchy == "0" {
            if !controllers.is_empty() {
                return None;
            }
            &mut unified
        } else {
            if controllers
                .split(',')
                .any(|controller| controller == "name=systemd")
            {
                &mut named
            } else {
                continue;
            }
        };
        match *target {
            Some(existing) if existing != path => return None,
            Some(_) => {}
            None => *target = Some(path),
        }
    }
    match unified {
        Some(path) if path != "/" => Some(path),
        unified => named.filter(|path| *path != "/").or(unified),
    }
}

fn parse_systemd_path(path: &str, system_manager: bool) -> Option<DesktopEvidence> {
    let components = canonical_components(path)?;
    if components.is_empty() {
        return Some(DesktopEvidence::default());
    }
    if components.first() == Some(&"system.slice") || components.first() == Some(&"init.scope") {
        return Some(if system_manager {
            DesktopEvidence {
                role: Some(DesktopRoleEvidence::System),
                ..DesktopEvidence::default()
            }
        } else {
            DesktopEvidence::default()
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

    let (session_id, session_root) =
        if let Some(session_id) = parse_session_scope(*session_or_manager) {
            (Some(session_id), true)
        } else if let Some(manager_uid) = parse_user_manager(*session_or_manager) {
            if manager_uid != uid {
                // Mismatched user identities cannot be attributed to one desktop
                // session. Do not even use the environment as a weaker fallback.
                return None;
            }
            (None, false)
        } else {
            return None;
        };

    let user_scope = Some(format!("/{}", components[..=2].join("/")));
    let rest = &components[3..];
    let mut evidence = DesktopEvidence {
        session_id,
        user_session: true,
        user_scope,
        environment_eligible: true,
        ..DesktopEvidence::default()
    };

    if rest.is_empty() {
        if session_root {
            evidence.role = Some(DesktopRoleEvidence::Background);
            evidence.environment_eligible = false;
        }
        return Some(evidence);
    }

    match rest[0] {
        // User-manager init.scope and the two standard background/session
        // slices are explicit Background roles. A missing/malformed unit does
        // not turn the whole slice into a guessed Background fact.
        "init.scope" => {
            evidence.role = Some(DesktopRoleEvidence::Background);
            evidence.environment_eligible = false;
        }
        "background.slice" | "session.slice" => {
            if rest
                .get(1)
                .copied()
                .is_some_and(|unit| valid_user_unit(unit))
            {
                evidence.role = Some(DesktopRoleEvidence::Background);
            }
            evidence.environment_eligible = false;
        }
        "app.slice" => {
            if let Some(unit) = rest.get(1).copied() {
                if !valid_user_unit(unit) {
                    evidence.environment_eligible = false;
                } else if valid_application_unit(unit) {
                    let scope = evidence.user_scope.as_deref()?;
                    evidence.app_unit_group =
                        Some(format!("systemd:{scope}/{}", rest[..=1].join("/")));
                }
                // A syntactically valid generic unit is weak context. It may
                // be identified by a validated per-process launcher hint, but
                // never by a merely inherited display variable.
            } else {
                evidence.environment_eligible = false;
            }
        }
        _ => {
            // Generic user@UID.service/<unit> or session-N.scope/<path>
            // membership is only a weak user-session context. It is not
            // Background by itself; complete graphical launch evidence may
            // refine it to App.
        }
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

fn valid_user_unit(component: &str) -> bool {
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

/// Only launcher-shaped units may identify an app family by cgroup alone.
/// A generic but syntactically valid user unit (`sleep.service`,
/// `run-u42.scope`) is not application evidence.
fn valid_application_unit(component: &str) -> bool {
    if !valid_user_unit(component) {
        return false;
    }
    let stem = component
        .strip_suffix(".scope")
        .or_else(|| component.strip_suffix(".service"))
        .unwrap_or_default();
    let standard_app = stem
        .strip_prefix("app-")
        .is_some_and(|identity| !identity.is_empty());
    let snap_app = {
        let mut parts = stem.split('.');
        parts.next() == Some("snap")
            && parts.next().is_some_and(|part| !part.is_empty())
            && parts.next().is_some_and(|part| !part.is_empty())
            && parts.all(|part| !part.is_empty())
    };
    standard_app || snap_app
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DesktopHints {
    flatpak_id: Option<String>,
    snap_instance: Option<String>,
    desktop_file: Option<String>,
    wayland_display: Option<String>,
    wayland_socket: Option<String>,
    display: Option<String>,
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
        return Some(DesktopHints::default());
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
            b"WAYLAND_DISPLAY" => assign_display(&mut hints.wayland_display, value)?,
            b"WAYLAND_SOCKET" => assign_display(&mut hints.wayland_socket, value)?,
            b"DISPLAY" => assign_display(&mut hints.display, value)?,
            _ => {}
        }
    }
    Some(hints)
}

fn assign_display(slot: &mut Option<String>, raw: &[u8]) -> Option<()> {
    let value = std::str::from_utf8(raw).ok()?.trim();
    if value.len() > MAX_HINT_VALUE_BYTES || value.chars().any(char::is_control) {
        return None;
    }
    if value.is_empty() {
        return Some(());
    }
    assign_unique(slot, value.to_owned())
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

    fn graphical_session(&self) -> bool {
        [
            self.wayland_display.as_deref(),
            self.wayland_socket.as_deref(),
            self.display.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|value| !value.is_empty())
    }
}

fn apply_environment_hints(evidence: &mut DesktopEvidence, hints: DesktopHints) {
    if !evidence.user_session || !evidence.environment_eligible || evidence.role.is_some() {
        return;
    }

    if !hints.graphical_session() {
        if evidence.app_unit_group.is_some() {
            evidence.role = Some(DesktopRoleEvidence::Background);
        }
        return;
    }

    if let Some(app_group) = evidence.app_unit_group.clone() {
        evidence.role = Some(DesktopRoleEvidence::App);
        evidence.app_group = Some(app_group);
        return;
    }

    if let (Some(user_scope), Some(app_group)) = (evidence.user_scope.as_deref(), hints.app_group())
    {
        evidence.role = Some(DesktopRoleEvidence::App);
        evidence.app_group = Some(format!("systemd:{user_scope}:{app_group}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_path(rest: &str) -> String {
        format!("/user.slice/user-1000.slice/user@1000.service/{rest}")
    }

    fn parse_v2(path: &str) -> Option<DesktopEvidence> {
        parse_v2_with(path, true)
    }

    fn parse_v2_with(path: &str, system_manager: bool) -> Option<DesktopEvidence> {
        parse_cgroup(format!("0::{path}\n").as_bytes(), system_manager)
    }

    fn apply_graphical_gate(evidence: &mut DesktopEvidence) {
        let hints = parse_environment(b"WAYLAND_DISPLAY=wayland-0\0")
            .expect("complete graphical environment");
        apply_environment_hints(evidence, hints);
    }

    #[test]
    fn app_units_require_graphical_environment_and_keep_distinct_families() {
        let units = [
            "app.slice/app-gnome-org.gnome.Terminal-42.scope",
            "app.slice/app-org.gnome.Nautilus-11.scope",
            "app.slice/app-org.kde.konsole-17.scope",
            "app.slice/app-flatpak-org.gnome.Builder-123.scope",
            "app.slice/snap.firefox.firefox.service",
            "app.slice/snap.gimp.gimp.scope",
        ];
        let mut groups = std::collections::HashSet::new();
        for unit in units {
            let path = user_path(unit);
            let mut evidence = parse_v2(&path).expect("valid user app cgroup");
            assert_eq!(evidence.role, None, "{unit} lacks graphical evidence");
            assert_eq!(
                evidence.role_marker(),
                DesktopRoleEvidence::Unknown,
                "{unit} must remain explicitly Unknown"
            );
            assert!(evidence.app_unit_group.is_some());
            apply_graphical_gate(&mut evidence);
            assert_eq!(
                evidence.role,
                Some(DesktopRoleEvidence::App),
                "{unit} must be an app"
            );
            assert!(evidence.user_session);
            assert_eq!(
                evidence.user_scope.as_deref(),
                Some("/user.slice/user-1000.slice/user@1000.service"),
                "the user scope must be canonicalized once without a second slash"
            );
            let group = evidence.app_group.expect("stable app family");
            assert!(
                !group.contains("systemd://"),
                "app-family keys must not double the scope slash: {group}"
            );
            assert!(group.ends_with(unit), "{group} must retain {unit}");
            assert!(
                groups.insert(group),
                "unrelated units must not share a family"
            );
        }
    }

    #[test]
    fn arbitrary_app_slice_units_need_a_validated_launcher_identity() {
        for unit in ["sleep.service", "run-u42.scope"] {
            assert!(valid_user_unit(unit), "{unit} is syntactically valid");
            assert!(
                !valid_application_unit(unit),
                "{unit} is not a launcher-shaped application unit"
            );
            let mut evidence =
                parse_v2(&user_path(&format!("app.slice/{unit}"))).expect("valid user cgroup");
            assert_eq!(evidence.role, None);
            assert_eq!(evidence.app_unit_group, None);
            assert!(evidence.environment_eligible);

            apply_environment_hints(
                &mut evidence,
                parse_environment(b"WAYLAND_DISPLAY=wayland-0\0")
                    .expect("inherited graphical environment"),
            );
            assert_eq!(
                evidence.role, None,
                "a display variable inherited from the session is not app identity"
            );
            assert_eq!(evidence.app_group, None);
        }

        for unit in [
            "app-org.example.Editor-7.scope",
            "snap.firefox.firefox.service",
            "snap.gimp.gimp.scope",
        ] {
            assert!(
                valid_application_unit(unit),
                "{unit} is a recognized application shape"
            );
        }

        let mut identified =
            parse_v2(&user_path("app.slice/sleep.service")).expect("generic user unit");
        apply_environment_hints(
            &mut identified,
            parse_environment(b"WAYLAND_DISPLAY=wayland-0\0FLATPAK_ID=org.example.SleepHelper\0")
                .expect("validated launcher identity"),
        );
        assert_eq!(identified.role, Some(DesktopRoleEvidence::App));
        assert_eq!(
            identified.app_group.as_deref(),
            Some(
                "systemd:/user.slice/user-1000.slice/user@1000.service:flatpak:org.example.SleepHelper"
            )
        );
    }

    #[test]
    fn descendants_of_one_app_unit_share_its_family_identity() {
        let scope = user_path("app.slice/app-org.kde.konsole-17.scope");
        let mut direct = parse_v2(&scope).expect("scope");
        let mut nested = parse_v2(&format!("{scope}/child")).expect("nested child");
        apply_graphical_gate(&mut direct);
        apply_graphical_gate(&mut nested);
        assert_eq!(direct.role, Some(DesktopRoleEvidence::App));
        assert_eq!(direct.app_group, nested.app_group);
    }

    #[test]
    fn explicit_background_session_roles_are_not_weak_user_context() {
        for path in [
            user_path("background.slice/org.example.worker.service"),
            user_path("session.slice/org.gnome.Shell@wayland.service"),
            user_path("init.scope"),
            "/user.slice/user-1000.slice/session-4.scope".to_owned(),
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

        for path in [
            "/user.slice/user-1000.slice/session-4.scope/org.example.worker".to_owned(),
            user_path("org.example.worker.service"),
            user_path(""),
        ] {
            let evidence = parse_v2(path.trim_end_matches('/')).expect("weak user-session path");
            assert_eq!(
                evidence.role, None,
                "{path} is not authoritative Background"
            );
            assert!(evidence.user_session);
        }
    }

    #[test]
    fn nested_session_app_scope_is_parsed_before_assigning_background() {
        let exact =
            "/user.slice/user-1000.slice/session-4.scope/app.slice/app-org.example.Editor-7.scope";
        let mut evidence = parse_v2(exact).expect("nested session app scope");
        assert_eq!(evidence.session_id, Some(4));
        assert_eq!(evidence.role, None);
        assert_eq!(
            evidence.user_scope.as_deref(),
            Some("/user.slice/user-1000.slice/session-4.scope")
        );
        apply_graphical_gate(&mut evidence);
        assert_eq!(evidence.role, Some(DesktopRoleEvidence::App));
        assert_eq!(
            evidence.app_group.as_deref(),
            Some(
                "systemd:/user.slice/user-1000.slice/session-4.scope/app.slice/app-org.example.Editor-7.scope"
            )
        );
    }

    #[test]
    fn system_cgroups_require_a_real_systemd_pid1_manager() {
        for path in [
            "/system.slice/org.example.service",
            "/system.slice/org.example.service/worker",
            "/init.scope",
            "/init.scope/worker",
        ] {
            let evidence = parse_v2(path).expect("valid system cgroup");
            assert_eq!(evidence.role, Some(DesktopRoleEvidence::System), "{path}");
        }
        for path in ["/system.slice/org.example.service", "/init.scope"] {
            let evidence =
                parse_v2_with(path, false).expect("synthetic WSL cgroup remains parseable");
            assert_eq!(
                evidence.role, None,
                "synthetic WSL cgroup {path} must stay Unknown"
            );
            assert_eq!(
                tm_core::classify::classify(tm_core::classify::ClassifyInput {
                    platform: tm_core::classify::ProcessPlatform::Unix,
                    pid: 3000,
                    name: "dbus-daemon",
                    has_window: None,
                    critical: None,
                    windows_owned: None,
                    desktop_role: Some(evidence.role_marker()),
                }),
                tm_core::model::ProcCategory::Unknown,
                "synthetic WSL evidence must not fall back to a System name fact"
            );
        }
        let unrelated = parse_v2("/machine.slice/app.slice/app-org.example-1.scope")
            .expect("well-formed unrelated cgroup");
        assert_eq!(unrelated.role, None);
        assert!(!unrelated.user_session);
        assert!(pid1_comm_is_systemd(b"systemd\n"));
        assert!(!pid1_comm_is_systemd(b"init\n"));
    }

    #[test]
    fn user_session_infrastructure_is_background_but_system_slice_is_system() {
        let user =
            parse_v2("/user.slice/user-1000.slice/session-4.scope").expect("user session cgroup");
        let system = parse_v2("/system.slice/dbus.service").expect("system service cgroup");
        let classify_with = |role| {
            tm_core::classify::classify(tm_core::classify::ClassifyInput {
                platform: tm_core::classify::ProcessPlatform::Unix,
                pid: 3000,
                name: "dbus-daemon",
                has_window: None,
                critical: None,
                windows_owned: None,
                desktop_role: role,
            })
        };
        assert_eq!(
            classify_with(user.role),
            tm_core::model::ProcCategory::Background
        );
        assert_eq!(
            classify_with(system.role),
            tm_core::model::ProcCategory::System
        );
    }

    #[test]
    fn malformed_escaped_ambiguous_and_unrelated_paths_do_not_guess() {
        for path in [
            "user.slice/user-1000.slice/user@1000.service/app.slice/app-org.example-1.scope",
            "/user.slice/../system.slice/org.example.service",
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.example\\x2d1.scope",
            "/user.slice/user-1000.slice/user@1001.service/app.slice/app-org.example-1.scope",
            "/user.slice/user-1000.slice/app.slice/app-org.example-1.scope",
        ] {
            assert_eq!(parse_v2(path), None, "{path}");
        }
        let malformed_unit =
            parse_v2("/user.slice/user-1000.slice/user@1000.service/app.slice/not-a-unit")
                .expect("recognizable user path");
        assert_eq!(malformed_unit.role, None);
        assert!(!malformed_unit.environment_eligible);
        let unrelated = parse_v2("/foo.slice/thing.service").expect("well-formed path");
        assert_eq!(unrelated, DesktopEvidence::default());
    }

    #[test]
    fn hybrid_cgroups_prefer_unified_v2_and_same_authority_conflicts_fail_closed() {
        let raw = "\
2:cpu,cpuacct:/
1:name=systemd:/user.slice/user-1000.slice/session-7.scope
0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.example-1.scope
";
        let evidence = parse_cgroup(raw.as_bytes(), true).expect("hybrid cgroup");
        assert_eq!(
            evidence.role, None,
            "app.slice still awaits its graphic gate"
        );
        assert!(evidence.app_unit_group.is_some());

        let ambiguous = "\
0::/user.slice/user-1000.slice/session-4.scope
0::/user.slice/user-1000.slice/session-5.scope
";
        assert_eq!(parse_cgroup(ambiguous.as_bytes(), true), None);
        let duplicate = "\
0::/user.slice/user-1000.slice/session-4.scope
0::/user.slice/user-1000.slice/session-4.scope
";
        assert_eq!(
            parse_cgroup(duplicate.as_bytes(), true)
                .expect("identical duplicate candidate")
                .role,
            Some(DesktopRoleEvidence::Background)
        );
    }

    #[test]
    fn cgroup_v1_named_systemd_hierarchy_is_supported() {
        let evidence = parse_cgroup(
            b"2:cpu,cpuacct:/\n1:name=systemd:/user.slice/user-1000.slice/session-7.scope\n",
            true,
        )
        .expect("v1 systemd path");
        assert_eq!(evidence.role, Some(DesktopRoleEvidence::Background));
        assert_eq!(evidence.session_id, Some(7));
    }

    #[test]
    fn hybrid_cgroups_do_not_let_a_root_unified_path_hide_useful_named_evidence() {
        let named_useful = "\
0::/
1:name=systemd:/user.slice/user-1000.slice/session-7.scope
";
        let evidence = parse_cgroup(named_useful.as_bytes(), true).expect("hybrid cgroup");
        assert_eq!(evidence.role, Some(DesktopRoleEvidence::Background));
        assert_eq!(evidence.session_id, Some(7));

        let unified_useful = "\
1:name=systemd:/
0::/user.slice/user-1000.slice/session-8.scope
";
        let evidence = parse_cgroup(unified_useful.as_bytes(), true).expect("hybrid cgroup");
        assert_eq!(evidence.role, Some(DesktopRoleEvidence::Background));
        assert_eq!(evidence.session_id, Some(8));
    }

    #[test]
    fn desktop_launch_hints_require_graphical_user_session_evidence() {
        let raw = b"PATH=/usr/bin\0WAYLAND_DISPLAY=wayland-0\0XDG_CURRENT_DESKTOP=KDE\0GIO_LAUNCHED_DESKTOP_FILE=/usr/share/applications/org.gnome.Terminal.desktop\0";
        let hints = parse_environment(raw).expect("complete environment");
        assert_eq!(
            hints.app_group().as_deref(),
            Some("desktop:org.gnome.Terminal.desktop")
        );

        let mut user = DesktopEvidence {
            user_session: true,
            user_scope: Some("/user.slice/user-1000.slice/session-4.scope".into()),
            environment_eligible: true,
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
            user_session: true,
            user_scope: Some("/user.slice/user-1001.slice/session-5.scope".into()),
            environment_eligible: true,
            ..DesktopEvidence::default()
        };
        apply_environment_hints(&mut other_user, hints.clone());
        assert_ne!(
            user.app_group, other_user.app_group,
            "launcher hints must not merge across user sessions"
        );

        for path in [
            user_path("background.slice/org.example.launcher.service"),
            user_path("session.slice/org.gnome.Shell@wayland.service"),
            user_path("init.scope"),
            "/user.slice/user-1000.slice/session-4.scope".to_owned(),
        ] {
            let mut background = parse_v2(&path).expect("authoritative background path");
            apply_environment_hints(&mut background, hints.clone());
            assert_eq!(
                background.role,
                Some(DesktopRoleEvidence::Background),
                "an App hint must not override {path}"
            );
            assert_eq!(background.app_group, None);
        }

        let mut unrelated = DesktopEvidence::default();
        apply_environment_hints(
            &mut unrelated,
            parse_environment(b"WAYLAND_DISPLAY=wayland-0\0FLATPAK_ID=org.example.App\0")
                .expect("complete environment"),
        );
        assert_eq!(unrelated, DesktopEvidence::default());

        let no_graphical_gate = parse_environment(
            b"XDG_CURRENT_DESKTOP=GNOME\0GIO_LAUNCHED_DESKTOP_FILE=/usr/share/applications/a.desktop\0",
        )
        .expect("complete environment");
        let mut user = DesktopEvidence {
            user_session: true,
            user_scope: Some("/user.slice/user-1000.slice/session-4.scope".into()),
            environment_eligible: true,
            ..DesktopEvidence::default()
        };
        apply_environment_hints(&mut user, no_graphical_gate);
        assert_eq!(user.role, None);
        assert_eq!(user.app_group, None);
    }

    #[test]
    fn headless_gui_and_unavailable_app_slice_roles_are_distinct() {
        let path = user_path("app.slice/app-org.example.Tool-9.scope");
        let unavailable = parse_v2(&path).expect("app slice");
        assert_eq!(unavailable.role, None);
        assert_eq!(unavailable.role_marker(), DesktopRoleEvidence::Unknown);

        let mut headless = parse_v2(&path).expect("app slice");
        apply_environment_hints(
            &mut headless,
            parse_environment(b"PATH=/usr/bin\0WAYLAND_DISPLAY=\0FLATPAK_ID=org.example.Tool\0")
                .expect("complete headless environment"),
        );
        assert_eq!(headless.role, Some(DesktopRoleEvidence::Background));
        assert_eq!(headless.app_group, None);

        let mut gui = parse_v2(&path).expect("app slice");
        apply_environment_hints(
            &mut gui,
            parse_environment(b"DISPLAY=:0\0FLATPAK_ID=org.example.Tool\0")
                .expect("complete GUI environment"),
        );
        assert_eq!(gui.role, Some(DesktopRoleEvidence::App));
        assert_eq!(
            gui.app_group.as_deref(),
            Some(
                "systemd:/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.example.Tool-9.scope"
            )
        );
    }

    #[test]
    fn flatpak_and_snap_hints_supply_fallback_identity() {
        let flatpak =
            parse_environment(b"WAYLAND_DISPLAY=wayland-0\0FLATPAK_ID=org.gnome.Builder\0")
                .expect("environment");
        assert_eq!(
            flatpak.app_group().as_deref(),
            Some("flatpak:org.gnome.Builder")
        );
        let snap = parse_environment(b"WAYLAND_SOCKET=3\0SNAP_INSTANCE_NAME=firefox_foo\0")
            .expect("environment");
        assert_eq!(snap.app_group().as_deref(), Some("snap:firefox_foo"));
        assert!(snap.graphical_session());
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
                platform: tm_core::classify::ProcessPlatform::Unix,
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
        assert_eq!(
            parse_environment(b"WAYLAND_DISPLAY=a\0WAYLAND_DISPLAY=b\0"),
            None,
            "disagreeing duplicate graphical evidence is ambiguous"
        );
        assert!(
            parse_environment(b"WAYLAND_DISPLAY=a\0WAYLAND_DISPLAY=a\0")
                .expect("identical duplicate")
                .graphical_session()
        );
        let whitespace_display =
            parse_environment(b"WAYLAND_DISPLAY=   \0DISPLAY=\t\0FLATPAK_ID=org.example.App\0")
                .expect("complete whitespace-only display environment");
        assert!(
            !whitespace_display.graphical_session(),
            "whitespace-only display variables must not satisfy the graphical gate"
        );
        let mut whitespace_app =
            parse_v2(&user_path("app.slice/app-org.example.App-1.scope")).expect("app slice");
        apply_environment_hints(&mut whitespace_app, whitespace_display);
        assert_eq!(
            whitespace_app.role,
            Some(DesktopRoleEvidence::Background),
            "a whitespace-only display is a complete headless environment"
        );
        assert_eq!(whitespace_app.app_group, None);
    }

    #[test]
    fn malformed_cgroup_and_unavailable_environment_remain_unknown() {
        assert_eq!(parse_cgroup(b"0::not-absolute\n", true), None);
        assert_eq!(read_environment_hints(u32::MAX), None);
        let evidence = DesktopEvidence {
            role: parse_cgroup(b"0::not-absolute\n", true).and_then(|evidence| evidence.role),
            ..DesktopEvidence::default()
        };
        assert_eq!(
            tm_core::classify::classify(tm_core::classify::ClassifyInput {
                platform: tm_core::classify::ProcessPlatform::Unix,
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
        assert_eq!(app.role, None);
        assert_eq!(app.role_marker(), DesktopRoleEvidence::Unknown);
        assert!(app.app_unit_group.is_some());
    }

    #[test]
    fn evidence_cache_expires_with_stagger_and_prunes_dead_pids() {
        let now = Instant::now();
        let first = DesktopEvidence {
            role: Some(DesktopRoleEvidence::App),
            app_group: Some("systemd:/scope/app".into()),
            session_id: Some(7),
            ..DesktopEvidence::default()
        };
        let second = DesktopEvidence {
            role: Some(DesktopRoleEvidence::Background),
            ..DesktopEvidence::default()
        };
        let mut cache = DesktopEvidenceCache::default();
        cache.remember(10, 100, now, first.clone());

        assert_eq!(
            cache.cached(10, 100, now + Duration::from_secs(1), |_| Some(100)),
            Some(first.clone())
        );
        assert_eq!(
            cache.cached(10, 100, now + evidence_ttl(10, 100), |_| Some(100)),
            None,
            "the staggered TTL must expire"
        );
        assert_eq!(
            cache.cached(10, 101, now + Duration::from_secs(1), |_| Some(101)),
            None,
            "a new PID generation must refresh immediately"
        );

        cache.remember(10, 101, now + Duration::from_secs(2), second.clone());
        assert_eq!(
            cache.cached(10, 100, now + Duration::from_secs(2), |_| Some(101)),
            None
        );
        assert_eq!(
            cache.cached(10, 101, now + Duration::from_secs(2), |_| Some(101)),
            Some(second)
        );

        cache.remember(
            20,
            200,
            now,
            DesktopEvidence {
                role: Some(DesktopRoleEvidence::Background),
                ..DesktopEvidence::default()
            },
        );
        cache.retain_live(&HashSet::from([10]));
        assert_eq!(
            cache.cached(20, 200, now, |_| Some(200)),
            None,
            "dead PIDs are pruned"
        );
    }

    #[test]
    fn cache_hit_generation_mismatch_invalidates_and_reinspects() {
        let now = Instant::now();
        let first = DesktopEvidence {
            role: Some(DesktopRoleEvidence::App),
            app_group: Some("systemd:/scope/app".into()),
            session_id: Some(7),
            ..DesktopEvidence::default()
        };
        let replacement = DesktopEvidence {
            role: Some(DesktopRoleEvidence::Unknown),
            ..DesktopEvidence::default()
        };
        let mut cache = DesktopEvidenceCache::default();
        cache.remember(10, 100, now, first);

        assert_eq!(
            cache.cached(10, 100, now + Duration::from_secs(1), |_| Some(101)),
            None,
            "a cache hit must revalidate the current /proc generation"
        );
        assert!(
            !cache.entries.contains_key(&(10, 100)),
            "the stale generation entry must be removed"
        );

        cache.remember(
            10,
            100,
            now,
            DesktopEvidence {
                role: Some(DesktopRoleEvidence::App),
                ..DesktopEvidence::default()
            },
        );
        let mut manager_reads = 0;
        let mut evidence_reads = 0;
        let got = cache.evidence_with(
            10,
            100,
            now + Duration::from_secs(1),
            |_| Some(101),
            || {
                manager_reads += 1;
                true
            },
            |_, _, _| {
                evidence_reads += 1;
                replacement.clone()
            },
        );
        assert_eq!(got, replacement);
        assert_eq!(manager_reads, 1);
        assert_eq!(evidence_reads, 1, "a generation mismatch must reinspect");
    }

    #[test]
    fn same_generation_exec_staleness_is_bounded_by_the_cache_ttl() {
        let now = Instant::now();
        let cached = DesktopEvidence {
            role: Some(DesktopRoleEvidence::App),
            app_group: Some("flatpak:org.example.BeforeExec".into()),
            ..DesktopEvidence::default()
        };
        let after_exec = DesktopEvidence {
            role: Some(DesktopRoleEvidence::Background),
            ..DesktopEvidence::default()
        };
        let mut cache = DesktopEvidenceCache::default();
        cache.remember(10, 100, now, cached.clone());
        let ttl = evidence_ttl(10, 100);

        assert_eq!(
            cache.evidence_with(
                10,
                100,
                now + ttl.saturating_sub(Duration::from_millis(1)),
                |_| Some(100),
                || panic!("a fresh hit must not query the system manager"),
                |_, _, _| panic!("a fresh hit must not refresh evidence"),
            ),
            cached,
            "exec preserves PID/start ticks, so the old environment is reused briefly"
        );

        let mut evidence_reads = 0;
        assert_eq!(
            cache.evidence_with(
                10,
                100,
                now + ttl,
                |_| Some(100),
                || true,
                |_, _, _| {
                    evidence_reads += 1;
                    after_exec.clone()
                },
            ),
            after_exec,
            "the bounded TTL refreshes after an environment-changing exec"
        );
        assert_eq!(evidence_reads, 1);
    }

    #[test]
    fn cache_expiry_jitter_is_deterministic_and_near_the_nominal_ttl() {
        let floor = DESKTOP_EVIDENCE_TTL - Duration::from_millis(500);
        let ceiling = DESKTOP_EVIDENCE_TTL + Duration::from_millis(500);
        let ttls: HashSet<_> = (1..=32)
            .map(|pid| evidence_ttl(pid, i64::from(pid) * 100))
            .collect();
        assert!(
            ttls.len() > 1,
            "different generations must not all expire in one synchronized sweep"
        );
        assert!(
            ttls.iter().all(|ttl| (floor..=ceiling).contains(ttl)),
            "the jitter must stay within one second of the nominal TTL"
        );
        assert_eq!(evidence_ttl(7, 700), evidence_ttl(7, 700));
    }

    #[test]
    fn evidence_from_an_old_process_generation_is_discarded() {
        let pid = std::process::id();
        let ticks = super::super::linux_proc_start_ticks(pid).expect("own start ticks");
        assert_eq!(inspect(pid, ticks + 1, true), DesktopEvidence::default());
    }
}
