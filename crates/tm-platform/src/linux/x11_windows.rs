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
//! instead of turning it into a false "windowless" fact. Native-Wayland app
//! roles are instead supplied separately by `desktop_session` from systemd
//! cgroup and capped per-process launcher evidence.
//!
//! The collector reuses one connection and atom set, reconnecting with a
//! short backoff. It rejects remote displays (which could block the sampling
//! engine on network I/O), caps the client list and per-window work, and
//! reconnects on a connection-level failure.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};
use x11rb::connection::Connection as _;
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::res::{self, ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, MapState, Window};
use x11rb::rust_connection::RustConnection;
use x11rb_protocol::parse_display::{ConnectAddress, parse_display};

/// Bound root-property reads so a corrupt property cannot force an unbounded
/// allocation. Truncated properties are treated as unavailable.
const MAX_PROPERTY_LONGS: u32 = 16_384;
/// Bound the synchronous per-window work done once per sample.
const MAX_CLIENT_WINDOWS: usize = 512;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);
const OBSERVATION_TTL: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

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
    /// PID and its `/proc` boot-relative start ticks.
    pub owners: HashMap<u32, i64>,
    pub coverage: WindowCoverage,
}

impl WindowObservation {
    fn unavailable() -> Self {
        Self {
            owners: HashMap::new(),
            coverage: WindowCoverage::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Atoms {
    root: Window,
    multi_screen: bool,
    client_list: Option<Atom>,
    stacking: Option<Atom>,
    pid: Option<Atom>,
    state: Option<Atom>,
    client_machine: Option<Atom>,
}

impl Atoms {
    fn needs_reintern(self) -> bool {
        self.client_list.is_none()
            || self.stacking.is_none()
            || self.pid.is_none()
            || self.state.is_none()
            || self.client_machine.is_none()
    }
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

/// Non-blocking handle owned by the Linux collector.
///
/// All X11 I/O happens on one dedicated thread. If the server wedges, the
/// engine thread keeps sampling and the cached observation expires to
/// unavailable instead of waiting on a blocking round trip.
pub(crate) struct WindowObserver {
    requests: SyncSender<u64>,
    responses: Receiver<(u64, Instant, WindowObservation)>,
    next_sequence: u64,
    outstanding: Option<(u64, Instant)>,
    latest: Option<(u64, Instant, WindowObservation)>,
}

impl WindowObserver {
    pub(crate) fn new() -> Self {
        let (requests, request_rx) = mpsc::sync_channel(1);
        let (response_tx, responses) = mpsc::channel();
        let spawn = std::thread::Builder::new()
            .name("tm-x11-windows".into())
            .spawn(move || {
                let mut observer = BlockingObserver::default();
                while let Ok(sequence) = request_rx.recv() {
                    let observation = observer.observe();
                    if response_tx
                        .send((sequence, Instant::now(), observation))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        if let Err(error) = spawn {
            tracing::warn!(%error, "failed to start Linux X11 window observer");
        }
        Self {
            requests,
            responses,
            next_sequence: 0,
            outstanding: None,
            latest: None,
        }
    }

    pub(crate) fn observe(&mut self) -> WindowObservation {
        let now = Instant::now();
        let mut fresh_sequence = None;
        while let Ok((sequence, observed_at, observation)) = self.responses.try_recv() {
            if self
                .latest
                .as_ref()
                .is_none_or(|(latest_sequence, _, _)| sequence >= *latest_sequence)
            {
                self.latest = Some((sequence, observed_at, observation));
                fresh_sequence = Some(sequence);
            }
            if self
                .outstanding
                .is_some_and(|(outstanding_sequence, _)| outstanding_sequence == sequence)
            {
                self.outstanding = None;
            }
        }

        if self
            .outstanding
            .is_some_and(|(_, sent)| now.saturating_duration_since(sent) > REQUEST_TIMEOUT)
        {
            self.outstanding = None;
        }
        if self.outstanding.is_none() {
            let sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.wrapping_add(1);
            if self.requests.try_send(sequence).is_ok() {
                self.outstanding = Some((sequence, now));
            }
        }

        match &self.latest {
            Some((sequence, observed_at, observation))
                if now.saturating_duration_since(*observed_at) <= OBSERVATION_TTL =>
            {
                let mut observation = observation.clone();
                if observation.coverage == WindowCoverage::Complete
                    && fresh_sequence != Some(*sequence)
                {
                    // Negative evidence is valid only for the just-finished
                    // observation. Cached Complete snapshots may be reused for
                    // positive owners, but absence becomes unknown.
                    observation.coverage = WindowCoverage::Partial;
                }
                observation
            }
            _ => WindowObservation::unavailable(),
        }
    }
}

/// Blocking connection state. This type is confined to the observer thread.
#[derive(Default)]
struct BlockingObserver {
    connection: Option<RustConnection>,
    atoms: Option<Atoms>,
    reintern_atoms: bool,
    retry_at: Option<Instant>,
    last_coverage: Option<WindowCoverage>,
}

impl BlockingObserver {
    fn observe(&mut self) -> WindowObservation {
        let now = Instant::now();
        let Some(display) = std::env::var("DISPLAY")
            .ok()
            .filter(|value| !value.is_empty())
        else {
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
        if self.reintern_atoms
            && let Some(connection) = self.connection.as_ref()
        {
            let root = self.atoms.map_or_else(
                || connection.setup().roots.first().map(|screen| screen.root),
                |atoms| Some(atoms.root),
            );
            match root
                .ok_or(())
                .and_then(|root| intern_atoms(connection, root))
            {
                Ok(atoms) => {
                    self.atoms = Some(atoms);
                    self.reintern_atoms = false;
                }
                Err(()) => {
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
            self.reintern_atoms = atoms.needs_reintern();
            self.retry_at = None;
        }
        if !session_is_x11_only() && outcome.observation.coverage != WindowCoverage::Unavailable {
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
    let atoms = intern_atoms(&connection, root).ok()?;
    Some((connection, atoms))
}

fn intern_atoms(connection: &RustConnection, root: Window) -> Result<Atoms, ()> {
    Ok(Atoms {
        root,
        multi_screen: connection.setup().roots.len() > 1,
        client_list: intern_atom(connection, b"_NET_CLIENT_LIST")?,
        stacking: intern_atom(connection, b"_NET_CLIENT_LIST_STACKING")?,
        pid: intern_atom(connection, b"_NET_WM_PID")?,
        state: intern_atom(connection, b"WM_STATE")?,
        client_machine: intern_atom(connection, b"WM_CLIENT_MACHINE")?,
    })
}

struct ClientPids {
    clients: Vec<res::Client>,
    by_client: HashMap<u32, u32>,
}

enum ClientPidQuery {
    Available(ClientPids),
    Unsupported,
    ConnectionFailed,
}

fn query_client_pids(connection: &RustConnection) -> ClientPidQuery {
    let clients = match connection.res_query_clients() {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => reply.clients,
            Err(ReplyError::ConnectionError(_)) => return ClientPidQuery::ConnectionFailed,
            Err(ReplyError::X11Error(_)) => return ClientPidQuery::Unsupported,
        },
        Err(ConnectionError::UnsupportedExtension) => return ClientPidQuery::Unsupported,
        Err(_) => return ClientPidQuery::ConnectionFailed,
    };
    if clients.is_empty() {
        return ClientPidQuery::Available(ClientPids {
            clients,
            by_client: HashMap::new(),
        });
    }
    let specs: Vec<ClientIdSpec> = clients
        .iter()
        .map(|client| ClientIdSpec {
            client: client.resource_base,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        })
        .collect();
    let reply = match connection.res_query_client_ids(&specs) {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => reply,
            Err(ReplyError::ConnectionError(_)) => return ClientPidQuery::ConnectionFailed,
            Err(ReplyError::X11Error(_)) => return ClientPidQuery::Unsupported,
        },
        Err(ConnectionError::UnsupportedExtension) => return ClientPidQuery::Unsupported,
        Err(_) => return ClientPidQuery::ConnectionFailed,
    };
    let by_client = reply
        .ids
        .into_iter()
        .filter(|value| u32::from(value.spec.mask) & u32::from(ClientIdMask::LOCAL_CLIENT_PID) != 0)
        .filter_map(|value| {
            let res::ClientIdValue { spec, value } = value;
            match value.as_slice() {
                [pid] if *pid != 0 => Some((spec.client, *pid)),
                _ => None,
            }
        })
        .collect();
    ClientPidQuery::Available(ClientPids { clients, by_client })
}

fn client_pid_for_window(window: Window, clients: &ClientPids) -> Option<u32> {
    let client = clients
        .clients
        .iter()
        .find(|client| (window & !client.resource_mask) == client.resource_base)?;
    clients.by_client.get(&client.resource_base).copied()
}

fn record_owner(owners: &mut HashMap<u32, i64>, pid: u32, coverage: &mut WindowCoverage) {
    match super::linux_proc_start_ticks(pid) {
        Some(start_ticks) => {
            owners.insert(pid, start_ticks);
        }
        None => *coverage = WindowCoverage::Partial,
    }
}

fn query_window_owners(connection: &RustConnection, atoms: Atoms) -> QueryOutcome {
    let pid_atom = atoms.pid;

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

    let mut coverage = if atoms.multi_screen {
        WindowCoverage::Partial
    } else {
        WindowCoverage::Complete
    };
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
                owners: HashMap::new(),
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

    let client_pids = match query_client_pids(connection) {
        ClientPidQuery::Available(client_pids) => Some(client_pids),
        ClientPidQuery::Unsupported => {
            coverage = WindowCoverage::Partial;
            None
        }
        ClientPidQuery::ConnectionFailed => {
            return QueryOutcome {
                observation: WindowObservation {
                    owners: HashMap::new(),
                    coverage: WindowCoverage::Unavailable,
                },
                connection_failed: true,
            };
        }
    };
    let local_hostname = local_hostname();
    let mut owners = HashMap::new();
    for window in windows {
        let attributes = match connection.get_window_attributes(window) {
            Ok(cookie) => match cookie.reply() {
                Ok(attributes) => attributes,
                Err(ReplyError::X11Error(_)) => {
                    // The window disappeared or changed identity between the
                    // client-list query and this read. We cannot turn that
                    // unobserved window into a negative ownership fact.
                    coverage = WindowCoverage::Partial;
                    continue;
                }
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
        match window_eligibility(attributes.map_state, attributes.override_redirect, wm_state) {
            WindowEligibility::Eligible => {}
            WindowEligibility::Ineligible => continue,
            WindowEligibility::Unknown => {
                coverage = WindowCoverage::Partial;
                continue;
            }
        }

        if let Some(client_pids) = &client_pids {
            if let Some(pid) = client_pid_for_window(window, client_pids) {
                record_owner(&mut owners, pid, &mut coverage);
                continue;
            }
            coverage = WindowCoverage::Partial;
        }

        let Some(machine_atom) = atoms.client_machine else {
            coverage = WindowCoverage::Partial;
            continue;
        };
        match read_text_property(connection, window, machine_atom, 1024) {
            TextPropertyReply::Value(machine)
                if client_machine_matches(&machine, local_hostname.as_deref()) => {}
            // Missing, malformed, or unverifiable machine identity means the
            // raw PID may belong to another host or a PID namespace.
            TextPropertyReply::Value(_)
            | TextPropertyReply::Missing
            | TextPropertyReply::Invalid
            | TextPropertyReply::XError => {
                coverage = WindowCoverage::Partial;
                continue;
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
        }

        let Some(pid_atom) = pid_atom else {
            coverage = WindowCoverage::Partial;
            continue;
        };
        match read_u32_property(
            connection,
            window,
            pid_atom,
            Some(AtomEnum::CARDINAL.into()),
            1,
        ) {
            PropertyReply::Values(values) => match parse_window_pid(&values) {
                Some(pid) => {
                    record_owner(&mut owners, pid, &mut coverage);
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

fn intern_atom(connection: &RustConnection, name: &[u8]) -> Result<Option<Atom>, ()> {
    let cookie = connection.intern_atom(true, name).map_err(|_| ())?;
    let reply = match cookie.reply() {
        Ok(reply) => reply,
        Err(ReplyError::X11Error(_)) => return Ok(None),
        Err(ReplyError::ConnectionError(_)) => return Err(()),
    };
    Ok((reply.atom != x11rb::NONE).then_some(reply.atom))
}

fn read_u32_property(
    connection: &RustConnection,
    window: Window,
    property: Atom,
    expected_type: Option<Atom>,
    max_longs: u32,
) -> PropertyReply {
    let cookie = match connection.get_property(false, window, property, AtomEnum::ANY, 0, max_longs)
    {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowEligibility {
    Eligible,
    Ineligible,
    Unknown,
}

fn window_eligibility(
    map_state: MapState,
    override_redirect: bool,
    wm_state: Option<u32>,
) -> WindowEligibility {
    if override_redirect {
        return WindowEligibility::Ineligible;
    }
    match wm_state {
        Some(WM_STATE_WITHDRAWN) => WindowEligibility::Ineligible,
        // ICCCM minimized and off-desktop clients are normally unmapped but
        // remain managed members of the client list.
        Some(state) if state == WM_STATE_NORMAL || state == WM_STATE_ICONIC => {
            WindowEligibility::Eligible
        }
        Some(_) | None => {
            if map_state == MapState::VIEWABLE {
                WindowEligibility::Eligible
            } else {
                // Without a usable WM_STATE, an unmapped managed client may
                // be minimized or on another desktop. Treat that as unknown
                // rather than proving it has no window.
                WindowEligibility::Unknown
            }
        }
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
    let Ok(parsed) = parse_display(Some(display)) else {
        return false;
    };
    let local_hostname = local_hostname();
    for address in parsed.connect_instruction() {
        match address {
            ConnectAddress::Socket(_) => return true,
            ConnectAddress::Hostname(host, _) => {
                let host = host.to_ascii_lowercase();
                if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]") {
                    return true;
                }
                if local_hostname.as_deref().is_some_and(|local| {
                    host == local.to_ascii_lowercase()
                        || local
                            .split('.')
                            .next()
                            .is_some_and(|short| host == short.to_ascii_lowercase())
                }) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn session_is_x11_only() -> bool {
    let session = std::env::var("XDG_SESSION_TYPE").ok();
    let wayland = std::env::var_os("WAYLAND_DISPLAY");
    x11_only_session(
        session.as_deref(),
        wayland.as_deref().is_some_and(|value| !value.is_empty()),
    )
}

fn x11_only_session(session: Option<&str>, wayland_display: bool) -> bool {
    session.is_some_and(|kind| kind.eq_ignore_ascii_case("x11")) && !wayland_display
}

fn local_hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|hostname| hostname.trim().to_string())
        .filter(|hostname| !hostname.is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|hostname| !hostname.is_empty())
        })
}

fn client_machine_matches(claimed: &str, local: Option<&str>) -> bool {
    let claimed = claimed.trim();
    let Some(local) = local else {
        return false;
    };
    if claimed.eq_ignore_ascii_case(local) {
        return true;
    }
    let local_short = local.split('.').next().unwrap_or(local);
    // A short local hostname cannot authenticate an arbitrary FQDN claim:
    // `host.example` and `evil.host` both otherwise look plausible. If the
    // local name is an FQDN, the short form is still equivalent.
    local.contains('.') && local_short.eq_ignore_ascii_case(claimed)
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
        assert_eq!(
            window_eligibility(MapState::UNMAPPED, false, Some(WM_STATE_NORMAL)),
            WindowEligibility::Eligible
        );
        assert_eq!(
            window_eligibility(MapState::UNMAPPED, false, Some(WM_STATE_ICONIC)),
            WindowEligibility::Eligible
        );
        assert_eq!(
            window_eligibility(MapState::VIEWABLE, false, None),
            WindowEligibility::Eligible
        );
        assert_eq!(
            window_eligibility(MapState::VIEWABLE, false, Some(WM_STATE_WITHDRAWN)),
            WindowEligibility::Ineligible
        );
        assert_eq!(
            window_eligibility(MapState::UNMAPPED, false, None),
            WindowEligibility::Unknown
        );
        assert_eq!(
            window_eligibility(MapState::UNVIEWABLE, false, None),
            WindowEligibility::Unknown
        );
        assert_eq!(
            window_eligibility(MapState::VIEWABLE, true, None),
            WindowEligibility::Ineligible
        );
    }

    #[test]
    fn wm_state_requires_the_full_two_cardinal_property() {
        assert_eq!(parse_wm_state(&[WM_STATE_NORMAL, 0]), Some(WM_STATE_NORMAL));
        assert_eq!(
            parse_wm_state(&[WM_STATE_ICONIC, 42]),
            Some(WM_STATE_ICONIC)
        );
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
    fn x_resource_client_base_maps_a_window_to_its_local_pid() {
        let client_pids = ClientPids {
            clients: vec![res::Client {
                resource_base: 0x200000,
                resource_mask: 0x1f_ffff,
            }],
            by_client: HashMap::from([(0x200000, 4242)]),
        };
        assert_eq!(client_pid_for_window(0x200123, &client_pids), Some(4242));
        assert_eq!(client_pid_for_window(0x400123, &client_pids), None);
    }

    #[test]
    fn only_local_x11_displays_are_trusted() {
        assert!(local_display(":0"));
        assert!(local_display("unix/host:0"));
        assert!(local_display("localhost:10.0"));
        assert!(local_display("127.0.0.1:0"));
        assert!(local_display("[::1]:0"));
        assert!(!local_display("stale:0"));
        assert!(!local_display("remote.example:0"));
    }

    #[test]
    fn client_machine_accepts_short_or_fqdn_local_names() {
        assert!(client_machine_matches("HOST.example", Some("host.example")));
        assert!(client_machine_matches("host", Some("host.example")));
        assert!(!client_machine_matches("host.evil", Some("host")));
        assert!(!client_machine_matches("evil.host", Some("host")));
        assert!(!client_machine_matches("container", Some("host")));
        assert!(!client_machine_matches("elsewhere", None));
    }

    #[test]
    fn complete_coverage_requires_positive_x11_session_evidence() {
        assert!(x11_only_session(Some("x11"), false));
        assert!(!x11_only_session(Some("wayland"), false));
        assert!(!x11_only_session(None, false));
        assert!(!x11_only_session(Some("x11"), true));
    }
}
