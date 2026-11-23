//! X11/EWMH window ownership for process classification.
//!
//! The window manager publishes its managed clients in `_NET_CLIENT_LIST`
//! (initial mapping order) and `_NET_CLIENT_LIST_STACKING` (bottom-to-top
//! stacking order). Each client's `_NET_WM_PID` provides the process owner.
//! Both lists are merged because either property can be temporarily missing
//! or stale in smaller window managers.
//!
//! A window is eligible when its ICCCM state is Normal or Iconic, or when the
//! state is absent and the X server still reports it viewable. Withdrawn and
//! override-redirect windows contribute no owner. This keeps minimized and
//! off-desktop windows classified as Apps, matching Task Manager.
//!
//! Negative evidence is only published as [`WindowCoverage::Complete`] for a
//! local X11 session with a readable EWMH client list and complete PID
//! attribution. Native Wayland clients have no portable system-wide window
//! inventory, so every XWayland observation remains partial. Missing or
//! malformed properties and bounded-budget truncation also produce partial
//! coverage; callers must then keep unobserved window ownership unknown
//! instead of turning it into a false "windowless" fact.
//!
//! The collector reuses one connection and atom set, reconnecting with a
//! short backoff. It rejects remote displays (which could block the sampling
//! engine on network I/O), caps the client list and per-window work, and
//! reconnects on a connection-level failure.

use std::collections::HashSet;
use std::time::{Duration, Instant};
use x11rb::connection::Connection as _;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, MapState, Window};
use x11rb::rust_connection::RustConnection;

/// Bound root-property reads so a corrupt property cannot force an unbounded
/// allocation. Truncated properties are treated as unavailable.
const MAX_PROPERTY_LONGS: u32 = 16_384;
/// Bound the synchronous per-window work done once per sample.
const MAX_CLIENT_WINDOWS: usize = 512;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

const WM_STATE_WITHDRAWN: u32 = 0;
const WM_STATE_NORMAL: u32 = 1;
const WM_STATE_ICONIC: u32 = 3;

/// How much of the session's window ownership is observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowCoverage {
    /// The local X11 client list was read and every eligible window had a
    /// trustworthy local PID. Absence of a PID is a real "no window" fact.
    Complete,
    /// Some owners are known, but absence cannot be treated as windowless.
    /// This includes XWayland/native-Wayland sessions, missing PID metadata,
    /// malformed properties, or a bounded-budget truncation.
    Partial,
    /// No usable observation was available.
    Unavailable,
}

#[derive(Debug, Clone)]
pub(crate) struct WindowObservation {
    pub owners: HashSet<u32>,
    pub coverage: WindowCoverage,
}

impl WindowObservation {
    fn unavailable() -> Self {
        Self {
            owners: HashSet::new(),
            coverage: WindowCoverage::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Atoms {
    root: Window,
    client_list: Option<Atom>,
    stacking: Option<Atom>,
    pid: Option<Atom>,
    state: Option<Atom>,
    client_machine: Option<Atom>,
}

enum PropertyReply {
    Values(Vec<u32>),
    Missing,
    Invalid,
    XError,
    ConnectionFailed,
}

enum TextPropertyReply {
    Value(String),
    Missing,
    Invalid,
    XError,
    ConnectionFailed,
}

struct QueryOutcome {
    observation: WindowObservation,
    connection_failed: bool,
}

/// Persistent X11 observer owned by the Linux collector.
pub(crate) struct WindowObserver {
    connection: Option<RustConnection>,
    atoms: Option<Atoms>,
    retry_at: Option<Instant>,
    last_coverage: Option<WindowCoverage>,
}

impl Default for WindowObserver {
    fn default() -> Self {
        Self {
            connection: None,
            atoms: None,
            retry_at: None,
            last_coverage: None,
        }
    }
}

impl WindowObserver {
    pub(crate) fn observe(&mut self) -> WindowObservation {
        let now = Instant::now();
        let Some(display) = std::env::var("DISPLAY").ok().filter(|value| !value.is_empty()) else {
            self.record_coverage(WindowCoverage::Unavailable);
            return WindowObservation::unavailable();
        };
        if !local_display(&display) {
            self.record_coverage(WindowCoverage::Unavailable);
            return WindowObservation::unavailable();
        }
        if self.retry_at.is_some_and(|retry_at| now < retry_at) {
            self.record_coverage(WindowCoverage::Unavailable);
            return WindowObservation::unavailable();
        }

        if self.connection.is_none() || self.atoms.is_none() {
            match connect() {
                Some((connection, atoms)) => {
                    self.connection = Some(connection);
                    self.atoms = Some(atoms);
                }
                None => {
                    self.connection = None;
                    self.atoms = None;
                    self.retry_at = Some(now + RECONNECT_BACKOFF);
                    self.record_coverage(WindowCoverage::Unavailable);
                    return WindowObservation::unavailable();
                }
            }
        }

        let Some(connection) = self.connection.as_ref() else {
            return WindowObservation::unavailable();
        };
        let Some(atoms) = self.atoms else {
            return WindowObservation::unavailable();
        };
        let mut outcome = query_window_owners(connection, atoms);
        if outcome.connection_failed {
            self.connection = None;
            self.atoms = None;
            self.retry_at = Some(now + RECONNECT_BACKOFF);
        } else {
            self.retry_at = None;
        }
        if wayland_session() && outcome.observation.coverage != WindowCoverage::Unavailable {
            outcome.observation.coverage = WindowCoverage::Partial;
        }
        self.record_coverage(outcome.observation.coverage);
        outcome.observation
    }

    fn record_coverage(&mut self, coverage: WindowCoverage) {
        if self.last_coverage == Some(coverage) {
            return;
        }
        self.last_coverage = Some(coverage);
        tracing::info!(?coverage, "Linux window ownership coverage changed");
    }
}

fn connect() -> Option<(RustConnection, Atoms)> {
    let (connection, screen) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    Some((
        connection,
        Atoms {
            root,
            client_list: intern_atom(&connection, b"_NET_CLIENT_LIST"),
            stacking: intern_atom(&connection, b"_NET_CLIENT_LIST_STACKING"),
            pid: intern_atom(&connection, b"_NET_WM_PID"),
            state: intern_atom(&connection, b"WM_STATE"),
            client_machine: intern_atom(&connection, b"WM_CLIENT_MACHINE"),
        },
    ))
}

fn query_window_owners(connection: &RustConnection, atoms: Atoms) -> QueryOutcome {
    let Some(pid_atom) = atoms.pid else {
        return QueryOutcome {
            observation: WindowObservation::unavailable(),
            connection_failed: false,
        };
    };

    let client_list = atoms.client_list.map_or(PropertyReply::Missing, |atom| {
        read_u32_property(
            connection,
            atoms.root,
            atom,
            Some(AtomEnum::WINDOW.into()),
            MAX_PROPERTY_LONGS,
        )
    });
    let stacking = atoms.stacking.map_or(PropertyReply::Missing, |atom| {
        read_u32_property(
            connection,
            atoms.root,
            atom,
            Some(AtomEnum::WINDOW.into()),
            MAX_PROPERTY_LONGS,
        )
    });
    if matches!(client_list, PropertyReply::ConnectionFailed)
        || matches!(stacking, PropertyReply::ConnectionFailed)
    {
        return QueryOutcome {
            observation: WindowObservation::unavailable(),
            connection_failed: true,
        };
    }

    let mut coverage = WindowCoverage::Complete;
    let mut list_available = false;
    let mut client_windows = Vec::new();
    let mut stacking_windows = Vec::new();
    for (reply, windows) in [
        (client_list, &mut client_windows),
        (stacking, &mut stacking_windows),
    ] {
        match reply {
            PropertyReply::Values(values) => {
                list_available = true;
                windows.extend(values);
            }
            PropertyReply::Missing => {}
            PropertyReply::Invalid | PropertyReply::XError => {
                coverage = WindowCoverage::Partial;
            }
            PropertyReply::ConnectionFailed => unreachable!("handled above"),
        }
    }
    if !list_available {
        return QueryOutcome {
            observation: WindowObservation {
                owners: HashSet::new(),
                coverage: WindowCoverage::Unavailable,
            },
            connection_failed: false,
        };
    }

    let mut windows = merge_window_lists(&client_windows, &stacking_windows);
    if windows.len() > MAX_CLIENT_WINDOWS {
        windows.truncate(MAX_CLIENT_WINDOWS);
        coverage = WindowCoverage::Partial;
    }

    let local_hostname = local_hostname();
    let mut owners = HashSet::new();
    for window in windows {
        let attributes = match connection.get_window_attributes(window) {
            Ok(cookie) => match cookie.reply() {
                Ok(attributes) => attributes,
                Err(ReplyError::X11Error(_)) => continue,
                Err(ReplyError::ConnectionError(_)) => {
                    return QueryOutcome {
                        observation: WindowObservation {
                            owners,
                            coverage: WindowCoverage::Unavailable,
                        },
                        connection_failed: true,
                    };
                }
            },
            Err(_) => {
                return QueryOutcome {
                    observation: WindowObservation {
                        owners,
                        coverage: WindowCoverage::Unavailable,
                    },
                    connection_failed: true,
                };
            }
        };

        let wm_state = match atoms.state.map_or(PropertyReply::Missing, |atom| {
            read_u32_property(connection, window, atom, Some(atom), 2)
        }) {
            PropertyReply::Values(values) => match parse_wm_state(&values) {
                Some(state) => Some(state),
                None => {
                    coverage = WindowCoverage::Partial;
                    None
                }
            },
            PropertyReply::Missing => None,
            PropertyReply::Invalid | PropertyReply::XError => {
                coverage = WindowCoverage::Partial;
                None
            }
            PropertyReply::ConnectionFailed => {
                return QueryOutcome {
                    observation: WindowObservation {
                        owners,
                        coverage: WindowCoverage::Unavailable,
                    },
                    connection_failed: true,
                };
            }
        };
        if !window_is_eligible(attributes.map_state, attributes.override_redirect, wm_state) {
            continue;
        }

        if let Some(machine_atom) = atoms.client_machine {
            match read_text_property(connection, window, machine_atom, 1024) {
                TextPropertyReply::Value(machine)
                    if !client_machine_matches(&machine, local_hostname.as_deref()) =>
                {
                    // The raw PID may belong to a container or another host.
                    // Positive attribution from this connection is not safe.
                    coverage = WindowCoverage::Partial;
                    continue;
                }
                TextPropertyReply::Invalid | TextPropertyReply::XError => {
                    coverage = WindowCoverage::Partial;
                }
                TextPropertyReply::ConnectionFailed => {
                    return QueryOutcome {
                        observation: WindowObservation {
                            owners,
                            coverage: WindowCoverage::Unavailable,
                        },
                        connection_failed: true,
                    };
                }
                TextPropertyReply::Value(_) | TextPropertyReply::Missing => {}
            }
        }

        match read_u32_property(
            connection,
            window,
            pid_atom,
            Some(AtomEnum::CARDINAL.into()),
            1,
        ) {
            PropertyReply::Values(values) => match parse_window_pid(&values) {
                Some(pid) => {
                    owners.insert(pid);
                }
                None => coverage = WindowCoverage::Partial,
            },
            PropertyReply::Missing | PropertyReply::Invalid | PropertyReply::XError => {
                coverage = WindowCoverage::Partial;
            }
            PropertyReply::ConnectionFailed => {
                return QueryOutcome {
                    observation: WindowObservation {
                        owners,
                        coverage: WindowCoverage::Unavailable,
                    },
                    connection_failed: true,
                };
            }
        }
    }

    QueryOutcome {
        observation: WindowObservation { owners, coverage },
        connection_failed: false,
    }
}

fn intern_atom(connection: &RustConnection, name: &[u8]) -> Option<Atom> {
    connection
        .intern_atom(true, name)
        .ok()?
        .reply()
        .ok()
        .map(|reply| reply.atom)
        .filter(|&atom| atom != x11rb::NONE)
}

fn read_u32_property(
    connection: &RustConnection,
    window: Window,
    property: Atom,
    expected_type: Option<Atom>,
    max_longs: u32,
) -> PropertyReply {
    let cookie = match connection.get_property(
        false,
        window,
        property,
        AtomEnum::ANY,
        0,
        max_longs,
    ) {
        Ok(cookie) => cookie,
        Err(_) => return PropertyReply::ConnectionFailed,
    };
    let reply = match cookie.reply() {
        Ok(reply) => reply,
        Err(ReplyError::X11Error(_)) => return PropertyReply::XError,
        Err(ReplyError::ConnectionError(_)) => return PropertyReply::ConnectionFailed,
    };
    if reply.type_ == x11rb::NONE || reply.format == 0 {
        return PropertyReply::Missing;
    }
    if reply.format != 32
        || reply.bytes_after != 0
        || expected_type.is_some_and(|expected| reply.type_ != expected)
    {
        return PropertyReply::Invalid;
    }
    reply
        .value32()
        .map(|values| PropertyReply::Values(values.collect()))
        .unwrap_or(PropertyReply::Invalid)
}

fn read_text_property(
    connection: &RustConnection,
    window: Window,
    property: Atom,
    max_bytes: u32,
) -> TextPropertyReply {
    let cookie = match connection.get_property(
        false,
        window,
        property,
        AtomEnum::STRING,
        0,
        max_bytes.div_ceil(4),
    ) {
        Ok(cookie) => cookie,
        Err(_) => return TextPropertyReply::ConnectionFailed,
    };
    let reply = match cookie.reply() {
        Ok(reply) => reply,
        Err(ReplyError::X11Error(_)) => return TextPropertyReply::XError,
        Err(ReplyError::ConnectionError(_)) => return TextPropertyReply::ConnectionFailed,
    };
    if reply.type_ == x11rb::NONE || reply.format == 0 {
        return TextPropertyReply::Missing;
    }
    if reply.format != 8 || reply.bytes_after != 0 {
        return TextPropertyReply::Invalid;
    }
    let end = reply
        .value
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(reply.value.len());
    TextPropertyReply::Value(String::from_utf8_lossy(&reply.value[..end]).into_owned())
}

fn merge_window_lists(client_list: &[Window], stacking_list: &[Window]) -> Vec<Window> {
    let capacity = client_list.len().saturating_add(stacking_list.len());
    let mut seen = HashSet::with_capacity(capacity);
    let mut windows = Vec::with_capacity(capacity);
    for &window in client_list.iter().chain(stacking_list.iter()) {
        if window != x11rb::NONE && seen.insert(window) {
            windows.push(window);
        }
    }
    windows
}

fn window_is_eligible(
    map_state: MapState,
    override_redirect: bool,
    wm_state: Option<u32>,
) -> bool {
    if override_redirect {
        return false;
    }
    match wm_state {
        Some(WM_STATE_WITHDRAWN) => false,
        // ICCCM minimized and off-desktop clients are normally unmapped but
        // remain managed members of the client list.
        Some(WM_STATE_NORMAL | WM_STATE_ICONIC) => true,
        // Unknown non-withdrawn states fall back to the server map state.
        Some(_) | None => map_state == MapState::VIEWABLE,
    }
}

fn parse_wm_state(values: &[u32]) -> Option<u32> {
    match values {
        [state, _icon_window] => Some(*state),
        _ => None,
    }
}

fn parse_window_pid(values: &[u32]) -> Option<u32> {
    match values {
        [pid] if *pid != 0 => Some(*pid),
        _ => None,
    }
}

fn local_display(display: &str) -> bool {
    let host = display
        .rsplit_once(':')
        .map_or(display, |(host, _display_number)| host);
    host.is_empty()
        || matches!(
            host.to_ascii_lowercase().as_str(),
            "unix" | "localhost" | "127.0.0.1" | "[::1]" | "::1"
        )
}

fn wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE")
        .ok()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("wayland"))
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
}

fn local_hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|hostname| hostname.trim().to_string())
        .filter(|hostname| !hostname.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|hostname| !hostname.is_empty()))
}

fn client_machine_matches(claimed: &str, local: Option<&str>) -> bool {
    let claimed = claimed.trim();
    let Some(local) = local else {
        return true;
    };
    if claimed.eq_ignore_ascii_case(local) {
        return true;
    }
    let claimed_host = claimed.split('.').next().unwrap_or(claimed);
    let local_host = local.split('.').next().unwrap_or(local);
    !claimed_host.is_empty() && claimed_host.eq_ignore_ascii_case(local_host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_lists_are_merged_without_duplicates_or_none() {
        assert_eq!(
            merge_window_lists(&[10, 20, 10, x11rb::NONE], &[20, 30, x11rb::NONE]),
            vec![10, 20, 30]
        );
        assert!(merge_window_lists(&[], &[]).is_empty());
    }

    #[test]
    fn normal_and_iconic_clients_are_eligible_even_when_unmapped() {
        assert!(window_is_eligible(
            MapState::UNMAPPED,
            false,
            Some(WM_STATE_NORMAL)
        ));
        assert!(window_is_eligible(
            MapState::UNMAPPED,
            false,
            Some(WM_STATE_ICONIC)
        ));
        assert!(window_is_eligible(MapState::VIEWABLE, false, None));

        assert!(!window_is_eligible(
            MapState::VIEWABLE,
            false,
            Some(WM_STATE_WITHDRAWN)
        ));
        assert!(!window_is_eligible(MapState::UNMAPPED, false, None));
        assert!(!window_is_eligible(MapState::UNVIEWABLE, false, None));
        assert!(!window_is_eligible(MapState::VIEWABLE, true, None));
    }

    #[test]
    fn wm_state_requires_the_full_two_cardinal_property() {
        assert_eq!(parse_wm_state(&[WM_STATE_NORMAL, 0]), Some(WM_STATE_NORMAL));
        assert_eq!(parse_wm_state(&[WM_STATE_ICONIC, 42]), Some(WM_STATE_ICONIC));
        assert_eq!(parse_wm_state(&[]), None);
        assert_eq!(parse_wm_state(&[WM_STATE_NORMAL]), None);
        assert_eq!(parse_wm_state(&[WM_STATE_NORMAL, 1, 2]), None);
    }

    #[test]
    fn window_pid_requires_exactly_one_nonzero_cardinal() {
        assert_eq!(parse_window_pid(&[42]), Some(42));
        assert_eq!(parse_window_pid(&[]), None);
        assert_eq!(parse_window_pid(&[0]), None);
        assert_eq!(parse_window_pid(&[42, 43]), None);
    }

    #[test]
    fn only_local_x11_displays_are_trusted() {
        assert!(local_display(":0"));
        assert!(local_display("unix:0"));
        assert!(local_display("localhost:10.0"));
        assert!(local_display("127.0.0.1:0"));
        assert!(local_display("[::1]:0"));
        assert!(!local_display("remote.example:0"));
    }

    #[test]
    fn client_machine_accepts_short_or_fqdn_local_names() {
        assert!(client_machine_matches("host", Some("host.example")));
        assert!(client_machine_matches("HOST.example", Some("host")));
        assert!(!client_machine_matches("container", Some("host")));
        assert!(client_machine_matches("elsewhere", None));
    }
}
