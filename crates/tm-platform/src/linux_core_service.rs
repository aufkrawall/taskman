//! Wire protocol, protected manifest and authorization policy for the Linux
//! core service — the counterpart of `win::core_service`.
//!
//! The hardened service is a systemd system unit running as root. The GUI
//! stays an ordinary user process; when an operation needs more privilege than
//! the GUI's own token has, it asks this broker over a Unix socket.
//!
//! Everything in this module is platform-neutral on purpose: the framing, the
//! manifest contract and every authorization decision are unit-testable on the
//! Windows development host, exactly like `linux_telemetry`. The socket
//! transport itself lives in [`crate::linux::core_service`].
//!
//! ## Trust boundary
//!
//! A local Unix socket cannot use a named-pipe DACL, so the boundary is:
//!
//! 1. The socket lives in a root-owned directory and the socket file itself is
//!    root-owned and never group- or world-writable.
//! 2. The client is authenticated by the KERNEL-reported peer credentials of
//!    the connection (`SO_PEERCRED`), never by anything the client claims.
//! 3. The authorized uids come from a root-owned manifest that also pins the
//!    protocol version, so a GUI and a service from different releases fail at
//!    the handshake instead of misreading each other.
//! 4. Unless the manifest explicitly opts out for development, the peer's
//!    executable must be one of the installed client paths, root-owned and
//!    not writable by group or others. An authorized user could otherwise
//!    hand the broker's capabilities to any binary that user can write.
//!
//! uid 0 is always accepted: it already owns every privilege the service has,
//! so refusing it would only break the service's own diagnostics.

use std::io::{self, Read, Write};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::linux_sensors::SensorSnapshot;
use tm_core::model::PriorityClass;

/// Version of the request/response schema. Bumped whenever a field changes
/// meaning or a required field is added; the manifest pins it as well.
pub const PROTOCOL_VERSION: u32 = 1;

/// Root-owned runtime directory that systemd creates for the service.
pub const SOCKET_DIR: &str = "/run/taskman";
/// The one socket path the service listens on.
pub const SOCKET_PATH: &str = "/run/taskman/core.sock";
/// Root-owned manifest written by the installer / package scriptlet.
pub const MANIFEST_PATH: &str = "/etc/taskman/core-service.json";
/// Protected install root. The service must never run from a directory a
/// normal user can write (same invariant as the Windows service).
pub const INSTALL_ROOT: &str = "/usr/lib/taskman";
/// Unit name of the system service.
pub const SERVICE_UNIT: &str = "taskman-core.service";
/// Binary name installed below [`INSTALL_ROOT`].
pub const SERVICE_BINARY_NAME: &str = "taskman-service";

/// Largest request the broker accepts. Requests name a process or a unit;
/// 64 KiB is far beyond any legitimate one and bounds the read buffer.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Largest response. A sensor snapshot with every hwmon attribute of a large
/// workstation plus all GPUs stays well below this; a process evidence
/// response is a few KiB.
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Largest manifest file. The manifest is a dozen short fields.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// How long a client waits for one response before treating the broker as
/// unavailable. Sampling must never park on a wedged service.
pub const CLIENT_DEADLINE_MS: u64 = 3_000;

// ---------------------------------------------------------------- manifest

/// The protected manifest the service validates before it listens.
///
/// Only root may write it, so its contents are the installer's decision, not
/// the user's. Unknown fields are rejected so a typo cannot silently weaken a
/// policy (`deny_unknown_fields` is the point: a misspelled `clients` key must
/// fail the service, not authorize every binary).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Wire protocol this install speaks.
    pub protocol: u32,
    /// Local uids allowed to connect. The installer records the user that ran
    /// it; a machine-wide install with several desktop users lists each one.
    pub authorized_uids: Vec<u32>,
    /// Installed client executables (files, or directories ending in `/`).
    /// Checked against the peer's `/proc/<pid>/exe`.
    #[serde(default)]
    pub clients: Vec<String>,
    /// Development escape hatch: accept any peer binary of an authorized uid.
    /// Never written by the packaged installer.
    #[serde(default)]
    pub allow_user_writable_client: bool,
    /// Socket path. Pinned to [`SOCKET_PATH`] so client and service cannot
    /// disagree about where the broker listens.
    #[serde(default = "default_socket")]
    pub socket: String,
    /// Minimum age of the cached sensor snapshot the broker serves. A burst of
    /// GUI requests must not turn into a burst of `/sys` + NVML polling.
    #[serde(default = "default_sensor_cache_ms")]
    pub sensor_cache_ms: u64,
    /// Unix epoch seconds of the install, for diagnostics.
    #[serde(default)]
    pub installed_unix_s: u64,
    /// Free-form note from the installer ("pacman", "install.sh --user 1000").
    #[serde(default)]
    pub note: String,
}

fn default_socket() -> String {
    SOCKET_PATH.to_string()
}

fn default_sensor_cache_ms() -> u64 {
    500
}

/// Lowest and highest useful sensor cache window.
const SENSOR_CACHE_RANGE_MS: std::ops::RangeInclusive<u64> = 50..=60_000;

impl Default for Manifest {
    fn default() -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            authorized_uids: Vec::new(),
            clients: Vec::new(),
            allow_user_writable_client: false,
            socket: default_socket(),
            sensor_cache_ms: default_sensor_cache_ms(),
            installed_unix_s: 0,
            note: String::new(),
        }
    }
}

impl Manifest {
    /// Parse and validate manifest text.
    ///
    /// A returned manifest has passed every structural check, so a caller can
    /// use it as-is; a rejected manifest names the exact offending field.
    pub fn parse(text: &str) -> Result<Self, String> {
        let manifest: Self = serde_json::from_str(text)
            .map_err(|error| format!("manifest is not valid JSON for this schema: {error}"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Structural validation. Every problem is reported, so a broken install
    /// is fixed in one pass instead of one restart per typo.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems: Vec<String> = Vec::new();
        if self.protocol != PROTOCOL_VERSION {
            problems.push(format!(
                "protocol {} is not supported (this build speaks {PROTOCOL_VERSION})",
                self.protocol
            ));
        }
        if self.authorized_uids.is_empty() {
            problems.push("authorized_uids is empty: no client could ever connect".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for uid in &self.authorized_uids {
            if !seen.insert(*uid) {
                problems.push(format!("authorized_uids lists {uid} more than once"));
            }
        }
        if self.socket != SOCKET_PATH {
            problems.push(format!(
                "socket must be {SOCKET_PATH}, found {:?}",
                self.socket
            ));
        }
        if !SENSOR_CACHE_RANGE_MS.contains(&self.sensor_cache_ms) {
            problems.push(format!(
                "sensor_cache_ms {} is outside {}..={} ms",
                self.sensor_cache_ms,
                SENSOR_CACHE_RANGE_MS.start(),
                SENSOR_CACHE_RANGE_MS.end()
            ));
        }
        for client in &self.clients {
            if let Err(problem) = validate_client_path(client) {
                problems.push(problem);
            }
        }
        if !self.allow_user_writable_client && self.clients.is_empty() {
            problems.push(
                "clients is empty and allow_user_writable_client is false: no client could run an action"
                    .into(),
            );
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Whether `path` is an installed client path this manifest trusts.
    pub fn allows_client_path(&self, path: &str) -> bool {
        self.clients.iter().any(|entry| {
            if let Some(directory) = entry.strip_suffix('/') {
                path.strip_prefix(directory)
                    .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
            } else {
                entry == path
            }
        })
    }
}

/// Absolute, normalized paths only: `..`, relative segments and empty entries
/// would make the trust decision depend on the caller's working directory.
fn validate_client_path(client: &str) -> Result<(), String> {
    if client.is_empty() {
        return Err("clients contains an empty path".into());
    }
    if !client.starts_with('/') {
        return Err(format!("client path {client:?} is not absolute"));
    }
    if client.contains('/') && client.split('/').any(|segment| segment == "..") {
        return Err(format!("client path {client:?} contains a parent segment"));
    }
    if client.contains('\0') || client.contains('\n') {
        return Err("client path contains a control character".into());
    }
    Ok(())
}

/// Protection rule for the manifest file itself, as observed by the service.
///
/// The service runs as root, so an executable or user-writable manifest would
/// let an unprivileged user choose the authorized uids. A missing owner means
/// the filesystem reported nothing usable, which fails closed.
pub fn manifest_protection_problem(owner_uid: Option<u32>, mode: Option<u32>) -> Option<String> {
    match owner_uid {
        Some(0) => {}
        Some(uid) => return Some(format!("manifest must be owned by root, found uid {uid}")),
        None => return Some("manifest owner could not be read".into()),
    }
    let Some(mode) = mode else {
        return Some("manifest mode could not be read".into());
    };
    let writable = mode & 0o022;
    (writable != 0).then(|| {
        format!("manifest must not be group- or world-writable (mode {mode:o})")
    })
}

/// Protection rule for the service binary: a user-writable image must never
/// run as root.
pub fn service_image_protection_problem(owner_uid: Option<u32>, mode: Option<u32>) -> Option<String> {
    manifest_protection_problem(owner_uid, mode)
        .map(|problem| problem.replace("manifest", "service image"))
}

// ---------------------------------------------------------------- peers

/// Kernel-reported identity of one connected peer (`SO_PEERCRED`).
///
/// The pid is the kernel's, from the connecting socket, so it cannot be
/// spoofed by the client. Every decision below is made from this struct plus
/// the peer's own executable, which the service resolves itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

/// What the service observed about the peer's executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientImage {
    /// Resolved `/proc/<pid>/exe` target. Absolute.
    pub path: String,
    pub owner_uid: u32,
    /// Permission bits (`st_mode & 0o7777`).
    pub mode: u32,
}

/// Outcome of the peer policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerVerdict {
    Allowed,
    Refused(Refusal),
}

/// Why a request was refused. The same enum travels on the wire, so a GUI can
/// tell "the broker rejected this on purpose" (never fall back) from "the
/// broker does not know this operation" (also never fall back) and from an
/// ordinary unavailable-service case (handled by the transport layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Refusal {
    /// Peer uid is not in the manifest.
    UnauthorizedUser,
    /// Peer executable could not be resolved.
    MissingClientImage,
    /// Peer executable is not one of the manifest's installed clients.
    ClientNotListed,
    /// Peer executable is user-writable (or writable by group/others).
    UnprotectedClient,
    /// The manifest does not speak this protocol version.
    ProtocolMismatch,
    /// The request body did not parse as a request.
    MalformedRequest,
    /// The request is well-formed but outside the allowlist.
    Unsupported,
    /// The target process/unit does not exist (or changed identity).
    NotFound,
    /// The action was refused by the service's own checks.
    Rejected,
    /// The action failed; the detail carries the OS error.
    Failed,
    /// The bounded worker pool is full.
    Busy,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnauthorizedUser => "unauthorized user",
            Self::MissingClientImage => "client image unavailable",
            Self::ClientNotListed => "client not installed",
            Self::UnprotectedClient => "client image is writable",
            Self::ProtocolMismatch => "protocol mismatch",
            Self::MalformedRequest => "malformed request",
            Self::Unsupported => "unsupported operation",
            Self::NotFound => "target not found",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
            Self::Busy => "busy",
        }
    }
}

/// The whole peer policy, as one pure function.
///
/// `image` is `None` when `/proc/<pid>/exe` could not be resolved. uid 0 is
/// accepted without an image because it needs no grant.
pub fn evaluate_peer(
    manifest: &Manifest,
    peer: &PeerCredentials,
    image: Option<&ClientImage>,
) -> PeerVerdict {
    if peer.uid != 0 && !manifest.authorized_uids.contains(&peer.uid) {
        return PeerVerdict::Refused(Refusal::UnauthorizedUser);
    }
    if peer.uid == 0 || manifest.allow_user_writable_client {
        return PeerVerdict::Allowed;
    }
    let Some(image) = image else {
        return PeerVerdict::Refused(Refusal::MissingClientImage);
    };
    if !manifest.allows_client_path(&image.path) {
        return PeerVerdict::Refused(Refusal::ClientNotListed);
    }
    // A writable client image would let its owner put arbitrary code behind
    // the authorized path, so the trust check applies to the resolved file.
    if image.owner_uid != 0 || image.mode & 0o022 != 0 {
        return PeerVerdict::Refused(Refusal::UnprotectedClient);
    }
    PeerVerdict::Allowed
}

// ---------------------------------------------------------------- protocol

/// One client request.
///
/// `deny_unknown_fields` matters: a newer GUI sending an operation this
/// service does not implement must fail loudly at parse time instead of
/// silently getting a default answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// Liveness + capability report.
    Status,
    /// Root sensor snapshot (hwmon, RAPL, thermal zones, NVML).
    Sensors,
    /// Module map of one exact process.
    ProcessModules { pid: u32, start_ticks: i64 },
    /// Every thread of one exact process.
    ProcessThreads { pid: u32, start_ticks: i64 },
    /// Socket endpoints attributed to one process (`/proc/<pid>/fd` needs
    /// CAP_SYS_PTRACE for another user's process).
    ProcessConnections { pid: u32 },
    /// Container/hardening evidence the kernel withholds from other users.
    ProcessEvidence { pid: u32, start_ticks: i64 },
    /// One privileged mutation.
    Action { action: ActionRequest },
}

impl Request {
    /// Short label for rate-limited logging.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Sensors => "sensors",
            Self::ProcessModules { .. } => "process-modules",
            Self::ProcessThreads { .. } => "process-threads",
            Self::ProcessConnections { .. } => "process-connections",
            Self::ProcessEvidence { .. } => "process-evidence",
            Self::Action { action } => action.label(),
        }
    }

    /// Structural bounds, checked before any filesystem work.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Status | Self::Sensors => Ok(()),
            Self::ProcessModules { pid, .. }
            | Self::ProcessThreads { pid, .. }
            | Self::ProcessConnections { pid, .. }
            | Self::ProcessEvidence { pid, .. } => valid_pid(*pid),
            Self::Action { action } => action.validate(),
        }
    }
}

fn valid_pid(pid: u32) -> Result<(), String> {
    if pid == 0 {
        Err("pid 0 is the kernel, not a process".into())
    } else {
        Ok(())
    }
}

/// A privileged mutation. Every process action carries the creation identity
/// (`start_ticks`) the GUI sampled, so a recycled pid can never direct the
/// action at a different process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ActionRequest {
    Kill {
        pid: u32,
        start_ticks: i64,
        /// Kill the whole descendant tree, not only the root pid.
        tree: bool,
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
    Service {
        unit: String,
        verb: ServiceVerb,
    },
}

impl ActionRequest {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Kill { .. } => "kill",
            Self::Suspend { .. } => "suspend",
            Self::Resume { .. } => "resume",
            Self::SetPriority { .. } => "set-priority",
            Self::SetAffinity { .. } => "set-affinity",
            Self::Service { .. } => "service",
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Kill { pid, .. }
            | Self::Suspend { pid, .. }
            | Self::Resume { pid, .. }
            | Self::SetPriority { pid, .. }
            | Self::SetAffinity { pid, .. } => valid_pid(*pid),
            Self::SetAffinity { mask: 0, .. } => {
                Err("an empty affinity mask would stop the process on every CPU".into())
            }
            Self::Service { unit, .. } => {
                if unit.is_empty() {
                    return Err("service unit name is empty".into());
                }
                if unit.len() > 256 {
                    return Err("service unit name is longer than 256 bytes".into());
                }
                if unit.chars().any(|ch| ch.is_control()) {
                    return Err("service unit name contains a control character".into());
                }
                Ok(())
            }
        }
    }
}

/// systemd verb. A wire enum so the protocol does not depend on
/// `actions::ServiceAction`'s (Windows-gated) serde derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceVerb {
    Start,
    Stop,
    Restart,
}

/// One response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Response {
    Status(Box<ServiceStatusInfo>),
    Sensors(Box<SensorSnapshot>),
    Modules(Vec<WireModule>),
    Threads(WireThreads),
    Connections(Vec<WireConnection>),
    Evidence(Box<ProcessEvidence>),
    /// An action completed.
    Done,
    /// The service refused or failed. `code` is the machine-readable part.
    Error(ServiceFailure),
}

impl Response {
    pub fn error(code: Refusal, detail: impl Into<String>) -> Self {
        Self::Error(ServiceFailure {
            code,
            detail: detail.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceFailure {
    pub code: Refusal,
    pub detail: String,
}

/// Capability and liveness report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatusInfo {
    pub protocol: u32,
    pub service_version: String,
    pub pid: u32,
    pub uid: u32,
    pub started_unix_s: u64,
    pub uptime_s: u64,
    pub socket: String,
    pub authorized_uids: Vec<u32>,
    /// Whether the last sensor poll produced any reading.
    pub sensors_available: bool,
    /// Whether NVML (`libnvidia-ml.so.1`) was loaded.
    pub nvml_available: bool,
    /// Whether the protected install path was verified at startup.
    pub install_verified: bool,
    /// `last_sensor_sample_unix_ms` is 0 until the first poll.
    pub last_sensor_sample_unix_ms: i64,
    pub requests_served: u64,
    pub refusals: u64,
}

/// One mapped file of a process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireModule {
    pub name: String,
    pub path: String,
    pub base_address: u64,
    pub size_bytes: u64,
}

/// Thread list of one process, with its base priority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireThreads {
    pub process_base_priority: i32,
    pub threads: Vec<WireThread>,
}

/// Kernel scheduler state of one thread, mirroring `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireThreadState {
    Running,
    Sleeping,
    DiskSleep,
    Stopped,
    Tracing,
    Zombie,
    Dead,
    Idle,
    Parked,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireThread {
    pub tid: u32,
    pub priority: i32,
    /// Raw nice value; `None` where the kernel does not expose one.
    pub nice: Option<i32>,
    /// Scheduler switch count; `None` means unavailable, never a measured zero.
    pub context_switches: Option<u64>,
    pub kernel_time_100ns: u64,
    pub user_time_100ns: u64,
    pub created_epoch_s: Option<i64>,
    pub state: WireThreadState,
    /// Cumulative CPU cycles where the kernel reports them.
    pub cycles: Option<u64>,
}

/// One socket endpoint. Addresses are strings so the wire stays independent of
/// the platform's socket types; the client parses them back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireConnection {
    /// `tcp`, `tcp6`, `udp`, `udp6`, `unix`.
    pub protocol: String,
    pub local: String,
    /// `None` for listeners and datagram endpoints.
    pub remote: Option<String>,
    /// TCP state name (`listen`, `established`, ...); `None` for UDP/Unix.
    pub state: Option<String>,
}

/// Per-file I/O accounting of one process (`/proc/<pid>/io` is owner-only, so
/// another user's process needs the broker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireFileIo {
    pub files: Vec<WireFileIoEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireFileIoEntry {
    pub path: String,
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    pub read_ops: Option<u64>,
    pub write_ops: Option<u64>,
    pub file_size: Option<u64>,
    pub offset: Option<u64>,
    pub access_known: bool,
    pub is_read: bool,
    pub is_write: bool,
    pub open_handles: u32,
}

/// Root-only process evidence that `/proc` withholds from other users.
///
/// Every field is optional: the service reports what the kernel actually made
/// readable, and a missing value must render as unavailable — never as zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEvidence {
    pub pid: u32,
    pub start_ticks: i64,
    pub name: String,
    pub exe: Option<String>,
    pub cwd: Option<String>,
    pub root: Option<String>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub euid: Option<u32>,
    pub egid: Option<u32>,
    pub state: Option<String>,
    pub threads: Option<u32>,
    pub capability_effective: Option<u64>,
    pub capability_permitted: Option<u64>,
    pub capability_bounding: Option<u64>,
    pub seccomp_mode: Option<u32>,
    pub no_new_privs: Option<bool>,
    pub dumpable: Option<bool>,
    pub oom_score: Option<i32>,
    pub oom_score_adj: Option<i32>,
    pub rss_bytes: Option<u64>,
    pub pss_bytes: Option<u64>,
    pub swap_bytes: Option<u64>,
    pub open_fds: Option<u32>,
    pub namespaces: Vec<NamespaceId>,
    pub cgroup: Option<String>,
    pub security_label: Option<String>,
    pub limits: Vec<ResourceLimit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceId {
    /// `mnt`, `pid`, `net`, `user`, ...
    pub kind: String,
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimit {
    pub name: String,
    /// `u64::MAX` is `RLIM_INFINITY`.
    pub soft: u64,
    pub hard: u64,
}

/// Versioned request envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub protocol: u32,
    pub request: Request,
}

/// Versioned response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    pub protocol: u32,
    pub response: Response,
}

impl ResponseEnvelope {
    pub fn new(response: Response) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            response,
        }
    }
}

// ---------------------------------------------------------------- framing

/// A frame is a big-endian `u32` body length followed by that many JSON bytes.
///
/// A length prefix is what makes one request per connection cheap and lets a
/// client bound its read BEFORE allocating: the service never trusts a length
/// it has not checked against [`MAX_REQUEST_BYTES`], and neither does the GUI.
pub fn encode_frame<T: Serialize>(value: &T, limit: usize) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    if body.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {} bytes exceeds the {limit}-byte limit", body.len()),
        ));
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Read one frame body. `Ok(None)` is a clean end of stream before any byte of
/// a new frame (an ordinary client disconnect), never a truncated frame.
pub fn read_frame(stream: &mut impl Read, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    let mut filled = 0;
    while filled < length.len() {
        match stream.read(&mut length[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended inside a frame header",
                ));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame header claims {length} bytes, limit is {limit}"),
        ));
    }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Encode and write one frame.
pub fn write_frame<T: Serialize>(
    stream: &mut impl Write,
    value: &T,
    limit: usize,
) -> io::Result<()> {
    let frame = encode_frame(value, limit)?;
    stream.write_all(&frame)
}

/// Decode a frame body into `T`.
pub fn decode_frame<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    serde_json::from_slice(body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

/// Whether `path` is the protected service image the unit runs.
///
/// Kept here (not in the server) so the GUI-side status probe and the server
/// agree on the one path.
pub fn protected_service_path() -> String {
    format!("{INSTALL_ROOT}/{SERVICE_BINARY_NAME}")
}

/// Whether `path` is inside the protected install root.
pub fn is_inside_install_root(path: &Path) -> bool {
    path.starts_with(INSTALL_ROOT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            protocol: PROTOCOL_VERSION,
            authorized_uids: vec![1000],
            clients: vec!["/usr/bin/taskman".into()],
            allow_user_writable_client: false,
            socket: SOCKET_PATH.into(),
            sensor_cache_ms: 500,
            installed_unix_s: 0,
            note: "test".into(),
        }
    }

    fn peer(uid: u32) -> PeerCredentials {
        PeerCredentials {
            pid: 42,
            uid,
            gid: uid,
        }
    }

    fn protected_image() -> ClientImage {
        ClientImage {
            path: "/usr/bin/taskman".into(),
            owner_uid: 0,
            mode: 0o755,
        }
    }

    #[test]
    fn manifest_round_trips_and_rejects_unknown_fields() {
        let text = serde_json::to_string(&manifest()).expect("serialize");
        assert_eq!(Manifest::parse(&text).expect("parse"), manifest());

        let mut value: serde_json::Value = serde_json::from_str(&text).expect("json");
        value["clients_typo"] = serde_json::json!(["/usr/bin/anything"]);
        let error = Manifest::parse(&value.to_string()).expect_err("unknown field");
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn manifest_defaults_only_fill_optional_fields() {
        let text = r#"{"protocol":1,"authorized_uids":[1000],"clients":["/usr/bin/taskman"]}"#;
        let manifest = Manifest::parse(text).expect("parse");
        assert_eq!(manifest.socket, SOCKET_PATH);
        assert_eq!(manifest.sensor_cache_ms, 500);
        assert!(!manifest.allow_user_writable_client);
    }

    #[test]
    fn manifest_validation_names_every_problem() {
        let mut broken = manifest();
        broken.protocol = PROTOCOL_VERSION + 1;
        broken.authorized_uids = vec![1000, 1000];
        broken.socket = "/tmp/taskman.sock".into();
        broken.sensor_cache_ms = 1;
        broken.clients = vec!["relative/path".into(), "".into()];
        let error = broken.validate().expect_err("invalid");
        for needle in [
            "protocol",
            "more than once",
            "socket must be",
            "sensor_cache_ms",
            "not absolute",
            "empty path",
        ] {
            assert!(error.contains(needle), "missing {needle:?} in {error}");
        }
    }

    #[test]
    fn manifest_requires_at_least_one_authorized_uid() {
        let mut empty = manifest();
        empty.authorized_uids.clear();
        assert!(empty.validate().expect_err("invalid").contains("authorized_uids"));
    }

    #[test]
    fn strict_manifest_requires_a_client_path() {
        let mut strict = manifest();
        strict.clients.clear();
        assert!(strict.validate().expect_err("invalid").contains("clients is empty"));

        strict.allow_user_writable_client = true;
        assert!(strict.validate().is_ok());
    }

    #[test]
    fn client_directory_entries_match_by_prefix_only() {
        let mut manifest = manifest();
        manifest.clients = vec!["/usr/lib/taskman/".into()];
        assert!(manifest.allows_client_path("/usr/lib/taskman/taskman"));
        assert!(!manifest.allows_client_path("/usr/lib/taskman/../bin/taskman"));
        assert!(!manifest.allows_client_path("/usr/lib/taskman"));
        assert!(!manifest.allows_client_path("/usr/lib/taskmanx/taskman"));
    }

    #[test]
    fn manifest_file_must_be_root_owned_and_not_writable() {
        assert!(manifest_protection_problem(Some(0), Some(0o644)).is_none());
        assert!(manifest_protection_problem(Some(1000), Some(0o644)).is_some());
        assert!(manifest_protection_problem(Some(0), Some(0o664)).is_some());
        assert!(manifest_protection_problem(Some(0), Some(0o646)).is_some());
        assert!(manifest_protection_problem(None, Some(0o644)).is_some());
        assert!(manifest_protection_problem(Some(0), None).is_some());
    }

    #[test]
    fn authorized_uid_with_protected_client_is_allowed() {
        assert_eq!(
            evaluate_peer(&manifest(), &peer(1000), Some(&protected_image())),
            PeerVerdict::Allowed
        );
    }

    #[test]
    fn unauthorized_uid_is_refused_before_any_image_read() {
        assert_eq!(
            evaluate_peer(&manifest(), &peer(1001), None),
            PeerVerdict::Refused(Refusal::UnauthorizedUser)
        );
    }

    #[test]
    fn root_is_always_allowed() {
        assert_eq!(
            evaluate_peer(&manifest(), &peer(0), None),
            PeerVerdict::Allowed
        );
    }

    #[test]
    fn listed_client_is_still_refused_when_writable() {
        let mut writable = protected_image();
        writable.mode = 0o775;
        assert_eq!(
            evaluate_peer(&manifest(), &peer(1000), Some(&writable)),
            PeerVerdict::Refused(Refusal::UnprotectedClient)
        );

        let mut foreign = protected_image();
        foreign.owner_uid = 1000;
        assert_eq!(
            evaluate_peer(&manifest(), &peer(1000), Some(&foreign)),
            PeerVerdict::Refused(Refusal::UnprotectedClient)
        );
    }

    #[test]
    fn unlisted_client_is_refused_even_when_protected() {
        let mut other = protected_image();
        other.path = "/usr/bin/bash".into();
        assert_eq!(
            evaluate_peer(&manifest(), &peer(1000), Some(&other)),
            PeerVerdict::Refused(Refusal::ClientNotListed)
        );
    }

    #[test]
    fn missing_client_image_is_refused_in_strict_mode_only() {
        assert_eq!(
            evaluate_peer(&manifest(), &peer(1000), None),
            PeerVerdict::Refused(Refusal::MissingClientImage)
        );
        let mut dev = manifest();
        dev.allow_user_writable_client = true;
        assert_eq!(evaluate_peer(&dev, &peer(1000), None), PeerVerdict::Allowed);
    }

    #[test]
    fn request_rejects_the_kernel_pid_and_empty_affinity() {
        assert!(
            Request::ProcessThreads {
                pid: 0,
                start_ticks: 1
            }
            .validate()
            .is_err()
        );
        assert!(
            Request::Action {
                action: ActionRequest::SetAffinity {
                    pid: 10,
                    start_ticks: 1,
                    mask: 0
                }
            }
            .validate()
            .is_err()
        );
        assert!(
            Request::Action {
                action: ActionRequest::Service {
                    unit: "sshd.service".into(),
                    verb: ServiceVerb::Restart
                }
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn service_unit_name_rejects_control_characters() {
        for unit in ["", "bad\nunit", &"x".repeat(257)] {
            let request = Request::Action {
                action: ActionRequest::Service {
                    unit: unit.into(),
                    verb: ServiceVerb::Start,
                },
            };
            assert!(request.validate().is_err(), "{unit:?} was accepted");
        }
    }

    #[test]
    fn frames_round_trip_and_reject_oversize_bodies() {
        let request = RequestEnvelope {
            protocol: PROTOCOL_VERSION,
            request: Request::ProcessThreads {
                pid: 7,
                start_ticks: 91,
            },
        };
        let frame = encode_frame(&request, MAX_REQUEST_BYTES).expect("encode");
        let mut cursor = std::io::Cursor::new(frame.clone());
        let body = read_frame(&mut cursor, MAX_REQUEST_BYTES)
            .expect("read")
            .expect("body");
        let decoded: RequestEnvelope = decode_frame(&body).expect("decode");
        assert_eq!(decoded, request);

        // A clean EOF between frames is not an error.
        assert!(read_frame(&mut cursor, MAX_REQUEST_BYTES).expect("eof").is_none());

        // A header that claims more than the limit is refused before allocating.
        let mut oversized = std::io::Cursor::new(u32::MAX.to_be_bytes().to_vec());
        assert!(read_frame(&mut oversized, MAX_REQUEST_BYTES).is_err());

        // A truncated body is an error, never a short read presented as a frame.
        let mut truncated = std::io::Cursor::new(vec![0, 0, 0, 8, 1, 2]);
        assert!(read_frame(&mut truncated, MAX_REQUEST_BYTES).is_err());
    }

    #[test]
    fn encoding_refuses_a_body_over_the_limit() {
        let big = "x".repeat(MAX_REQUEST_BYTES);
        assert!(
            encode_frame(&Request::Action {
                action: ActionRequest::Service {
                    unit: big,
                    verb: ServiceVerb::Start
                }
            }, MAX_REQUEST_BYTES)
            .is_err()
        );
    }

    #[test]
    fn responses_round_trip_with_every_payload() {
        let responses = [
            Response::Done,
            Response::Error(ServiceFailure {
                code: Refusal::Rejected,
                detail: "identity changed".into(),
            }),
            Response::Modules(vec![WireModule {
                name: "libc.so.6".into(),
                path: "/usr/lib/libc.so.6".into(),
                base_address: 0x7f00_0000,
                size_bytes: 0x2000,
            }]),
            Response::Connections(vec![WireConnection {
                protocol: "tcp".into(),
                local: "127.0.0.1:8080".into(),
                remote: None,
                state: Some("listen".into()),
            }]),
            Response::Threads(WireThreads {
                process_base_priority: 0,
                threads: vec![WireThread {
                    tid: 5,
                    priority: 0,
                    nice: Some(0),
                    context_switches: Some(12),
                    kernel_time_100ns: 3,
                    user_time_100ns: 4,
                    created_epoch_s: None,
                    state: WireThreadState::Sleeping,
                    cycles: None,
                }],
            }),
        ];
        for response in responses {
            let encoded = encode_frame(&response, MAX_RESPONSE_BYTES).expect("encode");
            let body = &encoded[4..];
            let decoded: Response = decode_frame(body).expect("decode");
            assert_eq!(decoded, response);
        }
    }

    #[test]
    fn protected_paths_are_inside_the_install_root() {
        assert!(is_inside_install_root(Path::new(&protected_service_path())));
        assert!(!is_inside_install_root(Path::new(
            "/home/user/target/release/taskman-service"
        )));
    }
}
