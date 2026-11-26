//! Root-side broker: socket setup, peer authentication and request dispatch.
//!
//! Runs only inside `taskman-service` as a systemd system unit. The transport
//! contract is [`crate::linux_core_service`]; this module is the implementation
//! the service binary calls.
//!
//! ## Shape
//!
//! The calling thread is the accept thread: it owns the listening socket,
//! resolves each connection's kernel credentials, authorizes the peer, and only
//! then hands the connection to one of two bounded workers through a 16-slot
//! queue. Authorization before queueing matters: an authorized peer could
//! otherwise hold both worker slots by connecting and never sending. A full
//! queue is answered with `Refusal::Busy` instead of blocking accept, and one
//! request per connection keeps a stalled client from pinning a worker forever.
//!
//! ## Peer identity
//!
//! A path is not an identity. `unshare -m` plus a bind mount is unprivileged and
//! makes any executable appear at an installed path, so the broker never
//! compares paths: at startup it resolves every manifest client in ITS OWN mount
//! namespace and records the `(st_dev, st_ino)` of the protected file, and per
//! connection it opens `/proc/<pid>/exe` so the KERNEL resolves the peer's
//! executable and its metadata. The `start_ticks` bracket around that open
//! closes pid reuse in both directions. The resolved path survives in logs only.
//!
//! That snapshot would make a package upgrade look like an impostor, so a
//! `ClientNotListed` mismatch re-resolves the manifest's clients (once per
//! window, logged, same rules) instead of requiring a service restart.
//!
//! ## Refusals
//!
//! A refusal is written before the request is read, so the socket is half-closed
//! and drained afterwards: a client that is mid-frame must receive an answer,
//! not the EPIPE/ECONNRESET that the GUI is allowed to retry under its own
//! token.
//!
//! SIGTERM/SIGINT set an atomic flag and write one byte to a self-pipe; the
//! accept loop waits in `poll` on the listener and on that pipe, so a stop is
//! an event rather than a poll interval, and no listener is ever "woken" by
//! connecting to it (which would race the next accept).
//!
//! Nothing on a request path may panic, and nothing the kernel does not publish
//! is invented: an unreadable field stays `None`, never a fabricated zero.

use std::collections::HashSet;
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::actions::{ConnectionProtocol, ServiceAction, TcpState, ThreadState};
use crate::linux_core_service::{
    ACTION_DEADLINE_MS, ActionRequest, ClientImage, INSTALL_ROOT, InstalledClient, MANIFEST_PATH,
    MAX_MANIFEST_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Manifest, NamespaceId,
    PROTOCOL_VERSION, PeerCredentials, PeerVerdict, ProcessEvidence, Refusal, Request,
    RequestEnvelope, ResourceLimit, Response, ResponseEnvelope, SERVICE_UNIT, SOCKET_DIR,
    SOCKET_PATH, ServiceStatusInfo, ServiceVerb, WireConnection, WireFileIo, WireFileIoEntry,
    WireModule, WireThread, WireThreadState, WireThreads, decode_frame, encode_frame,
    evaluate_peer, is_inside_install_root, manifest_protection_problem, read_frame,
    service_image_protection_problem,
};
use crate::linux_sensors::SensorSnapshot;
use tm_core::error::TmError;
use tm_core::model::PriorityClass;

use super::super::{
    ensure_process_generation, process_connections, process_io, process_modules,
    process_start_for_read, process_threads, process_tree, send_signal_checked, sensors, services,
    set_affinity_direct, set_priority_direct,
};

/// Version this build reports; the workspace version, so a status answer names
/// the release the service binary came from.
const SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Workers and queue depth. Two workers cover one desktop's GUI and keep a slow
/// sysfs read from starving the second request; the queue bounds how many
/// clients can wait behind them.
const WORKER_COUNT: usize = 2;
const WORK_QUEUE_CAP: usize = 16;

/// How long one authenticated client may take to send its frame and to drain
/// the answer. Longer than the GUI's own `CLIENT_DEADLINE_MS` (3 s) so a slow
/// client fails against its own deadline instead of being cut off mid-frame,
/// and short enough that a wedged client cannot hold a worker for long.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A `Refusal::Busy` answer must not stall the accept thread, so the queue-full
/// path gets a much smaller write budget: the frame is a few hundred bytes into
/// an empty socket buffer.
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// How long a refusal drains what the client may still be writing.
///
/// A refusal is written before the request is read, so a client in the middle of
/// its frame sees EPIPE/ECONNRESET on a half-closed socket — which the GUI
/// classifies as an unavailable broker and retries under its own token. Reading
/// (and discarding) the rest of that frame keeps the refusal an answer.
///
/// This is ONE absolute deadline for the whole drain, not a timeout per read:
/// the drain runs on the accept thread, every local uid can reach it with an
/// `UnauthorizedUser` refusal, and a peer that trickles one byte per read must
/// not be able to park the root broker's control plane.
const REFUSAL_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

/// How often an identity mismatch may trigger a client-list re-resolve.
///
/// Trust is a startup snapshot, so a package upgrade would leave the running
/// broker trusting the old inode and refusing the new GUI until a restart. One
/// re-resolve per window is enough to survive an upgrade without letting a peer
/// turn every mismatch into a `/proc` walk.
const CLIENT_REFRESH_WINDOW_MS: u64 = 30_000;

/// Back-off after `accept` failed because this process ran out of descriptors
/// or memory. Waiting for a client to close is the only fix; it is not a broken
/// listener, so it must not count toward [`MAX_CONSECUTIVE_ACCEPT_FAILURES`].
const ACCEPT_RESOURCE_BACKOFF: Duration = Duration::from_millis(20);

/// How long the self-check waits for its one probe connection.
const SOCKET_PROBE_TIMEOUT_MS: i32 = 1_000;

/// A listener that cannot accept this many times in a row is broken (not busy:
/// a full queue does not fail `accept`), so the failure stops being invisible.
const MAX_CONSECUTIVE_ACCEPT_FAILURES: u32 = 16;

/// Bound on the clients resolved from the manifest at startup. An install lists
/// one to a handful; a directory entry could list more, and a request path must
/// never be able to grow this.
const MAX_INSTALLED_CLIENTS: usize = 32;

/// The socket is world-connectable on purpose: authorization is per peer
/// (kernel credentials plus a protected client image), not per file.
const SOCKET_MODE: u32 = 0o666;
/// The runtime directory only exists to hold that socket; systemd's
/// `RuntimeDirectory=` creates it the same way.
const SOCKET_DIRECTORY_MODE: u32 = 0o755;

/// Ceiling for one `/proc/<pid>` text file. `/proc/<pid>/smaps_rollup` and
/// `/proc/<pid>/status` are a few KiB; `/proc/<pid>/smaps` (never read here) is
/// the one that grows with the address space.
const MAX_PROC_TEXT_BYTES: u64 = 64 * 1024;
/// `/proc/<pid>/limits` has 16 rows. A hostile procfs replacement must not turn
/// one evidence request into an unbounded list.
const MAX_LIMIT_ROWS: usize = 64;
/// The kernel publishes at most ~15 namespace entries per process.
const MAX_NAMESPACE_ENTRIES: usize = 32;
/// A file descriptor table larger than this is not counted: a truncated count
/// would be a fabricated number, so the answer is "unknown" instead.
const MAX_OPEN_FDS: u32 = 1_000_000;

/// One log line per refusal kind per window, and the same rule for the
/// accept-path warnings: a client in a loop must not become a log flood.
const REFUSAL_LOG_WINDOW_MS: u64 = 5_000;
const BROKER_WARNING_WINDOW_MS: u64 = 5_000;

// ------------------------------------------------------------ public surface

/// Run the broker until the process is asked to stop.
///
/// Fails only when the service cannot start at all (no manifest, unprotected
/// install, socket that cannot be bound). A per-connection error never stops
/// the broker; the accept loop reports and keeps serving.
///
/// `dev` (the `--dev` argv entry, which the packaged unit never passes)
/// disables the install-root and service-image checks and logs a warning, and it
/// skips manifest client resolution the same way a manifest with
/// `allow_user_writable_client` does — otherwise the escape hatch could not start
/// on the build tree it exists for. Both still require the root-owned manifest,
/// because the authorized uids have to come from somewhere the user cannot write,
/// and neither grants anything a normal start would: an unresolved client list is
/// only satisfiable by uid 0 or by that explicit manifest exemption. Passing
/// `--dev` means being root already, so it cannot widen anyone's privileges.
pub fn run_service(dev: bool) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    if uid != 0 {
        return Err(format!(
            "the Linux core service must run as root (euid 0), this process has uid {uid}"
        ));
    }
    if dev {
        tracing::warn!(
            manifest = MANIFEST_PATH,
            "running the Linux core service in dev mode: the install-root and service-image \
             checks are disabled; the root-owned manifest is still required"
        );
    }

    let manifest = load_manifest()?;
    // Dev mode and the manifest's own escape hatch must be able to start on a
    // machine where no client resolves (a build tree, a portable copy), but they
    // still get no grant a normal start would give: an empty list can only be
    // satisfied by uid 0 or by `allow_user_writable_client`.
    let clients_resolved = !(dev || manifest.allow_user_writable_client);
    let installed = if clients_resolved {
        resolve_installed_clients(&manifest)?
    } else {
        let reason = if dev {
            "dev mode"
        } else {
            "allow_user_writable_client"
        };
        tracing::warn!(
            reason,
            "skipping manifest client resolution: only uid 0 and clients this manifest \
             explicitly exempts can be authorized"
        );
        Vec::new()
    };
    let (service_image, install_verified) = verify_service_image(dev)?;
    let listener = prepare_listener()?;

    let state = Arc::new(ServiceState {
        manifest,
        installed: RwLock::new(installed),
        clients_resolved,
        client_refresh_ms: AtomicU64::new(0),
        started: Instant::now(),
        started_unix_s: unix_now_s(),
        install_verified,
        service_image,
        sensor_poll: Mutex::new(SensorCache::default()),
        sensor_sample_unix_ms: AtomicI64::new(0),
        sensor_available: AtomicBool::new(false),
        units_in_flight: Mutex::new(HashSet::new()),
        requests_served: AtomicU64::new(0),
        refusals: AtomicU64::new(0),
    });

    let (work_tx, work_rx) = std::sync::mpsc::sync_channel::<ClientWork>(WORK_QUEUE_CAP);
    let work_rx = Arc::new(Mutex::new(work_rx));
    let mut workers = Vec::with_capacity(WORKER_COUNT);
    for index in 0..WORKER_COUNT {
        let receiver = Arc::clone(&work_rx);
        let state = Arc::clone(&state);
        let worker = std::thread::Builder::new()
            .name(format!("tm-broker-{index}"))
            .spawn(move || run_worker(&receiver, &state))
            .map_err(|error| format!("could not start broker worker {index}: {error}"))?;
        workers.push(worker);
    }

    // Installed after the workers exist: a stop racing startup finds a broker
    // that can already finish its in-flight request and exit.
    let wake_fd = install_stop_signal()?;
    tracing::info!(
        socket = SOCKET_PATH,
        protocol = PROTOCOL_VERSION,
        authorized_uids = state.manifest.authorized_uids.len(),
        installed_clients = tm_core::sync::read(&state.installed).len(),
        image = %state.service_image,
        install_verified,
        "linux core service listening"
    );

    let result = accept_loop(&listener, wake_fd, &state, &work_tx);
    // Dropping the only sender ends both workers' `recv`, so a clean stop does
    // not abandon a privileged thread mid-flight.
    drop(work_tx);
    for worker in workers {
        let _ = worker.join();
    }
    if result.is_ok() {
        tracing::info!("linux core service stopped");
    }
    result
}

/// Headless smoke test: validate the manifest and the install paths, inspect
/// the socket and open one connection to it, then print a JSON report.
///
/// Never installs, starts, stops or restarts a unit. The report goes to stdout
/// in both cases; the `Err` variant still carries that JSON so an operator sees
/// every check, and marks only a real failure of this installation (manifest,
/// service image, or a socket that exists in a dangerous state). A missing
/// socket is reported but is not a failure: the unit may simply be stopped.
pub fn selfcheck() -> Result<String, String> {
    let manifest = load_manifest();
    let install = verify_service_image(false);
    let socket = inspect_socket(Path::new(SOCKET_PATH));
    let connection = match socket.state {
        SocketState::Present => probe_connection(Path::new(SOCKET_PATH)),
        SocketState::Missing | SocketState::Unsafe => "skipped",
    };
    let snapshot = sensors::snapshot();
    let nvml_available = sensors::nvml_available();

    let manifest_json = match &manifest {
        Ok(manifest) => serde_json::json!({
            "path": MANIFEST_PATH,
            "valid": true,
            "error": serde_json::Value::Null,
            "protocol": manifest.protocol,
            "authorized_uids": manifest.authorized_uids.len(),
            "clients": manifest.clients.len(),
            "allow_user_writable_client": manifest.allow_user_writable_client,
            "sensor_cache_ms": manifest.sensor_cache_ms,
        }),
        Err(problem) => serde_json::json!({
            "path": MANIFEST_PATH,
            "valid": false,
            "error": problem,
        }),
    };
    let install_json = match &install {
        Ok((image, verified)) => serde_json::json!({
            "root": INSTALL_ROOT,
            "image": image,
            "verified": verified,
            "error": serde_json::Value::Null,
        }),
        Err(problem) => serde_json::json!({
            "root": INSTALL_ROOT,
            "image": serde_json::Value::Null,
            "verified": false,
            "error": problem,
        }),
    };

    let mut failures: Vec<String> = Vec::new();
    if let Err(problem) = &manifest {
        failures.push(format!("manifest: {problem}"));
    }
    if let Err(problem) = &install {
        failures.push(format!("install: {problem}"));
    }
    if socket.state == SocketState::Unsafe {
        failures.push(format!("socket: {}", socket.detail));
    }

    let ok = failures.is_empty();
    let report = serde_json::json!({
        "ok": ok,
        "protocol": PROTOCOL_VERSION,
        "service_version": SERVICE_VERSION,
        "pid": std::process::id(),
        // SAFETY: getuid/geteuid have no preconditions and cannot fail.
        "uid": unsafe { libc::getuid() },
        // SAFETY: as above.
        "euid": unsafe { libc::geteuid() },
        "manifest": manifest_json,
        "install": install_json,
        "socket": {
            "path": SOCKET_PATH,
            "state": socket.state.name(),
            "detail": socket.detail,
            "connection": connection,
        },
        // Counts only: a self-check must not become a second way to read sensor
        // values out of a root process.
        "sensors": {
            "readings": snapshot.readings.len(),
            "gpus": snapshot.gpus.len(),
            "unreadable": snapshot.unreadable.len(),
            "nvml_available": nvml_available,
        },
        "failures": &failures,
    });
    let text = report.to_string();
    if ok { Ok(text) } else { Err(text) }
}

// -------------------------------------------------------------------- state

/// Everything the workers share. Immutable after startup except the published
/// sensor state, the in-flight units and the counters.
struct ServiceState {
    manifest: Manifest,
    /// The protected clients resolved from the manifest in THIS process's mount
    /// namespace. Peer trust is decided on their `(st_dev, st_ino)`, so an
    /// upgrade replaces this list instead of restarting the broker.
    installed: RwLock<Vec<InstalledClient>>,
    /// Whether `installed` was actually resolved. Dev mode and
    /// `allow_user_writable_client` skip resolution on purpose, and the upgrade
    /// refresh must not quietly re-introduce it there.
    clients_resolved: bool,
    /// Monotonic ms of the last client-list re-resolve attempt.
    client_refresh_ms: AtomicU64,
    started: Instant,
    started_unix_s: u64,
    /// Only ever true when the full install verification ran and passed; dev
    /// mode reports `false` rather than claiming a check it skipped.
    install_verified: bool,
    service_image: String,
    /// Serializes sensor polls only. A `Status` answer never takes it.
    sensor_poll: Mutex<SensorCache>,
    /// Published after every poll so `Status` cannot block behind sysfs + NVML.
    sensor_sample_unix_ms: AtomicI64,
    sensor_available: AtomicBool,
    /// Unit names with a verb in flight, so one unit's action can never be
    /// started twice by two clients.
    units_in_flight: Mutex<HashSet<String>>,
    requests_served: AtomicU64,
    refusals: AtomicU64,
}

/// The one polled sensor snapshot, so a burst of GUI requests is one sysfs +
/// NVML poll and not one poll per request.
#[derive(Default)]
struct SensorCache {
    last: Option<(Instant, SensorSnapshot)>,
}

/// One unit's in-flight claim; dropping it releases the unit.
struct UnitFlight<'a> {
    in_flight: &'a Mutex<HashSet<String>>,
    unit: String,
}

impl Drop for UnitFlight<'_> {
    fn drop(&mut self) {
        tm_core::sync::lock(self.in_flight).remove(&self.unit);
    }
}

impl ServiceState {
    fn count_request(&self) {
        self.requests_served.fetch_add(1, Ordering::Relaxed);
    }

    fn count_refusal(&self) {
        self.refusals.fetch_add(1, Ordering::Relaxed);
    }

    /// Serve the cached snapshot unless it is older than the manifest's window.
    ///
    /// The poll runs while the poll lock is held on purpose: a queued request
    /// waits for the poll in flight instead of starting a second one, which is
    /// exactly the "a burst is not a burst of polling" contract. `Status` does
    /// not take this lock, so it never waits behind a slow `/sys` walk.
    fn sensor_snapshot(&self) -> SensorSnapshot {
        let window = Duration::from_millis(self.manifest.sensor_cache_ms);
        let mut cache = tm_core::sync::lock(&self.sensor_poll);
        if let Some((taken, snapshot)) = cache.last.as_ref()
            && taken.elapsed() < window
        {
            return snapshot.clone();
        }
        let snapshot = sensors::snapshot();
        cache.last = Some((Instant::now(), snapshot.clone()));
        drop(cache);
        // Publish outside the lock: this is the only state `Status` reads.
        self.sensor_sample_unix_ms
            .store(snapshot.taken_at_unix_ms, Ordering::Relaxed);
        self.sensor_available
            .store(!snapshot.is_empty(), Ordering::Relaxed);
        snapshot
    }

    /// Last poll's availability and timestamp, read without any lock: a status
    /// request must never be the reason sysfs is read, nor wait for it.
    fn last_sensor_poll(&self) -> (bool, i64) {
        (
            self.sensor_available.load(Ordering::Relaxed),
            self.sensor_sample_unix_ms.load(Ordering::Relaxed),
        )
    }

    /// Claim one unit for one verb, or `None` when a verb is already running.
    fn begin_unit_flight(&self, unit: &str) -> Option<UnitFlight<'_>> {
        let mut in_flight = tm_core::sync::lock(&self.units_in_flight);
        if !in_flight.insert(unit.to_string()) {
            return None;
        }
        drop(in_flight);
        Some(UnitFlight {
            in_flight: &self.units_in_flight,
            unit: unit.to_string(),
        })
    }

    /// Re-resolve the manifest's clients after an identity mismatch.
    ///
    /// Trust is a startup snapshot, so a package upgrade would otherwise leave
    /// this process refusing the newly installed GUI until someone restarts the
    /// unit. Re-applying the same rule (root-owned, not group/world writable,
    /// inside a protected directory chain) is exactly what that restart would
    /// do, and the window keeps a peer from turning every mismatch into a
    /// `/proc` walk. Returns whether the recorded identity actually changed.
    fn refresh_installed_clients(&self) -> bool {
        if !self.clients_resolved {
            return false;
        }
        if !claim_log_window(
            &self.client_refresh_ms,
            monotonic_ms(),
            CLIENT_REFRESH_WINDOW_MS,
        ) {
            return false;
        }
        let clients = match resolve_installed_clients(&self.manifest) {
            Ok(clients) => clients,
            Err(problem) => {
                tracing::warn!(
                    %problem,
                    "client identity mismatch, and the manifest's clients could not be re-resolved"
                );
                return false;
            }
        };
        let changed = *tm_core::sync::read(&self.installed) != clients;
        if changed {
            tracing::warn!(
                clients = clients.len(),
                "installed client identity changed since startup: the manifest's clients were \
                 re-resolved"
            );
            *tm_core::sync::write(&self.installed) = clients;
        }
        changed
    }

    fn status(&self) -> ServiceStatusInfo {
        let (sensors_available, last_sensor_sample_unix_ms) = self.last_sensor_poll();
        ServiceStatusInfo {
            protocol: PROTOCOL_VERSION,
            service_version: SERVICE_VERSION.to_string(),
            pid: std::process::id(),
            // The broker's real uid, not the peer's: this describes the service
            // that answered.
            // SAFETY: getuid has no preconditions and cannot fail.
            uid: unsafe { libc::getuid() },
            started_unix_s: self.started_unix_s,
            uptime_s: self.started.elapsed().as_secs(),
            socket: SOCKET_PATH.to_string(),
            authorized_uids: self.manifest.authorized_uids.clone(),
            sensors_available,
            nvml_available: sensors::nvml_available(),
            install_verified: self.install_verified,
            last_sensor_sample_unix_ms,
            requests_served: self.requests_served.load(Ordering::Relaxed),
            refusals: self.refusals.load(Ordering::Relaxed),
        }
    }
}

fn unix_now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

// ----------------------------------------------------------------- manifest

/// Read and validate the protected manifest.
///
/// Two passes on purpose: `symlink_metadata` names the exact problem (a symlink,
/// a directory, an oversized file, a foreign owner) before anything is read, and
/// the descriptor opened afterwards with `O_NOFOLLOW` re-checks owner, mode and
/// size on the object actually being read, so a swap between the two cannot get
/// a different file's text parsed as the policy.
fn load_manifest() -> Result<Manifest, String> {
    let path = Path::new(MANIFEST_PATH);
    let metadata = std::fs::symlink_metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => {
            format!("{MANIFEST_PATH} is missing: this installation is incomplete")
        }
        _ => format!("{MANIFEST_PATH} could not be inspected: {error}"),
    })?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{MANIFEST_PATH} must not be a symlink"));
    }
    if !metadata.is_file() {
        return Err(format!("{MANIFEST_PATH} is not a regular file"));
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "{MANIFEST_PATH} is {} bytes, larger than the {MAX_MANIFEST_BYTES}-byte limit",
            metadata.len()
        ));
    }
    if let Some(problem) =
        manifest_protection_problem(Some(metadata.uid()), Some(metadata.mode() & 0o7777))
    {
        return Err(format!("{problem} ({MANIFEST_PATH})"));
    }
    if let Some(problem) = protected_directory_chain_problem(path) {
        return Err(format!("{MANIFEST_PATH}: {problem}"));
    }
    let text = read_protected_text(path, "manifest")?;
    Manifest::parse(&text).map_err(|problem| format!("{MANIFEST_PATH}: {problem}"))
}

/// Read a root-owned file that must not be swapped underneath the check.
fn read_protected_text(path: &Path, label: &str) -> Result<String, String> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{} contains a NUL byte", path.display()))?;
    // SAFETY: `c_path` is a valid NUL-terminated path and `open` only reads the
    // flags; it returns a fresh descriptor that nothing else owns.
    let descriptor = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(format!(
            "{} could not be opened: {}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    // SAFETY: `open` returned a fresh descriptor nothing else owns, so taking
    // ownership here is what releases it exactly once.
    let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    let metadata = file
        .metadata()
        .map_err(|error| format!("{} could not be inspected: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "{} is {} bytes, larger than the {MAX_MANIFEST_BYTES}-byte limit",
            path.display(),
            metadata.len()
        ));
    }
    if let Some(problem) =
        manifest_protection_problem(Some(metadata.uid()), Some(metadata.mode() & 0o7777))
    {
        return Err(format!("{problem} ({}, {label})", path.display()));
    }
    let mut bytes = Vec::with_capacity(4 * 1024);
    // `take` bounds the read even if the file grew after the metadata check.
    (&file)
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!(
            "{} grew past the {MAX_MANIFEST_BYTES}-byte limit while it was read",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))
}

/// Every directory above `path` must be root-owned, a real directory, and not
/// writable by group or others: a writable parent lets an ordinary user replace
/// the file the service trusts, which no per-file check can detect.
fn protected_directory_chain_problem(path: &Path) -> Option<String> {
    let mut current = path.parent();
    while let Some(directory) = current {
        match std::fs::symlink_metadata(directory) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Some(format!("{} must not be a symlink", directory.display()));
                }
                if !metadata.is_dir() {
                    return Some(format!("{} is not a directory", directory.display()));
                }
                if let Some(problem) = protected_directory_problem(
                    &directory.display().to_string(),
                    Some(metadata.uid()),
                    Some(metadata.mode() & 0o7777),
                ) {
                    return Some(problem);
                }
            }
            Err(error) => {
                return Some(format!(
                    "{} could not be inspected: {error}",
                    directory.display()
                ));
            }
        }
        current = directory.parent();
    }
    None
}

/// Shared owner/mode rule for a directory the service trusts.
fn protected_directory_problem(
    label: &str,
    owner_uid: Option<u32>,
    mode: Option<u32>,
) -> Option<String> {
    match owner_uid {
        Some(0) => {}
        Some(uid) => return Some(format!("{label} must be owned by root, found uid {uid}")),
        None => return Some(format!("{label} owner could not be read")),
    }
    let Some(mode) = mode else {
        return Some(format!("{label} mode could not be read"));
    };
    let writable = mode & 0o022;
    (writable != 0).then(|| format!("{label} must not be group- or world-writable (mode {mode:o})"))
}

/// `None` when `directory` and every ancestor is a real directory that root
/// owns and nobody but root can write.
///
/// Shared with the service binary, which writes its logs into one of these
/// trees: a writable ancestor lets an ordinary user replace whatever the
/// service later opens there, and no per-file check can detect that.
pub fn protected_directory_chain(directory: &Path) -> Option<String> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Some(format!("{} must not be a symlink", directory.display()));
            }
            if !metadata.is_dir() {
                return Some(format!("{} is not a directory", directory.display()));
            }
            if let Some(problem) = protected_directory_problem(
                &directory.display().to_string(),
                Some(metadata.uid()),
                Some(metadata.mode() & 0o7777),
            ) {
                return Some(problem);
            }
        }
        Err(error) => {
            return Some(format!(
                "{} could not be inspected: {error}",
                directory.display()
            ));
        }
    }
    protected_directory_chain_problem(directory)
}

/// Resolve every manifest client ONCE, in this process's mount namespace.
///
/// This is the whole point of the identity model: the recorded `(st_dev,
/// st_ino)` is the protected installed file as root sees it, so a peer that
/// arranges for another executable to appear at an installed path (a private
/// mount namespace plus a bind mount is unprivileged) reports a different
/// inode and is not trusted. A missing entry is skipped with a warning — an
/// install may list a client that a package does not ship today — but a
/// present entry the service cannot prove safe is a broken install, and an
/// install where nothing resolves could never authorize anyone.
fn resolve_installed_clients(manifest: &Manifest) -> Result<Vec<InstalledClient>, String> {
    let mut installed = Vec::new();
    // Explicit files first. A named client must never be dropped in favour of
    // whatever a directory entry happens to contain, or it would later be
    // refused as `ClientNotListed` with nothing in the log to explain it.
    for entry in manifest
        .clients
        .iter()
        .filter(|entry| !entry.ends_with('/'))
    {
        if installed.len() >= MAX_INSTALLED_CLIENTS {
            tracing::warn!(
                client = %entry,
                limit = MAX_INSTALLED_CLIENTS,
                "manifest client list is truncated: this listed client is NOT trusted"
            );
            continue;
        }
        if let Some(client) = resolve_client_file(entry, Path::new(entry))? {
            installed.push(client);
        }
    }
    for entry in manifest.clients.iter().filter(|entry| entry.ends_with('/')) {
        if installed.len() >= MAX_INSTALLED_CLIENTS {
            tracing::warn!(
                client = %entry,
                limit = MAX_INSTALLED_CLIENTS,
                "manifest client list is truncated: this client directory is not trusted"
            );
            continue;
        }
        // `filter` above guarantees the suffix, so this cannot loop on a `None`.
        if let Some(directory) = entry.strip_suffix('/') {
            resolve_client_directory(entry, Path::new(directory), &mut installed)?;
        }
    }
    if installed.is_empty() {
        return Err(format!(
            "no manifest client resolved to a protected installed executable (clients: {})",
            manifest.clients.join(", ")
        ));
    }
    Ok(installed)
}

/// One listed client path. `Ok(None)` means the entry is simply not installed.
fn resolve_client_file(entry: &str, path: &Path) -> Result<Option<InstalledClient>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(client = entry, "manifest client is not installed; skipped");
            return Ok(None);
        }
        Err(error) => {
            return Err(format!(
                "manifest client {entry} could not be inspected: {error}"
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "manifest client {entry} must be a regular file, not a symlink or a special file"
        ));
    }
    if let Some(problem) = protected_directory_problem(
        &format!("manifest client {entry}"),
        Some(metadata.uid()),
        Some(metadata.mode() & 0o7777),
    ) {
        return Err(problem);
    }
    // The same chain rule the manifest, the service image and the log directory
    // get: a writable parent lets its owner replace the file the identity was
    // recorded from, which would make the recorded inode a lie. The rule is
    // about the chain ABOVE the file: passing the file itself made it report
    // "is not a directory" and refused startup on a real install.
    if let Some(problem) = protected_directory_chain_problem(path) {
        return Err(format!("manifest client {entry}: {problem}"));
    }
    Ok(Some(InstalledClient {
        path: entry.to_string(),
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

/// A directory entry in `clients` (`/usr/lib/taskman/`) trusts every protected
/// executable installed directly inside it. The trust is still the identity
/// recorded here, not the path a peer reports.
fn resolve_client_directory(
    entry: &str,
    directory: &Path,
    installed: &mut Vec<InstalledClient>,
) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(
                client = entry,
                "manifest client directory is not installed; skipped"
            );
            return Ok(());
        }
        Err(error) => {
            return Err(format!(
                "manifest client directory {entry} could not be inspected: {error}"
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "manifest client directory {entry} must be a real directory"
        ));
    }
    // A writable directory would let its owner plant a file the service then
    // trusts, which is the one thing an installed directory must prevent.
    if let Some(problem) = protected_directory_problem(
        &format!("manifest client directory {entry}"),
        Some(metadata.uid()),
        Some(metadata.mode() & 0o7777),
    ) {
        return Err(problem);
    }
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("manifest client directory {entry} could not be read: {error}"))?;
    for file in entries.flatten() {
        if installed.len() >= MAX_INSTALLED_CLIENTS {
            tracing::warn!(
                client = entry,
                limit = MAX_INSTALLED_CLIENTS,
                "manifest client directory lists more files than the broker tracks"
            );
            break;
        }
        let path = file.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        if let Some(problem) = protected_directory_problem(
            &format!("manifest client {}", path.display()),
            Some(metadata.uid()),
            Some(metadata.mode() & 0o7777),
        ) {
            // A directory entry lists a tree, not one file: an unprotected file
            // in it is simply not a client, and refusing the whole service for
            // one stray helper would be worse than not trusting it.
            tracing::warn!(client = %path.display(), %problem, "manifest client is not trusted");
            continue;
        }
        if let Some(problem) = protected_directory_chain_problem(&path) {
            tracing::warn!(client = %path.display(), %problem, "manifest client is not trusted");
            continue;
        }
        installed.push(InstalledClient {
            path: path.to_string_lossy().into_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    Ok(())
}

/// Verify the running image, i.e. that this process is the protected binary.
///
/// The check runs after `exec`, so it proves the image and every directory above
/// it are root-owned and unwritable; the immutable-install argument is that
/// nobody but root could have written either.
fn verify_service_image(dev: bool) -> Result<(String, bool), String> {
    let image = std::fs::read_link("/proc/self/exe")
        .map_err(|error| format!("/proc/self/exe could not be resolved: {error}"))?;
    let image = image.to_string_lossy().into_owned();
    if dev {
        tracing::warn!(
            image = %image,
            "dev mode skips the protected install path and service image ownership checks"
        );
        return Ok((image, false));
    }
    if !is_inside_install_root(Path::new(&image)) {
        return Err(format!(
            "the service image {image} is not inside the protected install root {INSTALL_ROOT}"
        ));
    }
    let metadata = std::fs::metadata(&image)
        .map_err(|error| format!("the service image {image} could not be inspected: {error}"))?;
    if let Some(problem) =
        service_image_protection_problem(Some(metadata.uid()), Some(metadata.mode() & 0o7777))
    {
        return Err(format!("{problem} ({image})"));
    }
    if let Some(problem) = protected_directory_chain_problem(Path::new(&image)) {
        return Err(format!(
            "the service image is not in a protected directory tree: {problem}"
        ));
    }
    Ok((image, true))
}

// ------------------------------------------------------------------- socket

/// Create or verify the runtime directory, then bind the socket.
fn prepare_listener() -> Result<UnixListener, String> {
    let directory = Path::new(SOCKET_DIR);
    prepare_socket_directory(directory)?;
    remove_stale_socket(Path::new(SOCKET_PATH))?;
    let listener = UnixListener::bind(SOCKET_PATH)
        .map_err(|error| format!("could not bind {SOCKET_PATH}: {error}"))?;
    std::fs::set_permissions(SOCKET_PATH, std::fs::Permissions::from_mode(SOCKET_MODE))
        .map_err(|error| format!("could not set the mode of {SOCKET_PATH}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("could not make {SOCKET_PATH} non-blocking: {error}"))?;
    // std creates a socket with `SOCK_CLOEXEC`, but no privileged descriptor may
    // survive an exec, so it is also asserted here where it is visible.
    set_close_on_exec(listener.as_raw_fd())
        .map_err(|error| format!("could not mark {SOCKET_PATH} close-on-exec: {error}"))?;
    Ok(listener)
}

fn prepare_socket_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => verify_socket_directory(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir(path)
                .map_err(|error| format!("could not create {}: {error}", path.display()))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_DIRECTORY_MODE))
                .map_err(|error| {
                    format!("could not set the mode of {}: {error}", path.display())
                })?;
            // Verify what is actually there now: `create_dir` would have
            // inherited the umask, and the mode above only holds if nobody
            // raced the name.
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
            verify_socket_directory(path, &metadata)
        }
        Err(error) => Err(format!("could not inspect {}: {error}", path.display())),
    }
}

fn verify_socket_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() {
        return Err(format!("{} must not be a symlink", path.display()));
    }
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    protected_directory_problem(
        &format!("the socket directory {}", path.display()),
        Some(metadata.uid()),
        Some(metadata.mode() & 0o7777),
    )
    .map_or(Ok(()), Err)
}

/// An existing path may only be removed when lstat proves it is a socket owned
/// by root. Unlinking anything else would let this service delete a file an
/// ordinary user pointed at the socket path.
fn remove_stale_socket(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not inspect {}: {error}", path.display())),
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(format!(
                    "refusing to unlink {}: it is not a socket",
                    path.display()
                ));
            }
            if metadata.uid() != 0 {
                return Err(format!(
                    "refusing to unlink {}: it is owned by uid {}",
                    path.display(),
                    metadata.uid()
                ));
            }
            std::fs::remove_file(path)
                .map_err(|error| format!("could not unlink {}: {error}", path.display()))
        }
    }
}

fn set_close_on_exec(descriptor: RawFd) -> io::Result<()> {
    // SAFETY: `fcntl` only reads/writes the descriptor's own flags, and the
    // descriptor is owned by the caller for the duration of the call.
    let descriptor_flags = unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) };
    if descriptor_flags < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCredentials> {
    // SAFETY: `ucred` is plain old data, the length is its own size, and
    // `getsockopt` writes at most that many bytes into it.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the pointer and the length describe `credentials`, and the stream
    // owns a live descriptor for the duration of the call.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast::<libc::c_void>(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let pid = u32::try_from(credentials.pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "peer pid is negative"))?;
    Ok(PeerCredentials {
        pid,
        uid: credentials.uid,
        gid: credentials.gid,
    })
}

/// Resolve the peer's executable as a kernel object, not as a path string.
///
/// `open("/proc/<pid>/exe")` makes the KERNEL resolve the task's own executable,
/// so a private mount namespace cannot substitute anything: the returned
/// `(st_dev, st_ino)` is the file that task actually runs. The path is kept for
/// logging only.
///
/// The creation identity (`start_ticks`) is read before the open and again
/// after it. Without that bracket an authorized user could exit and let pid
/// reuse hand the connection's pid to the installed GUI, or the reverse: a
/// grant earned by one process generation must never authorize another.
fn resolve_peer_image(pid: u32) -> Result<ClientImage, Refusal> {
    if pid == 0 {
        return Err(Refusal::MissingClientImage);
    }
    let before = super::super::read_proc_start_ticks(pid).map_err(|_| Refusal::NotFound)?;
    // No `O_NOFOLLOW`: `/proc/<pid>/exe` is a magic link whose target IS the
    // executable, and refusing to follow it would refuse every peer.
    let executable = open_proc_executable(pid).map_err(|_| Refusal::MissingClientImage)?;
    let metadata = executable
        .metadata()
        .map_err(|_| Refusal::MissingClientImage)?;
    if !metadata.is_file() {
        return Err(Refusal::MissingClientImage);
    }
    let path = std::fs::read_link(format!("/proc/{pid}/exe"))
        .map(|target| target.to_string_lossy().into_owned())
        .unwrap_or_default();
    let after = super::super::read_proc_start_ticks(pid).map_err(|_| Refusal::NotFound)?;
    if before != after {
        return Err(Refusal::NotFound);
    }
    Ok(ClientImage {
        path,
        device: metadata.dev(),
        inode: metadata.ino(),
        owner_uid: metadata.uid(),
        mode: metadata.mode() & 0o7777,
    })
}

/// Open the executable the kernel reports for `pid`, close-on-exec.
fn open_proc_executable(pid: u32) -> io::Result<std::fs::File> {
    let path = CString::new(format!("/proc/{pid}/exe"))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid contains a NUL byte"))?;
    // SAFETY: `path` is a valid NUL-terminated string and `open` only reads the
    // flags; it returns a fresh descriptor that nothing else owns.
    let descriptor = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is fresh and unowned, so taking ownership here
    // releases it exactly once.
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
}

// ------------------------------------------------------------------ signals

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Write end of the self-pipe, only ever written by [`on_stop_signal`]; `-1`
/// until the pipe exists, so a signal before startup cannot write fd 0.
static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_stop_signal(_signal: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Release);
    let descriptor = WAKE_WRITE_FD.load(Ordering::Acquire);
    if descriptor >= 0 {
        let byte = 1u8;
        // SAFETY: `write` is async-signal-safe, the pipe is non-blocking, and
        // its write end is never closed while the process lives, so this cannot
        // block or touch memory the handler does not own.
        unsafe { libc::write(descriptor, (&raw const byte).cast::<libc::c_void>(), 1) };
    }
}

/// Install the stop handlers and return the wake pipe's read end.
///
/// The handler writes to the self-pipe, and that write is what actually wakes
/// the accept loop's `poll`; the atomic flag is what the loop then reads.
///
/// `sigaction` with explicit zero flags means NO `SA_RESTART`, so a blocking
/// syscall in the accept thread returns `EINTR` instead of being resumed. That
/// does NOT bound a worker: `read_frame` retries `Interrupted`, so a worker's
/// socket read is bounded by `REQUEST_TIMEOUT`, not by the signal.
fn install_stop_signal() -> Result<RawFd, String> {
    let mut descriptors: [libc::c_int; 2] = [0; 2];
    // SAFETY: `descriptors` is the two-element array `pipe2` requires.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(format!(
            "could not create the signal wake pipe: {}",
            io::Error::last_os_error()
        ));
    }
    WAKE_WRITE_FD.store(descriptors[1], Ordering::Release);
    // SAFETY: `sigaction` is plain old data; an all-zero value is the empty
    // handler table, which the two fields below fill in.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    // Casting a function item through `*const ()` keeps the address rustc's
    // `function_casts_as_integer` lint objects to a single, explicit step.
    action.sa_sigaction = on_stop_signal as *const () as libc::sighandler_t;
    action.sa_flags = 0;
    // SAFETY: `sa_mask` is the all-zero set, i.e. valid storage for the empty
    // set `sigemptyset` writes.
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
        return Err(format!(
            "could not initialize the signal set: {}",
            io::Error::last_os_error()
        ));
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: `action` is a fully initialized handler table and a null old
        // table is allowed.
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(format!(
                "could not install the signal handler: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(descriptors[0])
}

// --------------------------------------------------------------- accept loop

struct ClientWork {
    stream: UnixStream,
    peer: PeerCredentials,
}

/// Wait eventfully for either a connection or a stop, and hand accepted
/// connections to the worker pool without ever blocking on it.
fn accept_loop(
    listener: &UnixListener,
    wake_fd: RawFd,
    state: &ServiceState,
    work_tx: &SyncSender<ClientWork>,
) -> Result<(), String> {
    let listener_fd = listener.as_raw_fd();
    let mut consecutive_failures = 0u32;
    while !STOP_REQUESTED.load(Ordering::Acquire) {
        let mut polled = [
            libc::pollfd {
                fd: listener_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // No timeout: the only ways out are a ready listener and a stop, and a
        // stop always arrives through the pipe. A timeout here would be a poll
        // interval dressed up as an event.
        // SAFETY: the array and its length match, and both descriptors stay
        // owned by this function's caller for the whole loop.
        let ready = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll on the listening socket failed: {error}"));
        }
        if polled[1].revents != 0 {
            // Either a stop was requested or the wake pipe broke; both mean the
            // broker is finished.
            break;
        }
        if polled[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
            continue;
        }
        // The listener is non-blocking, so a connection withdrawn between poll
        // and accept costs one loop iteration instead of a parked thread.
        match listener.accept() {
            Ok((stream, _address)) => {
                consecutive_failures = 0;
                enqueue(stream, state, work_tx)?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_resource_exhaustion(&error) => {
                // Out of descriptors or memory: the listener is fine, `accept`
                // will work again once a client closes. Back-pressure, not a
                // fault, and definitely not a reason to stop the root broker.
                if broker_warning_allowed() {
                    tracing::warn!(%error, "accept is out of resources; waiting for one to free up");
                }
                wait_for_stop(wake_fd, ACCEPT_RESOURCE_BACKOFF);
            }
            Err(error) => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_ACCEPT_FAILURES {
                    return Err(format!(
                        "the listening socket refused {consecutive_failures} accepts in a row: \
                         {error}"
                    ));
                }
                if broker_warning_allowed() {
                    tracing::warn!(%error, "accept on the broker socket failed");
                }
            }
        }
    }
    Ok(())
}

/// Descriptor or memory pressure, not a broken listener.
fn is_resource_exhaustion(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

/// Wait up to `duration`, but return immediately when a stop wakes the pipe.
///
/// A plain sleep here would make a stop wait for the whole back-off; polling the
/// wake pipe keeps the shutdown event-driven.
fn wait_for_stop(wake_fd: RawFd, duration: Duration) {
    let mut polled = [libc::pollfd {
        fd: wake_fd,
        events: libc::POLLIN,
        revents: 0,
    }];
    let timeout = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: the array and its length match, and `wake_fd` stays owned by the
    // caller for the whole loop.
    unsafe { libc::poll(polled.as_mut_ptr(), 1, timeout) };
}

/// Authenticate the peer, then queue the connection.
///
/// Authorization happens HERE, before the connection owns a worker slot: an
/// authorized peer could otherwise hold both workers for the request deadline
/// by connecting and never sending anything, repeatedly. Refusals go out on the
/// short write budget and are drained, so a refused client sees an answer
/// rather than a vanished broker.
fn enqueue(
    stream: UnixStream,
    state: &ServiceState,
    work_tx: &SyncSender<ClientWork>,
) -> Result<(), String> {
    let peer = match peer_credentials(&stream) {
        Ok(peer) => peer,
        Err(error) => {
            // Without kernel credentials there is nothing to authorize, so the
            // connection is dropped rather than served under a guess.
            if broker_warning_allowed() {
                tracing::warn!(%error, "could not read a client's peer credentials");
            }
            return Ok(());
        }
    };
    if let Err(error) = set_close_on_exec(stream.as_raw_fd())
        && broker_warning_allowed()
    {
        tracing::warn!(%error, "could not mark an accepted connection close-on-exec");
    }
    // uid 0 needs no grant, so an image it cannot be resolved for (a task that
    // just exec'd away, a kernel thread) must not lock the superuser out; for
    // everyone else an unresolvable image is the refusal itself.
    let image = match resolve_peer_image(peer.pid) {
        Ok(image) => Some(image),
        Err(_) if peer.uid == 0 => None,
        Err(refusal) => {
            let detail = auth_refusal_detail(refusal, &peer, None);
            refuse(state, stream, refusal, detail);
            return Ok(());
        }
    };
    // The lock is released before the refusal path runs: a refresh takes the
    // write side and must not deadlock behind this read.
    let verdict = {
        let installed = tm_core::sync::read(&state.installed);
        evaluate_peer(&state.manifest, &installed, &peer, image.as_ref())
    };
    // An identity mismatch is what a package upgrade looks like from here: the
    // installed GUI now has a new inode while this process still trusts the old
    // one. Re-resolve once per window and let a match through, exactly as a
    // restart would; anything else stays the same refusal.
    let verdict = match verdict {
        PeerVerdict::Refused(Refusal::ClientNotListed) if state.refresh_installed_clients() => {
            let installed = tm_core::sync::read(&state.installed);
            evaluate_peer(&state.manifest, &installed, &peer, image.as_ref())
        }
        other => other,
    };
    if let PeerVerdict::Refused(refusal) = verdict {
        let detail = auth_refusal_detail(refusal, &peer, image.as_ref());
        refuse(state, stream, refusal, detail);
        return Ok(());
    }
    match work_tx.try_send(ClientWork { stream, peer }) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(work)) => {
            // Back-pressure, not a fault: answer instead of blocking accept, so
            // one slow client cannot stop the socket from being drained.
            if broker_warning_allowed() {
                tracing::warn!("the broker's request queue is full; answering busy");
            }
            refuse(
                state,
                work.stream,
                Refusal::Busy,
                "the broker's request queue is full".to_string(),
            );
            Ok(())
        }
        // Both workers are gone; the pool cannot be rebuilt from here.
        Err(TrySendError::Disconnected(_)) => Err("the broker worker pool stopped".into()),
    }
}

fn run_worker(receiver: &Mutex<Receiver<ClientWork>>, state: &ServiceState) {
    loop {
        // The guard is a temporary of this statement, so the lock is released
        // before the request is handled: the workers, not the queue, are the
        // concurrency limit.
        let next = tm_core::sync::lock(receiver).recv();
        match next {
            Ok(work) => handle_client(work, state),
            Err(_) => break,
        }
    }
}

// ------------------------------------------------------------- one connection

/// Serve exactly one already-authorized request and drop the connection.
///
/// Peer resolution and the authorization decision happened in [`enqueue`], so a
/// worker only ever carries traffic that has already been granted.
fn handle_client(work: ClientWork, state: &ServiceState) {
    let ClientWork { mut stream, peer } = work;
    // Bounded before the first write (a refusal path writes before it reads) so
    // no client that never drains its socket can park a worker.
    if let Err(error) = stream.set_write_timeout(Some(REQUEST_TIMEOUT)) {
        if broker_warning_allowed() {
            tracing::warn!(%error, "could not set a broker write deadline");
        }
        return;
    }
    if let Err(error) = stream.set_read_timeout(Some(REQUEST_TIMEOUT)) {
        if broker_warning_allowed() {
            tracing::warn!(%error, "could not set a broker read deadline");
        }
        return;
    }

    let body = match read_frame(&mut stream, MAX_REQUEST_BYTES) {
        Ok(Some(body)) => body,
        // A client that connects and closes without a request is ordinary: the
        // self-check's connection probe does exactly this.
        Ok(None) => return,
        Err(error) => {
            if error.kind() == io::ErrorKind::InvalidData {
                // The frame was refused before allocating, and the client is
                // still there to receive the reason.
                let detail = format!("request frame refused: {error}");
                send_refusal(state, &mut stream, Refusal::MalformedRequest, detail);
            } else if broker_warning_allowed() {
                tracing::warn!(%error, peer_uid = peer.uid, "could not read a broker request");
            }
            return;
        }
    };

    let envelope: RequestEnvelope = match decode_frame(&body) {
        Ok(envelope) => envelope,
        Err(error) => {
            let detail = format!("request body is not a request: {error}");
            send_refusal(state, &mut stream, Refusal::MalformedRequest, detail);
            return;
        }
    };
    // A version mismatch is answered before anything else is interpreted: a
    // field this build would misread must never reach dispatch.
    if envelope.protocol != PROTOCOL_VERSION {
        let detail = format!(
            "this broker speaks protocol {PROTOCOL_VERSION}, the client sent {}",
            envelope.protocol
        );
        send_refusal(state, &mut stream, Refusal::ProtocolMismatch, detail);
        return;
    }
    if let Err(problem) = envelope.request.validate() {
        send_refusal(state, &mut stream, Refusal::MalformedRequest, problem);
        return;
    }

    let label = envelope.request.label();
    let is_action = matches!(&envelope.request, Request::Action { .. });
    state.count_request();
    let response = dispatch(state, peer.pid, envelope.request);
    tracing::debug!(
        peer_uid = peer.uid,
        request = label,
        "broker request served"
    );
    // An action can legitimately run for `ACTION_DEADLINE_MS` (a `systemctl
    // restart` waits on systemd's job), so its answer gets that budget instead
    // of the read deadline: a client that is still waiting must not be cut off
    // by a write timeout, and it must not see a half-written frame either.
    let write_budget = if is_action {
        Duration::from_millis(ACTION_DEADLINE_MS)
    } else {
        REQUEST_TIMEOUT
    };
    if let Err(error) = stream.set_write_timeout(Some(write_budget)) {
        if broker_warning_allowed() {
            tracing::warn!(%error, "could not set a broker write deadline");
        }
        return;
    }
    send_response(state, &mut stream, response);
}

/// Answer one refusal, count it, and make sure the client sees an answer.
fn send_refusal(state: &ServiceState, stream: &mut UnixStream, code: Refusal, detail: String) {
    log_refusal(code, &detail);
    send_response(state, stream, Response::error(code, detail));
    finish_refusal(stream);
}

/// Answer a connection this process refuses to serve and close it.
fn refuse(state: &ServiceState, stream: UnixStream, code: Refusal, detail: String) {
    let mut stream = stream;
    // The refusal is written before any request is read, so it must not be
    // allowed to park the accept thread on a client that never drains.
    let _ = stream.set_write_timeout(Some(BUSY_TIMEOUT));
    send_refusal(state, &mut stream, code, detail);
}

/// Half-close after a refusal and discard what the client is still writing.
///
/// A refusal is written before the request is read. Closing straight away makes
/// a client that is mid-frame see EPIPE/ECONNRESET, which is transport
/// failure — the one class the GUI is allowed to retry elsewhere. Reading the
/// remainder (bounded, with a deadline) turns the refusal into an answer the
/// client can classify as a decision.
fn finish_refusal(stream: &mut UnixStream) {
    let _ = stream.shutdown(Shutdown::Write);
    // ONE absolute deadline for the whole drain. A per-read timeout would let a
    // peer that trickles one byte per read hold this — and this runs on the
    // accept thread, for any local uid, so that would park the control plane.
    let deadline = Instant::now() + REFUSAL_DRAIN_TIMEOUT;
    let mut discarded = 0usize;
    let mut buffer = [0u8; 4096];
    while discarded < MAX_REQUEST_BYTES {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let Some(remaining) = deadline.checked_duration_since(now) else {
            break;
        };
        if stream.set_read_timeout(Some(remaining)).is_err() {
            return;
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => discarded = discarded.saturating_add(read),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

/// Encode and write exactly one response, replacing an oversize payload with a
/// failure frame instead of letting it look like a hung broker.
fn send_response(state: &ServiceState, stream: &mut UnixStream, response: Response) {
    if matches!(response, Response::Error(_)) {
        state.count_refusal();
    }
    let frame = match encode_frame(&ResponseEnvelope::new(response), MAX_RESPONSE_BYTES) {
        Ok(frame) => frame,
        Err(error) => {
            tracing::error!(%error, "broker response exceeded the frame limit");
            let fallback = ResponseEnvelope::new(Response::error(
                Refusal::Failed,
                "the response exceeded the frame limit",
            ));
            match encode_frame(&fallback, MAX_RESPONSE_BYTES) {
                Ok(frame) => frame,
                // Cannot happen for a fixed short body; writing nothing is
                // still better than writing a truncated frame.
                Err(_) => return,
            }
        }
    };
    let _ = stream.write_all(&frame);
    let _ = stream.flush();
}

fn auth_refusal_detail(
    code: Refusal,
    peer: &PeerCredentials,
    image: Option<&ClientImage>,
) -> String {
    let mut detail = format!("{}: uid {} pid {}", code.as_str(), peer.uid, peer.pid);
    if let Some(image) = image {
        // Diagnostic only. Trust was decided on (st_dev, st_ino); a path can be
        // anything the peer's namespace makes it look like.
        detail.push_str(" image ");
        detail.push_str(&image.path);
    }
    detail
}

// ------------------------------------------------------------------ dispatch

fn dispatch(state: &ServiceState, peer_pid: u32, request: Request) -> Response {
    match request {
        Request::Status => Response::Status(Box::new(state.status())),
        Request::Sensors => Response::Sensors(Box::new(state.sensor_snapshot())),
        Request::ProcessModules { pid, start_ticks } => modules_response(pid, start_ticks),
        Request::ProcessThreads { pid, start_ticks } => threads_response(pid, start_ticks),
        Request::ProcessConnections { pid, start_ticks } => connections_response(pid, start_ticks),
        Request::ProcessFileIo { pid, start_ticks } => file_io_response(pid, start_ticks),
        Request::ProcessEvidence { pid, start_ticks } => match collect_evidence(pid, start_ticks) {
            Ok(evidence) => Response::Evidence(Box::new(evidence)),
            Err(error) => error_response(error),
        },
        Request::Action { action } => run_action(state, peer_pid, action),
    }
}

fn modules_response(pid: u32, start_ticks: i64) -> Response {
    // Same bracket as the direct action: the generation is read before and
    // after, so a recycled pid can never be described as the sampled process.
    let before = match process_start_for_read(pid, Some(start_ticks)) {
        Ok(before) => before,
        Err(error) => return error_response(error),
    };
    let modules = match process_modules::list_process_modules(pid) {
        Ok(modules) => modules,
        Err(error) => return error_response(error),
    };
    if let Err(error) = ensure_process_generation(pid, before) {
        return error_response(error);
    }
    Response::Modules(
        modules
            .into_iter()
            .map(|module| WireModule {
                name: module.name,
                path: module.path,
                base_address: module.base_address,
                size_bytes: module.size_bytes,
            })
            .collect(),
    )
}

fn threads_response(pid: u32, start_ticks: i64) -> Response {
    let before = match process_start_for_read(pid, Some(start_ticks)) {
        Ok(before) => before,
        Err(error) => return error_response(error),
    };
    let threads = match process_threads::list_process_threads(pid, Some(before)) {
        Ok(threads) => threads,
        Err(error) => return error_response(error),
    };
    if let Err(error) = ensure_process_generation(pid, before) {
        return error_response(error);
    }
    Response::Threads(WireThreads {
        process_base_priority: threads.process_base_priority,
        threads: threads
            .threads
            .into_iter()
            .map(|thread| WireThread {
                tid: thread.tid,
                priority: thread.priority,
                nice: thread.nice,
                context_switches: thread.context_switches,
                kernel_time_100ns: thread.kernel_time_100ns,
                user_time_100ns: thread.user_time_100ns,
                created_epoch_s: thread.created_epoch_s,
                state: wire_thread_state(thread.state),
                // Procfs publishes neither an entry routine nor cumulative CPU
                // cycles, so the wire keeps both unavailable.
                cycles: thread.cycles,
            })
            .collect(),
    })
}

fn connections_response(pid: u32, start_ticks: i64) -> Response {
    // `/proc/<pid>/fd` and the network tables are several reads, so the sampled
    // generation is proven before the walk and again after it: a recycled pid
    // must not answer with another process's endpoints.
    let before = match process_start_for_read(pid, Some(start_ticks)) {
        Ok(before) => before,
        Err(error) => return error_response(error),
    };
    let connections = match process_connections::list_process_connections(pid) {
        Ok(connections) => connections,
        Err(error) => return error_response(error),
    };
    if let Err(error) = ensure_process_generation(pid, before) {
        return error_response(error);
    }
    Response::Connections(
        connections
            .into_iter()
            .map(|connection| WireConnection {
                protocol: connection_protocol_name(connection.protocol).to_string(),
                local: connection.local.to_string(),
                remote: connection.remote.map(|remote| remote.to_string()),
                state: connection.state.map(tcp_state_name),
            })
            .collect(),
    )
}

/// Per-file I/O of one exact process.
///
/// The reader brackets the two procfs walks with the same creation identity, so
/// a partially old handle list and a partially new counter read can never be
/// presented as one coherent answer.
fn file_io_response(pid: u32, start_ticks: i64) -> Response {
    match process_io::list_process_file_io(pid, Some(start_ticks)) {
        Ok(read) => Response::FileIo(Box::new(WireFileIo {
            files: read
                .snapshot
                .files
                .into_iter()
                .map(|file| WireFileIoEntry {
                    path: file.path,
                    read_bytes: file.read_bytes,
                    write_bytes: file.write_bytes,
                    read_ops: file.read_ops,
                    write_ops: file.write_ops,
                    file_size: file.file_size,
                    offset: file.offset,
                    access_known: file.access_known,
                    is_read: file.is_read,
                    is_write: file.is_write,
                    open_handles: file.open_handles,
                })
                .collect(),
        })),
        Err(error) => error_response(error),
    }
}

fn wire_thread_state(state: ThreadState) -> WireThreadState {
    match state {
        // Linux `TASK_RUNNING` covers on-CPU and runnable-in-queue alike, which
        // is what procfs `R` means and what the wire calls `running`.
        ThreadState::Running | ThreadState::Runnable => WireThreadState::Running,
        ThreadState::Sleeping => WireThreadState::Sleeping,
        ThreadState::Waiting => WireThreadState::DiskSleep,
        ThreadState::Stopped => WireThreadState::Stopped,
        ThreadState::Terminated => WireThreadState::Zombie,
        ThreadState::Idle => WireThreadState::Idle,
        ThreadState::Parked => WireThreadState::Parked,
        // The raw `/proc` character is carried, not collapsed: the direct reader
        // shows the same evidence, and the client can render the byte it names.
        ThreadState::Other(raw) => WireThreadState::Other(u8::try_from(raw).unwrap_or(u8::MAX)),
        // Windows-only scheduler states have no procfs meaning; they cannot be
        // produced by this backend and carry no byte to report.
        ThreadState::Initialized
        | ThreadState::Ready
        | ThreadState::Standby
        | ThreadState::Transition
        | ThreadState::DeferredReady => WireThreadState::Other(u8::MAX),
    }
}

fn connection_protocol_name(protocol: ConnectionProtocol) -> &'static str {
    match protocol {
        ConnectionProtocol::Tcp => "tcp",
        ConnectionProtocol::Tcp6 => "tcp6",
        ConnectionProtocol::Udp => "udp",
        ConnectionProtocol::Udp6 => "udp6",
    }
}

/// TCP state as a stable kebab-case name, matching the wire's own naming
/// convention so the client can map it back without a lookup table.
fn tcp_state_name(state: TcpState) -> String {
    match state {
        TcpState::Closed => "closed".into(),
        TcpState::Listen => "listen".into(),
        TcpState::SynSent => "syn-sent".into(),
        TcpState::SynReceived => "syn-received".into(),
        TcpState::Established => "established".into(),
        TcpState::FinWait1 => "fin-wait1".into(),
        TcpState::FinWait2 => "fin-wait2".into(),
        TcpState::CloseWait => "close-wait".into(),
        TcpState::Closing => "closing".into(),
        TcpState::LastAck => "last-ack".into(),
        TcpState::TimeWait => "time-wait".into(),
        TcpState::DeleteTcb => "delete-tcb".into(),
        TcpState::Other(raw) => format!("other-{raw}"),
    }
}

// ------------------------------------------------------------------- actions

fn run_action(state: &ServiceState, peer_pid: u32, action: ActionRequest) -> Response {
    match action {
        ActionRequest::Kill {
            pid,
            start_ticks,
            tree,
        } => {
            if let Some(refusal) = mutation_target_refusal(pid, peer_pid) {
                return refusal;
            }
            match process_tree::kill_process_with_delivery(pid, Some(start_ticks), tree).0 {
                Ok(()) => Response::Done,
                Err(error) => error_response(error),
            }
        }
        ActionRequest::Suspend { pid, start_ticks } => {
            if let Some(refusal) = mutation_target_refusal(pid, peer_pid) {
                return refusal;
            }
            match send_signal_checked(pid, Some(start_ticks), libc::SIGSTOP) {
                Ok(()) => Response::Done,
                Err(error) => error_response(error),
            }
        }
        ActionRequest::Resume { pid, start_ticks } => {
            if let Some(refusal) = mutation_target_refusal(pid, peer_pid) {
                return refusal;
            }
            match send_signal_checked(pid, Some(start_ticks), libc::SIGCONT) {
                Ok(()) => Response::Done,
                Err(error) => error_response(error),
            }
        }
        ActionRequest::SetPriority {
            pid,
            start_ticks,
            priority,
        } => {
            if let Some(refusal) = mutation_target_refusal(pid, peer_pid) {
                return refusal;
            }
            // `Unknown` is not a target. Saying so here keeps it an explicit
            // policy refusal instead of an OS error text.
            if priority == PriorityClass::Unknown {
                return Response::error(
                    Refusal::Rejected,
                    "an explicit priority class is required",
                );
            }
            match set_priority_direct(pid, Some(start_ticks), priority) {
                Ok(()) => Response::Done,
                Err(error) => error_response(error),
            }
        }
        ActionRequest::SetAffinity {
            pid,
            start_ticks,
            mask,
        } => {
            if let Some(refusal) = mutation_target_refusal(pid, peer_pid) {
                return refusal;
            }
            if mask == 0 {
                return Response::error(
                    Refusal::Rejected,
                    "an empty affinity mask would stop the process on every CPU",
                );
            }
            match set_affinity_direct(pid, Some(start_ticks), mask) {
                Ok(()) => Response::Done,
                Err(error) => error_response(error),
            }
        }
        ActionRequest::Service { unit, verb } => {
            // The same rule pkexec uses, so a unit name can never become a
            // systemctl option or reach outside `*.service`.
            if let Err(problem) = crate::linux_action_support::validate_service_unit(&unit) {
                return Response::error(Refusal::Rejected, problem);
            }
            // systemd accepts an escaped backslash, so `foo\x2dbar.service` and
            // `foo-bar.service` name ONE unit. The in-flight claim is keyed by
            // the string as sent, so admitting both would let two mutations of
            // the same unit run at once. A name needing an escape is refused
            // rather than normalized: no packaged unit has one.
            if unit.contains('\\') {
                return Response::error(
                    Refusal::Rejected,
                    "systemd escape sequences in a unit name are not accepted",
                );
            }
            if unit == SERVICE_UNIT {
                return Response::error(
                    Refusal::Rejected,
                    "the broker does not control its own unit",
                );
            }
            // One verb per unit at a time. A second request for the same unit
            // answers `Busy` without starting anything, so a client that gave up
            // on a slow verb and retried cannot mutate the unit twice.
            let Some(_flight) = state.begin_unit_flight(&unit) else {
                return Response::error(
                    Refusal::Busy,
                    format!("a verb for {unit} is already in flight"),
                );
            };
            let action = match verb {
                ServiceVerb::Start => ServiceAction::Start,
                ServiceVerb::Stop => ServiceAction::Stop,
                ServiceVerb::Restart => ServiceAction::Restart,
            };
            match services::control_unit_direct(&unit, action) {
                Ok(()) => Response::Done,
                Err(error) => error_response(error),
            }
        }
    }
}

/// Refusals that apply to every process mutation, before any syscall.
fn mutation_target_refusal(pid: u32, peer_pid: u32) -> Option<Response> {
    if pid <= 1 {
        return Some(Response::error(
            Refusal::Rejected,
            format!("pid {pid} is the init process"),
        ));
    }
    if pid == std::process::id() {
        return Some(Response::error(
            Refusal::Rejected,
            "the broker does not target itself",
        ));
    }
    if pid == peer_pid {
        return Some(Response::error(
            Refusal::Rejected,
            "a client cannot act on its own process through the broker",
        ));
    }
    None
}

/// Map one platform error onto the wire code a client can act on.
///
/// Only a real `Ok(())` from an action produces `Response::Done`; everything
/// else is a refusal, so a client never believes a mutation happened.
fn error_response(error: TmError) -> Response {
    match error {
        TmError::ProcessNotFound { pid } => Response::error(
            Refusal::NotFound,
            format!("process {pid} exited or its creation identity changed"),
        ),
        TmError::Unsupported(what) => Response::error(
            Refusal::Unsupported,
            format!("operation not supported on this platform: {what}"),
        ),
        TmError::ServiceNotFound(unit) => {
            Response::error(Refusal::NotFound, format!("unit {unit} was not found"))
        }
        TmError::Platform { detail, .. } => {
            if is_not_found_detail(&detail) {
                Response::error(Refusal::NotFound, detail)
            } else {
                Response::error(Refusal::Failed, detail)
            }
        }
        other => Response::error(Refusal::Failed, other.to_string()),
    }
}

/// systemctl reports a missing unit as text, not as an error code; that is the
/// one OS failure the wire has a distinct code for.
fn is_not_found_detail(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "could not be found",
        "not found",
        "no such file",
        "does not exist",
        "not loaded",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

// ------------------------------------------------------------------ evidence

/// Read the root-only process evidence the wire carries.
///
/// `/proc/<pid>/io` is deliberately not read: [`ProcessEvidence`] has no field
/// for it, and touching a file nothing can render would only widen what the
/// broker reaches into without producing an answer.
fn collect_evidence(pid: u32, start_ticks: i64) -> Result<ProcessEvidence, TmError> {
    // `/proc/<pid>/stat` is the creation identity, read before and after the
    // walk so a recycled pid cannot be described with a mix of two processes.
    let before = process_start_for_read(pid, Some(start_ticks))?;
    let status = read_proc_text(pid, "status")
        .as_deref()
        .map(parse_status_fields)
        .unwrap_or_default();
    let rollup = read_proc_text(pid, "smaps_rollup")
        .as_deref()
        .map(parse_smaps_rollup)
        .unwrap_or_default();
    let limits = read_proc_text(pid, "limits")
        .as_deref()
        .map(parse_limits)
        .unwrap_or_default();
    let StatusFields {
        name,
        state,
        threads,
        uid,
        gid,
        euid,
        egid,
        capability_effective,
        capability_permitted,
        capability_bounding,
        seccomp_mode,
        no_new_privs,
        dumpable,
        rss_bytes,
        swap_bytes,
    } = status;

    let evidence = ProcessEvidence {
        pid,
        start_ticks,
        name: name.unwrap_or_default(),
        exe: read_link_text(format!("/proc/{pid}/exe")),
        cwd: read_link_text(format!("/proc/{pid}/cwd")),
        root: read_link_text(format!("/proc/{pid}/root")),
        uid,
        gid,
        euid,
        egid,
        state,
        threads,
        capability_effective,
        capability_permitted,
        capability_bounding,
        seccomp_mode,
        no_new_privs,
        dumpable,
        oom_score: read_proc_i32(pid, "oom_score"),
        oom_score_adj: read_proc_i32(pid, "oom_score_adj"),
        // smaps_rollup is the accounting sum; status carries the coarse
        // `VmRSS`/`VmSwap` when the rollup is not readable.
        rss_bytes: rollup.rss_bytes.or(rss_bytes),
        pss_bytes: rollup.pss_bytes,
        swap_bytes: rollup.swap_bytes.or(swap_bytes),
        open_fds: count_open_fds(pid),
        namespaces: read_namespaces(pid),
        cgroup: read_proc_text(pid, "cgroup")
            .as_deref()
            .and_then(parse_cgroup),
        security_label: read_proc_text(pid, "attr/current")
            .as_deref()
            .and_then(parse_security_label),
        limits,
    };
    ensure_process_generation(pid, before)?;
    Ok(evidence)
}

fn read_link_text(path: impl AsRef<Path>) -> Option<String> {
    let target = std::fs::read_link(path.as_ref()).ok()?;
    Some(target.to_string_lossy().into_owned())
}

/// Read one `/proc/<pid>` text record with a hard size cap.
///
/// A truncated read drops its last line: a half record must not be parsed into a
/// plausible-looking value.
fn read_proc_text(pid: u32, name: &str) -> Option<String> {
    let path = format!("/proc/{pid}/{name}");
    let file = std::fs::File::open(&path).ok()?;
    let mut bytes = Vec::with_capacity(4 * 1024);
    file.take(MAX_PROC_TEXT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let truncated = bytes.len() as u64 > MAX_PROC_TEXT_BYTES;
    if truncated {
        bytes.truncate(MAX_PROC_TEXT_BYTES as usize);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(if truncated {
        cut_last_line(&text)
    } else {
        text
    })
}

/// Keep only the complete lines of a truncated record.
fn cut_last_line(text: &str) -> String {
    match text.rfind('\n') {
        Some(end) => text[..=end].to_string(),
        None => String::new(),
    }
}

fn read_proc_i32(pid: u32, name: &str) -> Option<i32> {
    read_proc_text(pid, name)?.trim().parse().ok()
}

fn count_open_fds(pid: u32) -> Option<u32> {
    let entries = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    let mut count = 0u32;
    for _ in entries.flatten() {
        count = count.saturating_add(1);
        if count > MAX_OPEN_FDS {
            // A truncated count is a fabricated number, so answer unknown.
            return None;
        }
    }
    Some(count)
}

fn read_namespaces(pid: u32) -> Vec<NamespaceId> {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/ns")) else {
        return Vec::new();
    };
    let mut namespaces = Vec::new();
    for entry in entries.flatten() {
        if namespaces.len() >= MAX_NAMESPACE_ENTRIES {
            break;
        }
        let Some(target) = read_link_text(entry.path()) else {
            continue;
        };
        if let Some(namespace) = parse_namespace_link(&target) {
            namespaces.push(namespace);
        }
    }
    namespaces.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.inode.cmp(&b.inode)));
    namespaces.dedup_by(|a, b| a.kind == b.kind && a.inode == b.inode);
    namespaces
}

/// Everything one `/proc/<pid>/status` read can state. Every field is optional
/// because a line the kernel does not publish must not read as a value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StatusFields {
    name: Option<String>,
    state: Option<String>,
    threads: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    euid: Option<u32>,
    egid: Option<u32>,
    capability_effective: Option<u64>,
    capability_permitted: Option<u64>,
    capability_bounding: Option<u64>,
    seccomp_mode: Option<u32>,
    no_new_privs: Option<bool>,
    dumpable: Option<bool>,
    rss_bytes: Option<u64>,
    swap_bytes: Option<u64>,
}

fn parse_status_fields(text: &str) -> StatusFields {
    let mut fields = StatusFields::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "Name" => fields.name = (!value.is_empty()).then(|| value.to_string()),
            // `State` is "S (sleeping)"; the single-letter code is the fact.
            "State" => fields.state = value.split_whitespace().next().map(str::to_string),
            "Threads" => fields.threads = value.parse().ok(),
            "Uid" => {
                let (real, effective) = parse_id_pair(value);
                fields.uid = real;
                fields.euid = effective;
            }
            "Gid" => {
                let (real, effective) = parse_id_pair(value);
                fields.gid = real;
                fields.egid = effective;
            }
            "CapEff" => fields.capability_effective = parse_hex_u64(value),
            "CapPrm" => fields.capability_permitted = parse_hex_u64(value),
            "CapBnd" => fields.capability_bounding = parse_hex_u64(value),
            "Seccomp" => fields.seccomp_mode = value.parse().ok(),
            "NoNewPrivs" => fields.no_new_privs = parse_bool_flag(value),
            // Only newer kernels publish `Dumpable`; where it is missing the
            // answer stays unknown instead of being inferred from ptrace rules.
            "Dumpable" => fields.dumpable = parse_bool_flag(value),
            "VmRSS" => fields.rss_bytes = parse_kib_bytes(value),
            "VmSwap" => fields.swap_bytes = parse_kib_bytes(value),
            _ => {}
        }
    }
    fields
}

fn parse_id_pair(value: &str) -> (Option<u32>, Option<u32>) {
    let ids = value
        .split_whitespace()
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    match ids.as_slice() {
        [real, effective, ..] => (Some(*real), Some(*effective)),
        _ => (None, None),
    }
}

fn parse_bool_flag(value: &str) -> Option<bool> {
    match value {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn parse_hex_u64(value: &str) -> Option<u64> {
    u64::from_str_radix(value, 16).ok()
}

fn parse_kib_bytes(value: &str) -> Option<u64> {
    let kib: u64 = value.split_whitespace().next()?.parse().ok()?;
    kib.checked_mul(1024)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RollupFields {
    rss_bytes: Option<u64>,
    pss_bytes: Option<u64>,
    swap_bytes: Option<u64>,
}

fn parse_smaps_rollup(text: &str) -> RollupFields {
    let mut fields = RollupFields::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let bytes = parse_kib_bytes(value);
        match key {
            "Rss" => fields.rss_bytes = bytes,
            "Pss" => fields.pss_bytes = bytes,
            "Swap" => fields.swap_bytes = bytes,
            _ => {}
        }
    }
    fields
}

/// `/proc/<pid>/limits` rows are `<name> <soft> <hard> [<unit>]`, and the last
/// two rows (`Max nice priority`, `Max realtime priority`) have no unit column,
/// so the unit is the last word only when that word is not a value.
fn parse_limits(text: &str) -> Vec<ResourceLimit> {
    let mut limits = Vec::new();
    for line in text.lines().skip(1) {
        if limits.len() >= MAX_LIMIT_ROWS {
            break;
        }
        if let Some(limit) = parse_limit_row(line) {
            limits.push(limit);
        }
    }
    limits
}

fn parse_limit_row(line: &str) -> Option<ResourceLimit> {
    let words = line.split_whitespace().collect::<Vec<_>>();
    let unit_present = words.len() >= 4 && !is_limit_value(words[words.len() - 1]);
    let value_words = if unit_present {
        &words[..words.len() - 1]
    } else {
        &words[..]
    };
    if value_words.len() < 3 {
        return None;
    }
    let name_end = value_words.len() - 2;
    let name = value_words[..name_end].join(" ");
    if name.is_empty() || !name.bytes().any(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    Some(ResourceLimit {
        name,
        soft: parse_limit_value(value_words[name_end])?,
        hard: parse_limit_value(value_words[name_end + 1])?,
    })
}

/// `RLIM_INFINITY` on the wire is `u64::MAX`, so `unlimited` and the numeric
/// form of infinity cannot be confused with a real limit.
fn parse_limit_value(value: &str) -> Option<u64> {
    if value == "unlimited" {
        return Some(u64::MAX);
    }
    value.parse().ok()
}

fn is_limit_value(value: &str) -> bool {
    value == "unlimited" || value.parse::<u64>().is_ok()
}

/// Pick one cgroup path: the unified (v2) hierarchy when it exists, otherwise
/// the first hierarchy the kernel lists.
fn parse_cgroup(text: &str) -> Option<String> {
    let mut unified = None;
    let mut first = None;
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        if hierarchy == "0" && controllers.is_empty() {
            unified.get_or_insert_with(|| path.to_string());
        } else {
            first.get_or_insert_with(|| path.to_string());
        }
    }
    unified.or(first)
}

fn parse_security_label(text: &str) -> Option<String> {
    let label = text.trim().trim_matches('\0').trim();
    (!label.is_empty()).then(|| label.to_string())
}

fn parse_namespace_link(target: &str) -> Option<NamespaceId> {
    let (kind, rest) = target.split_once(":[")?;
    if kind.is_empty()
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    let inode = rest.strip_suffix(']')?;
    Some(NamespaceId {
        kind: kind.to_string(),
        inode: inode.parse().ok()?,
    })
}

// -------------------------------------------------------------- rate limiting

/// One slot per [`Refusal`] variant, so each kind gets its own window instead of
/// one noisy kind silencing the rest.
const REFUSAL_KIND_COUNT: usize = 11;
static REFUSAL_LOG_AT_MS: [AtomicU64; REFUSAL_KIND_COUNT] =
    [const { AtomicU64::new(0) }; REFUSAL_KIND_COUNT];
static BROKER_WARNING_AT_MS: AtomicU64 = AtomicU64::new(0);

fn refusal_index(code: Refusal) -> usize {
    match code {
        Refusal::UnauthorizedUser => 0,
        Refusal::MissingClientImage => 1,
        Refusal::ClientNotListed => 2,
        Refusal::UnprotectedClient => 3,
        Refusal::ProtocolMismatch => 4,
        Refusal::MalformedRequest => 5,
        Refusal::Unsupported => 6,
        Refusal::NotFound => 7,
        Refusal::Rejected => 8,
        Refusal::Failed => 9,
        Refusal::Busy => 10,
    }
}

/// Claim one log window. Returns `false` while the previous window is still
/// open, and records `now_ms` atomically so two threads cannot both log.
fn claim_log_window(slot: &AtomicU64, now_ms: u64, window_ms: u64) -> bool {
    let mut previous = slot.load(Ordering::Relaxed);
    loop {
        if previous != 0 && now_ms.saturating_sub(previous) < window_ms {
            return false;
        }
        match slot.compare_exchange_weak(previous, now_ms, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => previous = actual,
        }
    }
}

/// Monotonic milliseconds for rate limiting; never zero, so `0` can keep meaning
/// "never logged".
fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed().as_millis();
    elapsed.min(u128::from(u64::MAX - 1)) as u64 + 1
}

fn refusal_log_allowed(code: Refusal) -> bool {
    claim_log_window(
        &REFUSAL_LOG_AT_MS[refusal_index(code)],
        monotonic_ms(),
        REFUSAL_LOG_WINDOW_MS,
    )
}

fn broker_warning_allowed() -> bool {
    claim_log_window(
        &BROKER_WARNING_AT_MS,
        monotonic_ms(),
        BROKER_WARNING_WINDOW_MS,
    )
}

fn log_refusal(code: Refusal, detail: &str) {
    if refusal_log_allowed(code) {
        tracing::warn!(
            refusal = code.as_str(),
            %detail,
            "linux core service refused a request"
        );
    }
}

// ----------------------------------------------------------------- selfcheck

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketState {
    /// Nothing is listening; reported, but not a failure.
    Missing,
    /// A root-owned socket file exists.
    Present,
    /// Something is in the way, or the runtime directory is unprotected.
    Unsafe,
}

impl SocketState {
    fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Present => "present",
            Self::Unsafe => "unsafe",
        }
    }
}

struct SocketReport {
    state: SocketState,
    detail: String,
}

fn inspect_socket(path: &Path) -> SocketReport {
    if let Some(problem) = socket_directory_problem(Path::new(SOCKET_DIR)) {
        return SocketReport {
            state: SocketState::Unsafe,
            detail: problem,
        };
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return SocketReport {
                    state: SocketState::Unsafe,
                    detail: format!("{SOCKET_PATH} exists but is not a socket"),
                };
            }
            if metadata.uid() != 0 {
                return SocketReport {
                    state: SocketState::Unsafe,
                    detail: format!("{SOCKET_PATH} is owned by uid {}", metadata.uid()),
                };
            }
            SocketReport {
                state: SocketState::Present,
                detail: format!(
                    "{SOCKET_PATH} is a root-owned socket (mode {:o})",
                    metadata.mode() & 0o7777
                ),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => SocketReport {
            state: SocketState::Missing,
            detail: format!("{SOCKET_PATH} does not exist: the unit is not running"),
        },
        Err(error) => SocketReport {
            state: SocketState::Unsafe,
            detail: format!("{SOCKET_PATH} could not be inspected: {error}"),
        },
    }
}

fn socket_directory_problem(path: &Path) -> Option<String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => verify_socket_directory(path, &metadata).err(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Some(format!("{} does not exist", path.display()))
        }
        Err(error) => Some(format!(
            "{} could not be inspected: {error}",
            path.display()
        )),
    }
}

/// Open and immediately close one local connection, with a real deadline.
///
/// `UnixStream::connect` blocks indefinitely on a full backlog, so the self-check
/// does the connect itself: a non-blocking socket plus `poll` means the probe
/// costs one descriptor for at most [`SOCKET_PROBE_TIMEOUT_MS`] and leaves
/// nothing behind (a helper thread could not be cancelled and would outlive the
/// report).
fn probe_connection(path: &Path) -> &'static str {
    let Ok((address, length)) = unix_socket_address(path) else {
        return "unavailable";
    };
    // SAFETY: the family/type/protocol combination is valid, and the returned
    // descriptor is owned here from the next line on.
    let descriptor = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if descriptor < 0 {
        return "unavailable";
    }
    // SAFETY: `address` is a fully initialized `sockaddr_un` of `length` bytes.
    let connected = unsafe {
        libc::connect(
            descriptor,
            (&raw const address).cast::<libc::sockaddr>(),
            length,
        )
    };
    let outcome = if connected == 0 {
        "connected"
    } else {
        let error = io::Error::last_os_error();
        // The listener exists and the handshake is in flight; wait for it.
        // Anything else (a full backlog answers EAGAIN here) did not connect.
        match error.raw_os_error() {
            Some(libc::EINPROGRESS) => wait_for_connect(descriptor),
            _ => "refused",
        }
    };
    // SAFETY: the descriptor was created here and is closed exactly once.
    unsafe { libc::close(descriptor) };
    outcome
}

/// Wait for a pending non-blocking connect, then read its outcome.
fn wait_for_connect(descriptor: RawFd) -> &'static str {
    let mut polled = [libc::pollfd {
        fd: descriptor,
        events: libc::POLLOUT,
        revents: 0,
    }];
    // SAFETY: the array and its length match, and the descriptor is owned by
    // the caller for the duration of the call.
    let ready = unsafe { libc::poll(polled.as_mut_ptr(), 1, SOCKET_PROBE_TIMEOUT_MS) };
    if ready <= 0 {
        return if ready == 0 { "timeout" } else { "unavailable" };
    }
    let mut pending: libc::c_int = 0;
    let mut pending_length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the option value and its length describe `pending`.
    let result = unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&raw mut pending).cast::<libc::c_void>(),
            &mut pending_length,
        )
    };
    if result != 0 {
        return "unavailable";
    }
    if pending == 0 { "connected" } else { "refused" }
}

/// The kernel's `sockaddr_un` for one path.
///
/// `UnixStream::connect` builds this internally; the self-check needs the raw
/// form because it connects on a non-blocking socket.
fn unix_socket_address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: `sockaddr_un` is plain old data and a zeroed value is the empty
    // address, which the family and the path below fill in.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path does not fit in a sockaddr_un",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    // The trailing zero from `zeroed` terminates the path, and the kernel reads
    // `sun_path` as a C string, so the full struct size is a valid length.
    let length = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    Ok((address, length))
}

// ----------------------------------------------------------------------- tests

// ----------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A realistic `/proc/<pid>/status` for a root service process.
    const STATUS_TEXT: &str = "\
Name:\ttaskman-service\n\
Umask:\t0022\n\
State:\tS (sleeping)\n\
Tgid:\t812\n\
Pid:\t812\n\
PPid:\t1\n\
Threads:\t4\n\
Uid:\t0\t0\t0\t0\n\
Gid:\t0\t0\t0\t0\n\
CapEff:\t000001ffffffffff\n\
CapPrm:\t000001ffffffffff\n\
CapBnd:\t000001ffffffffff\n\
Seccomp:\t0\n\
NoNewPrivs:\t1\n\
VmRSS:\t  12345 kB\n\
VmSwap:\t      8 kB\n";

    #[test]
    fn status_text_supplies_identity_and_hardening_fields() {
        let fields = parse_status_fields(STATUS_TEXT);
        assert_eq!(fields.name.as_deref(), Some("taskman-service"));
        assert_eq!(fields.state.as_deref(), Some("S"));
        assert_eq!(fields.threads, Some(4));
        assert_eq!((fields.uid, fields.euid), (Some(0), Some(0)));
        assert_eq!((fields.gid, fields.egid), (Some(0), Some(0)));
        assert_eq!(fields.capability_effective, Some(0x0000_01ff_ffff_ffff));
        assert_eq!(fields.capability_permitted, Some(0x0000_01ff_ffff_ffff));
        assert_eq!(fields.capability_bounding, Some(0x0000_01ff_ffff_ffff));
        assert_eq!(fields.seccomp_mode, Some(0));
        assert_eq!(fields.no_new_privs, Some(true));
        assert_eq!(fields.rss_bytes, Some(12_345 * 1024));
        assert_eq!(fields.swap_bytes, Some(8 * 1024));
    }

    #[test]
    fn absent_status_lines_stay_unknown_rather_than_defaulting() {
        let fields = parse_status_fields("Name:\tworker\nUid:\tbroken\n");
        assert_eq!(fields.name.as_deref(), Some("worker"));
        // A malformed Uid line must not become uid 0.
        assert_eq!((fields.uid, fields.euid), (None, None));
        assert_eq!(fields.capability_effective, None);
        assert_eq!(fields.dumpable, None);
        assert_eq!(fields.seccomp_mode, None);
        assert_eq!(fields.no_new_privs, None);
        assert_eq!(fields.state, None);
        assert_eq!(fields.rss_bytes, None);
    }

    #[test]
    fn smaps_rollup_converts_kib_and_ignores_derived_rows() {
        let rollup = parse_smaps_rollup(
            "\
Rss:               40960 kB\n\
Pss:               20480 kB\n\
Pss_Dirty:             0 kB\n\
Swap:               1024 kB\n",
        );
        assert_eq!(rollup.rss_bytes, Some(40_960 * 1024));
        assert_eq!(rollup.pss_bytes, Some(20_480 * 1024));
        assert_eq!(rollup.swap_bytes, Some(1024 * 1024));

        let missing = parse_smaps_rollup("Rss:            12 kB\n");
        assert_eq!(missing.pss_bytes, None);
        assert_eq!(missing.swap_bytes, None);
    }

    #[test]
    fn limit_rows_tolerate_a_missing_units_column() {
        let limits = parse_limits(
            "\
Limit                     Soft Limit           Hard Limit           Units     \n\
Max cpu time              unlimited            unlimited            seconds   \n\
Max open files            1024                 1048576              files     \n\
Max nice priority         0                    0                              \n\
Max realtime timeout      unlimited            unlimited            us        \n",
        );
        assert_eq!(limits.len(), 4);
        assert_eq!(limits[0].name, "Max cpu time");
        assert_eq!((limits[0].soft, limits[0].hard), (u64::MAX, u64::MAX));
        assert_eq!(limits[1].name, "Max open files");
        assert_eq!((limits[1].soft, limits[1].hard), (1024, 1_048_576));
        assert_eq!(limits[2].name, "Max nice priority");
        assert_eq!((limits[2].soft, limits[2].hard), (0, 0));
        assert_eq!(limits[3].name, "Max realtime timeout");
    }

    #[test]
    fn rlimit_infinity_is_u64_max_and_nothing_else() {
        assert_eq!(parse_limit_value("unlimited"), Some(u64::MAX));
        assert_eq!(parse_limit_value("0"), Some(0));
        assert_eq!(parse_limit_value("-1"), None);
        assert_eq!(parse_limit_value("lots"), None);
        assert_eq!(parse_limit_row("garbage"), None);
    }

    #[test]
    fn limit_parsing_is_bounded() {
        let mut text = String::from("header\n");
        for row in 0..500 {
            text.push_str(&format!(
                "Max limit {row}    1                    2                    files\n"
            ));
        }
        assert_eq!(parse_limits(&text).len(), MAX_LIMIT_ROWS);
    }

    #[test]
    fn cgroup_prefers_the_unified_hierarchy() {
        assert_eq!(
            parse_cgroup("1:name=systemd:/system.slice/taskman-core.service\n0::/system.slice\n")
                .as_deref(),
            Some("/system.slice")
        );
        assert_eq!(
            parse_cgroup("4:cpu,cpuacct:/system.slice\n").as_deref(),
            Some("/system.slice")
        );
        assert_eq!(parse_cgroup("\n"), None);
    }

    #[test]
    fn namespace_links_need_a_kind_and_an_inode() {
        assert_eq!(
            parse_namespace_link("pid:[4026531836]"),
            Some(NamespaceId {
                kind: "pid".into(),
                inode: 4_026_531_836,
            })
        );
        assert_eq!(parse_namespace_link("garbage"), None);
        assert_eq!(parse_namespace_link(":[1]"), None);
        assert_eq!(parse_namespace_link("net:[not-a-number]"), None);
        assert_eq!(parse_namespace_link("net:4026531840"), None);
    }

    #[test]
    fn security_labels_never_become_an_empty_string() {
        assert_eq!(
            parse_security_label("unconfined\n").as_deref(),
            Some("unconfined")
        );
        assert_eq!(parse_security_label("  \n"), None);
        assert_eq!(parse_security_label("\0\0"), None);
    }

    #[test]
    fn truncated_proc_text_drops_its_partial_tail() {
        assert_eq!(cut_last_line("Name:\tx\nState:\tS (slee"), "Name:\tx\n");
        assert_eq!(cut_last_line("Name:\tx\n"), "Name:\tx\n");
        assert_eq!(cut_last_line("no newline"), "");
    }

    #[test]
    fn log_rate_limiting_opens_one_window_per_interval() {
        let slot = AtomicU64::new(0);
        assert!(claim_log_window(&slot, 1_000, 5_000));
        assert!(!claim_log_window(&slot, 1_000, 5_000));
        assert!(!claim_log_window(&slot, 5_999, 5_000));
        assert!(claim_log_window(&slot, 6_000, 5_000));
    }

    #[test]
    fn protected_directories_must_be_root_owned_and_unwritable() {
        assert!(protected_directory_problem("/run/taskman", Some(0), Some(0o755)).is_none());
        assert!(protected_directory_problem("/run/taskman", Some(1000), Some(0o755)).is_some());
        assert!(protected_directory_problem("/run/taskman", Some(0), Some(0o775)).is_some());
        assert!(protected_directory_problem("/run/taskman", Some(0), Some(0o757)).is_some());
        assert!(protected_directory_problem("/run/taskman", None, Some(0o755)).is_some());
        assert!(protected_directory_problem("/run/taskman", Some(0), None).is_some());
    }

    #[test]
    fn thread_states_map_onto_the_wire_enum() {
        assert_eq!(
            wire_thread_state(ThreadState::Runnable),
            WireThreadState::Running
        );
        assert_eq!(
            wire_thread_state(ThreadState::Sleeping),
            WireThreadState::Sleeping
        );
        assert_eq!(
            wire_thread_state(ThreadState::Waiting),
            WireThreadState::DiskSleep
        );
        assert_eq!(
            wire_thread_state(ThreadState::Stopped),
            WireThreadState::Stopped
        );
        assert_eq!(
            wire_thread_state(ThreadState::Terminated),
            WireThreadState::Zombie
        );
        assert_eq!(wire_thread_state(ThreadState::Idle), WireThreadState::Idle);
        assert_eq!(
            wire_thread_state(ThreadState::Parked),
            WireThreadState::Parked
        );
        assert_eq!(
            wire_thread_state(ThreadState::Other(u32::from(b'?'))),
            WireThreadState::Other(b'?')
        );
        // Windows-only scheduler states must not be invented as Linux ones, and
        // they carry no `/proc` byte to report.
        assert_eq!(
            wire_thread_state(ThreadState::Ready),
            WireThreadState::Other(u8::MAX)
        );
    }

    #[test]
    fn connection_names_round_trip_through_the_wire_strings() {
        assert_eq!(connection_protocol_name(ConnectionProtocol::Tcp), "tcp");
        assert_eq!(connection_protocol_name(ConnectionProtocol::Tcp6), "tcp6");
        assert_eq!(connection_protocol_name(ConnectionProtocol::Udp), "udp");
        assert_eq!(connection_protocol_name(ConnectionProtocol::Udp6), "udp6");
        assert_eq!(tcp_state_name(TcpState::Listen), "listen");
        assert_eq!(tcp_state_name(TcpState::Established), "established");
        assert_eq!(tcp_state_name(TcpState::Other(99)), "other-99");
    }

    #[test]
    fn a_vanished_process_answers_not_found() {
        match error_response(TmError::ProcessNotFound { pid: 4242 }) {
            Response::Error(failure) => {
                assert_eq!(failure.code, Refusal::NotFound);
                assert!(failure.detail.contains("4242"), "{}", failure.detail);
            }
            other => panic!("a vanished process must be a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_action_is_never_reported_as_done() {
        for error in [
            TmError::platform(
                "systemctl control service",
                "Unit nope.service could not be found.",
            ),
            TmError::platform("kill", "Operation not permitted"),
            TmError::Unsupported("systemd service control (trusted systemctl path)"),
        ] {
            assert!(
                matches!(error_response(error), Response::Error(_)),
                "a failed action became a success"
            );
        }
        assert_eq!(
            error_response(TmError::platform(
                "systemctl control service",
                "Unit nope.service could not be found."
            )),
            Response::error(Refusal::NotFound, "Unit nope.service could not be found.")
        );
    }

    #[test]
    fn mutation_guards_refuse_init_the_service_and_the_caller() {
        assert!(mutation_target_refusal(0, 1000).is_some());
        assert!(mutation_target_refusal(1, 1000).is_some());
        assert!(mutation_target_refusal(std::process::id(), 1000).is_some());
        assert!(mutation_target_refusal(4242, 4242).is_some());
        assert!(mutation_target_refusal(4242, 1000).is_none());
    }

    #[test]
    fn refusal_kinds_each_get_their_own_log_window() {
        let codes = [
            Refusal::UnauthorizedUser,
            Refusal::MissingClientImage,
            Refusal::ClientNotListed,
            Refusal::UnprotectedClient,
            Refusal::ProtocolMismatch,
            Refusal::MalformedRequest,
            Refusal::Unsupported,
            Refusal::NotFound,
            Refusal::Rejected,
            Refusal::Failed,
            Refusal::Busy,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for code in codes {
            assert!(
                seen.insert(refusal_index(code)),
                "two refusal kinds share one slot"
            );
        }
        assert_eq!(seen.len(), REFUSAL_KIND_COUNT);
    }

    /// Live proof that the evidence path parses what a real kernel publishes. It
    /// reads only this test process's own `/proc` entry, so it needs no privilege
    /// and no socket.
    #[test]
    fn this_process_evidence_reports_identity_and_limits() {
        let pid = std::process::id();
        let start_ticks = crate::linux::read_proc_start_ticks(pid).expect("own start ticks");
        let evidence = collect_evidence(pid, start_ticks).expect("own process evidence");
        assert_eq!(evidence.pid, pid);
        assert_eq!(evidence.start_ticks, start_ticks);
        assert!(!evidence.name.is_empty());
        assert!(evidence.exe.is_some());
        assert!(evidence.uid.is_some());
        assert!(evidence.threads.is_some_and(|threads| threads >= 1));
        assert!(!evidence.limits.is_empty());
        assert!(evidence.open_fds.is_some());
        // `/proc/<pid>/ns` can be hidden even for the caller's own process on a
        // hardened host, so the list may be empty; every entry that did parse
        // must still name a namespace kind.
        for namespace in &evidence.namespaces {
            assert!(!namespace.kind.is_empty(), "{namespace:?}");
        }
    }

    /// A mismatched creation identity must never produce evidence.
    #[test]
    fn evidence_for_a_changed_identity_is_not_found() {
        let pid = std::process::id();
        let start_ticks = crate::linux::read_proc_start_ticks(pid).expect("own start ticks");
        let error = collect_evidence(pid, start_ticks + 1).expect_err("identity mismatch");
        assert!(
            matches!(error, TmError::ProcessNotFound { .. }),
            "unexpected error: {error:?}"
        );
    }

    // ------------------------------------------------------------- tempdirs

    /// A unique directory under the system temp dir, removed on drop.
    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir()
                .join(format!("tm-broker-{label}-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).expect("test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Root-owned expectations cannot be produced by an unprivileged test run.
    fn running_as_root() -> bool {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        uid == 0
    }

    /// Skip a test this run cannot express, and SAY so.
    ///
    /// Most containers run the suite as root, where a foreign-owned file or
    /// directory cannot be created — exactly the coverage these tests exist for.
    /// A silent return would report a green suite with none of it, so the skip
    /// is announced and stays visible in the log.
    fn skip_when_root(test: &str) -> bool {
        if !running_as_root() {
            return false;
        }
        eprintln!(
            "note: {test} skipped, this run is root so a foreign-owned file cannot be created"
        );
        true
    }

    fn metadata_of(path: &Path) -> std::fs::Metadata {
        std::fs::symlink_metadata(path).expect("test metadata")
    }

    // ---------------------------------------------------------------- socket

    #[test]
    fn a_socket_directory_the_service_does_not_own_is_refused() {
        if skip_when_root("a_socket_directory_the_service_does_not_own_is_refused") {
            return;
        }
        let directory = TestDirectory::new("socket-dir");
        let error = verify_socket_directory(directory.path(), &metadata_of(directory.path()))
            .expect_err("a foreign directory must not be trusted");
        assert!(error.contains("owned by root"), "{error}");
        assert!(socket_directory_problem(directory.path()).is_some());
    }

    #[test]
    fn a_missing_socket_directory_is_created_with_the_pinned_mode() {
        if skip_when_root("a_missing_socket_directory_is_created_with_the_pinned_mode") {
            return;
        }
        let parent = TestDirectory::new("socket-create");
        let directory = parent.path().join("taskman");
        // Creation itself succeeds; the owner rule then refuses the result,
        // because an unprivileged test run cannot create a root-owned directory.
        let error = prepare_socket_directory(&directory).expect_err("owner check");
        assert!(error.contains("owned by root"), "{error}");
        let metadata = metadata_of(&directory);
        assert!(metadata.is_dir());
        assert_eq!(metadata.mode() & 0o7777, SOCKET_DIRECTORY_MODE);
    }

    #[test]
    fn stale_socket_removal_only_accepts_a_socket_that_root_owns() {
        let directory = TestDirectory::new("stale-socket");
        let missing = directory.path().join("missing.sock");
        assert!(remove_stale_socket(&missing).is_ok());

        let regular = directory.path().join("regular.sock");
        std::fs::write(&regular, b"not a socket").expect("regular file");
        let error = remove_stale_socket(&regular).expect_err("a regular file must survive");
        assert!(error.contains("not a socket"), "{error}");
        assert!(regular.exists(), "the refusal must not unlink anything");

        if skip_when_root("stale_socket_removal_only_accepts_a_socket_that_root_owns") {
            return;
        }
        let socket_path = directory.path().join("bound.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        drop(listener);
        let error = remove_stale_socket(&socket_path).expect_err("a foreign socket must survive");
        assert!(error.contains("owned by uid"), "{error}");
        assert!(socket_path.exists());
    }

    #[test]
    fn socket_directory_chain_rules_reject_a_missing_directory() {
        let directory = TestDirectory::new("chain");
        let missing = directory.path().join("absent");
        assert!(protected_directory_chain(&missing).is_some());
        // The directory it created is a real directory, but not root-owned.
        if !skip_when_root("socket_directory_chain_rules_reject_a_missing_directory") {
            assert!(protected_directory_chain(directory.path()).is_some());
        }
    }

    // ----------------------------------------------------------------- peers

    #[test]
    fn peer_credentials_describe_the_process_on_the_other_end() {
        let (server, _client) = UnixStream::pair().expect("socket pair");
        let credentials = peer_credentials(&server).expect("peer credentials");
        // Both ends belong to this process, so the kernel reports this process.
        assert_eq!(credentials.pid, std::process::id());
        // SAFETY: getuid/getgid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        assert_eq!(credentials.uid, uid);
        assert_eq!(credentials.gid, gid);
    }

    /// A refused client must receive an answer and a clean close, not a reset
    /// while it is still writing — a reset is what makes the GUI retry the
    /// operation under its own token.
    #[test]
    fn a_refusal_drains_what_the_client_is_still_writing() {
        let (mut server, mut client) = UnixStream::pair().expect("socket pair");
        let pending = b"GET / HTTP/1.1\r\n\r\n";
        client.write_all(pending).expect("client write");
        server.write_all(&[0u8; 8]).expect("server write");
        let started = Instant::now();
        finish_refusal(&mut server);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the drain must be bounded"
        );

        let mut received = Vec::new();
        let mut buffer = [0u8; 64];
        loop {
            match client.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => received.extend_from_slice(&buffer[..read]),
                Err(error) => panic!("a refused client must not see {error}"),
            }
        }
        // The response is intact and the write half is closed after it.
        assert_eq!(received.len(), 8);
    }

    /// The drain budget is ONE deadline, not one timeout per read.
    ///
    /// A peer that trickles bytes would otherwise buy itself a fresh timeout for
    /// every byte, and this runs on the accept thread: an unprivileged client
    /// must not be able to park the broker's control plane.
    #[test]
    fn a_trickling_client_cannot_stretch_the_refusal_drain() {
        let (mut server, mut client) = UnixStream::pair().expect("socket pair");
        // The producer really does sleep: a trickling peer is the condition
        // under test, not a raced handshake.
        let trickle = std::thread::spawn(move || {
            for _ in 0..16 {
                if client.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();
        finish_refusal(&mut server);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(1_000),
            "the drain must be bounded by one deadline, took {elapsed:?}"
        );
        trickle.join().expect("trickle thread");
    }

    /// Regression for a real install: the client resolution must check the
    /// directory chain ABOVE the client file. Passing the file itself made the
    /// rule answer "is not a directory", and the broker refused every start
    /// with `manifest client /usr/bin/taskman: /usr/bin/taskman is not a
    /// directory` — the service never came up on a packaged system.
    #[test]
    fn a_client_file_is_judged_through_the_chain_above_it_not_as_a_directory() {
        let file = Path::new("/usr/bin/env");
        if !file.exists() {
            eprintln!("skipping: this host has no /usr/bin/env");
            return;
        }
        assert!(
            protected_directory_chain_problem(file).is_none(),
            "a file inside a protected directory has a valid chain"
        );
        assert!(
            protected_directory_chain(file).is_some(),
            "and calling the directory rule on the file itself is the defect"
        );
    }

    /// The P0's executable pin, exercised against a real kernel: the identity
    /// the broker records for a peer must be the file object the kernel reports
    /// for that task, not a path it read somewhere.
    #[test]
    fn the_resolved_image_identity_is_the_kernel_executable_object() {
        let image = resolve_peer_image(std::process::id()).expect("own executable image");
        // `/proc/self/exe` and `/proc/<our pid>/exe` are the same magic link.
        let metadata = std::fs::metadata("/proc/self/exe").expect("own executable metadata");
        assert_eq!(image.device, metadata.dev());
        assert_eq!(image.inode, metadata.ino());
        assert_eq!(image.owner_uid, metadata.uid());
        assert_eq!(image.mode, metadata.mode() & 0o7777);
        // The path is a diagnostic only, but it must still name the image.
        assert!(!image.path.is_empty(), "{image:?}");
    }

    // ---------------------------------------------------- installed clients

    #[test]
    fn a_user_owned_client_file_refuses_the_broker() {
        if skip_when_root("a_user_owned_client_file_refuses_the_broker") {
            return;
        }
        let directory = TestDirectory::new("installed-client");
        let client = directory.path().join("taskman");
        std::fs::write(&client, b"#!/bin/sh\n").expect("client file");
        let mut manifest = Manifest::default();
        manifest.clients = vec![client.to_string_lossy().into_owned()];
        let error = resolve_installed_clients(&manifest).expect_err("a foreign client");
        assert!(error.contains("owned by root"), "{error}");
    }

    #[test]
    fn a_client_directory_the_user_owns_refuses_the_broker() {
        if skip_when_root("a_client_directory_the_user_owns_refuses_the_broker") {
            return;
        }
        let directory = TestDirectory::new("client-directory");
        let mut manifest = Manifest::default();
        manifest.clients = vec![format!("{}/", directory.path().to_string_lossy())];
        let error = resolve_installed_clients(&manifest).expect_err("a foreign directory");
        assert!(error.contains("owned by root"), "{error}");
    }

    #[test]
    fn a_missing_client_is_skipped_and_an_empty_install_is_refused() {
        let mut manifest = Manifest::default();
        manifest.clients = vec![
            "/nonexistent/taskman-client".into(),
            "/nonexistent/taskman-helper/".into(),
        ];
        let error = resolve_installed_clients(&manifest).expect_err("nothing resolved");
        assert!(error.contains("no manifest client resolved"), "{error}");
    }
}
