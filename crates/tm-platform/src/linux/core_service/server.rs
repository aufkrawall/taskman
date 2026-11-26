//! Root-side broker: socket setup, peer authentication and request dispatch.
//!
//! Runs only inside `taskman-service` as a systemd system unit. The transport
//! contract is [`crate::linux_core_service`]; this module is the implementation
//! the service binary calls.
//!
//! ## Shape
//!
//! The calling thread is the accept thread: it owns the listening socket,
//! resolves each connection's kernel credentials, and hands the connection to
//! one of two bounded workers through a 16-slot queue. A full queue is answered
//! with `Refusal::Busy` instead of blocking accept, and one request per
//! connection keeps a stalled client from pinning a worker forever.
//!
//! SIGTERM/SIGINT set an atomic flag and write one byte to a self-pipe; the
//! accept loop waits in `poll` on the listener and on that pipe, so a stop is
//! an event rather than a poll interval, and no listener is ever "woken" by
//! connecting to it (which would race the next accept).
//!
//! Nothing on a request path may panic, and nothing the kernel does not publish
//! is invented: an unreadable field stays `None`, never a fabricated zero.

use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::actions::{ConnectionProtocol, ServiceAction, TcpState, ThreadState};
use crate::linux_core_service::{
    ActionRequest, ClientImage, INSTALL_ROOT, MANIFEST_PATH, MAX_MANIFEST_BYTES, MAX_REQUEST_BYTES,
    MAX_RESPONSE_BYTES, Manifest, NamespaceId, PROTOCOL_VERSION, PeerCredentials, PeerVerdict,
    ProcessEvidence, Refusal, Request, RequestEnvelope, ResourceLimit, Response, ResponseEnvelope,
    SERVICE_UNIT, SOCKET_DIR, SOCKET_PATH, ServiceStatusInfo, ServiceVerb, WireConnection,
    WireModule, WireThread, WireThreadState, WireThreads, decode_frame, encode_frame,
    evaluate_peer, is_inside_install_root, manifest_protection_problem, read_frame,
    service_image_protection_problem,
};
use crate::linux_sensors::SensorSnapshot;
use tm_core::error::TmError;
use tm_core::model::PriorityClass;

use super::super::{
    ensure_process_generation, process_connections, process_modules, process_start_for_read,
    process_threads, process_tree, send_signal_checked, sensors, services, set_affinity_direct,
    set_priority_direct,
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

/// A listener that cannot accept this many times in a row is broken (not busy:
/// a full queue does not fail `accept`), so the failure stops being invisible.
const MAX_CONSECUTIVE_ACCEPT_FAILURES: u32 = 16;

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

/// Development escape hatch. Only ever an explicit argv entry: the packaged
/// unit's `ExecStart` never passes it, and it cannot be set from the manifest.
const DEV_ARGUMENT: &str = "--dev";

// ------------------------------------------------------------ public surface

/// Run the broker until the process is asked to stop.
///
/// Fails only when the service cannot start at all (no manifest, unprotected
/// install, socket that cannot be bound). A per-connection error never stops
/// the broker; the accept loop reports and keeps serving.
///
/// `--dev` on the command line disables the install-path and client-image
/// protection checks and logs a warning. It exists so a developer can run the
/// broker out of a build tree; the packaged unit never passes it, and passing
/// it requires root anyway, so it cannot widen anyone's privileges.
pub fn run_service() -> Result<(), String> {
    let dev = std::env::args().any(|argument| argument == DEV_ARGUMENT);
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    if uid != 0 {
        return Err(format!(
            "the Linux core service must run as root (euid 0), this process has uid {uid}"
        ));
    }
    if dev {
        tracing::warn!(
            "running the Linux core service in dev mode: the install-root and client-image \
             protection checks are disabled"
        );
    }

    let manifest = load_manifest()?;
    let (service_image, install_verified) = verify_service_image(dev)?;
    let listener = prepare_listener()?;

    let state = Arc::new(ServiceState {
        manifest,
        started: Instant::now(),
        started_unix_s: unix_now_s(),
        install_verified,
        service_image,
        sensors: Mutex::new(SensorCache::default()),
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

/// Everything the workers share. Immutable after startup except the cache and
/// the counters, both behind their own lock/atomics.
struct ServiceState {
    manifest: Manifest,
    started: Instant,
    started_unix_s: u64,
    /// Only ever true when the full install verification ran and passed; dev
    /// mode reports `false` rather than claiming a check it skipped.
    install_verified: bool,
    service_image: String,
    sensors: Mutex<SensorCache>,
    requests_served: AtomicU64,
    refusals: AtomicU64,
}

/// The one polled sensor snapshot, so a burst of GUI requests is one sysfs +
/// NVML poll and not one poll per request.
#[derive(Default)]
struct SensorCache {
    last: Option<(Instant, SensorSnapshot)>,
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
    /// The poll runs while the lock is held on purpose: a queued request then
    /// waits for the poll in flight instead of starting a second one, which is
    /// exactly the "a burst is not a burst of polling" contract.
    fn sensor_snapshot(&self) -> SensorSnapshot {
        let window = Duration::from_millis(self.manifest.sensor_cache_ms);
        let mut cache = tm_core::sync::lock(&self.sensors);
        if let Some((taken, snapshot)) = cache.last.as_ref()
            && taken.elapsed() < window
        {
            return snapshot.clone();
        }
        let snapshot = sensors::snapshot();
        cache.last = Some((Instant::now(), snapshot.clone()));
        snapshot
    }

    /// Last poll's availability and timestamp, without polling anything: a
    /// status request must never be the reason sysfs is read.
    fn last_sensor_poll(&self) -> (bool, i64) {
        match tm_core::sync::lock(&self.sensors).last.as_ref() {
            Some((_, snapshot)) => (!snapshot.is_empty(), snapshot.taken_at_unix_ms),
            None => (false, 0),
        }
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

/// Resolve the peer's executable the way the policy expects it: the kernel's
/// own `/proc/<pid>/exe`, then that file's owner and mode.
///
/// The ` (deleted)` suffix is deliberately NOT stripped: a replaced binary no
/// longer resolves, and the honest answer is "no client image" rather than
/// checking the manifest against whichever file now has that path.
fn resolve_client_image(pid: u32) -> Option<ClientImage> {
    if pid == 0 {
        return None;
    }
    let link = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let path = link.to_string_lossy().into_owned();
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(ClientImage {
        path,
        owner_uid: metadata.uid(),
        mode: metadata.mode() & 0o7777,
    })
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
/// `signal(2)` is enough even though its handlers may run with `SA_RESTART`:
/// the stop path never depends on an interrupted syscall, because the handler
/// writes to the self-pipe the accept loop is waiting on.
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
    let handler: extern "C" fn(libc::c_int) = on_stop_signal;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler has the signature `signal` requires and only
        // touches atomics plus the async-signal-safe `write`.
        let previous = unsafe { libc::signal(signal, handler as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
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

/// Resolve kernel credentials and queue the connection, or answer `Busy`.
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
    match work_tx.try_send(ClientWork { stream, peer }) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(work)) => {
            // Back-pressure, not a fault: answer instead of blocking accept, so
            // one slow client cannot stop the socket from being drained.
            let mut stream = work.stream;
            if broker_warning_allowed() {
                tracing::warn!("the broker's request queue is full; answering busy");
            }
            let _ = stream.set_write_timeout(Some(BUSY_TIMEOUT));
            send_response(
                state,
                &mut stream,
                Response::error(Refusal::Busy, "the broker's request queue is full"),
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

/// Authenticate one peer, serve exactly one request, and drop the connection.
fn handle_client(work: ClientWork, state: &ServiceState) {
    let ClientWork { mut stream, peer } = work;
    // Bounded before the first write, so the refusal path cannot park a worker
    // on a client that never drains its socket.
    if let Err(error) = stream.set_write_timeout(Some(REQUEST_TIMEOUT)) {
        if broker_warning_allowed() {
            tracing::warn!(%error, "could not set a broker write deadline");
        }
        return;
    }
    let image = resolve_client_image(peer.pid);
    if let PeerVerdict::Refused(code) = evaluate_peer(&state.manifest, &peer, image.as_ref()) {
        let detail = auth_refusal_detail(code, &peer, image.as_ref());
        log_refusal(code, &detail);
        send_response(state, &mut stream, Response::error(code, detail));
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
                log_refusal(Refusal::MalformedRequest, &detail);
                send_response(
                    state,
                    &mut stream,
                    Response::error(Refusal::MalformedRequest, detail),
                );
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
            log_refusal(Refusal::MalformedRequest, &detail);
            send_response(
                state,
                &mut stream,
                Response::error(Refusal::MalformedRequest, detail),
            );
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
        log_refusal(Refusal::ProtocolMismatch, &detail);
        send_response(
            state,
            &mut stream,
            Response::error(Refusal::ProtocolMismatch, detail),
        );
        return;
    }
    if let Err(problem) = envelope.request.validate() {
        log_refusal(Refusal::MalformedRequest, &problem);
        send_response(
            state,
            &mut stream,
            Response::error(Refusal::MalformedRequest, problem),
        );
        return;
    }

    let label = envelope.request.label();
    state.count_request();
    let response = dispatch(state, peer.pid, envelope.request);
    tracing::debug!(
        peer_uid = peer.uid,
        request = label,
        "broker request served"
    );
    send_response(state, &mut stream, response);
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
        Request::ProcessConnections { pid } => connections_response(pid),
        Request::ProcessEvidence { pid, start_ticks } => match collect_evidence(pid, start_ticks) {
            Ok(evidence) => Response::Evidence(Box::new(evidence)),
            Err(error) => error_response(error),
        },
        Request::Action { action } => run_action(peer_pid, action),
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

fn connections_response(pid: u32) -> Response {
    // The wire carries no creation identity for this read, so the broker takes
    // its own bracket around the walk.
    let before = match process_start_for_read(pid, None) {
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
        // Windows-only scheduler states have no procfs meaning, and an unmapped
        // kernel byte lands here too.
        ThreadState::Initialized
        | ThreadState::Ready
        | ThreadState::Standby
        | ThreadState::Transition
        | ThreadState::DeferredReady
        | ThreadState::Other(_) => WireThreadState::Other,
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

fn run_action(peer_pid: u32, action: ActionRequest) -> Response {
    match action {
        ActionRequest::Kill {
            pid,
            start_ticks,
            tree,
        } => {
            if let Some(refusal) = mutation_target_refusal(pid, peer_pid) {
                return refusal;
            }
            match process_tree::kill_process(pid, Some(start_ticks), tree) {
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
            if unit == SERVICE_UNIT {
                return Response::error(
                    Refusal::Rejected,
                    "the broker does not control its own unit",
                );
            }
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

/// Open and immediately close one local connection.
///
/// The connect itself can park on a full backlog, so it runs on a helper thread
/// with a deadline; the broker answers a connection that closes without a
/// request by dropping it, and the helper cannot outlive the process.
fn probe_connection(path: &Path) -> &'static str {
    let (sender, receiver) = std::sync::mpsc::channel();
    let target: PathBuf = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("tm-selfcheck-connect".into())
        .spawn(move || {
            let _ = sender.send(UnixStream::connect(&target).is_ok());
        });
    if spawned.is_err() {
        return "unavailable";
    }
    match receiver.recv_timeout(Duration::from_millis(1_000)) {
        Ok(true) => "connected",
        Ok(false) => "refused",
        Err(_) => "timeout",
    }
}

// ----------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

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
            WireThreadState::Other
        );
        // Windows-only scheduler states must not be invented as Linux ones.
        assert_eq!(
            wire_thread_state(ThreadState::Ready),
            WireThreadState::Other
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
        assert!(!evidence.namespaces.is_empty());
        assert!(evidence.open_fds.is_some());
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
}
