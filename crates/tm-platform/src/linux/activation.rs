//! Per-user Linux activation coordinator.
//!
//! The first GUI process owns an advisory lock and a mode-0600 Unix socket in
//! the user's runtime directory. Later launches ask that process to show and
//! focus its window. The kernel lock, not the PID written beside it, is the
//! authority: a recycled PID can never make a stale lock look live. A launch
//! whose handoff does not finish within a fixed deadline starts independently
//! instead of refusing to open.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{Shutdown, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOCK_FILE_NAME: &str = "taskman-activation.lock";
const SOCKET_FILE_NAME: &str = "taskman-activation.sock";
const LOCK_RECORD_HEADER: &str = "taskman-instance-v1";
const REQUEST_PREFIX: &str = "TASKMAN-ACTIVATE/1 ";
const RESPONSE_PREFIX: &str = "TASKMAN-ACTIVATED/1 ";
const REJECT_PREFIX: &str = "TASKMAN-ACTIVATION-REJECTED/1 ";
const MAX_MESSAGE_BYTES: usize = 128;
const HANDOFF_DEADLINE: Duration = Duration::from_secs(5);
const PER_CONNECTION_IO_TIMEOUT: Duration = Duration::from_secs(1);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// The process that owns activation coordination for this user session.
///
/// Dropping the owner stops the listener and removes only the socket inode it
/// created. The advisory lock remains held for the lifetime of this value.
pub struct ActivationOwner {
    stop: Arc<AtomicBool>,
    socket_path: PathBuf,
    socket_identity: SocketIdentity,
    inbox: ActivationInbox,
    wake: Arc<OwnedFd>,
    listener_thread: Option<JoinHandle<()>>,
    _lock_file: File,
}

/// Cloneable UI-side receiver for activation requests.
#[derive(Clone)]
pub struct ActivationInbox {
    shared: Arc<InboxShared>,
}

/// Result of trying to join an existing TaskMan instance.
pub enum ActivationAcquisition {
    /// Another process accepted the activation request.
    HandedOff,
    /// This process now owns the per-user listener.
    Primary {
        owner: ActivationOwner,
        inbox: ActivationInbox,
    },
    /// Coordination failed or timed out; the GUI should start independently.
    Uncoordinated { detail: String },
}

/// A request that must be applied on the GUI thread.
///
/// `acknowledge` releases the waiting secondary launch. A wedged primary never
/// acknowledges, so the secondary can start its own window after its deadline.
pub struct ActivationRequest {
    reply: Sender<bool>,
}

impl ActivationRequest {
    /// Confirm that the UI thread has queued the show/restore/focus commands.
    pub fn acknowledge(self) {
        let _ = self.reply.send(true);
    }

    /// Report that this compositor cannot confirm restoration/focus. The
    /// secondary launch will start an independent window instead.
    pub fn reject(self) {
        let _ = self.reply.send(false);
    }
}

struct InboxShared {
    receiver: Mutex<Receiver<ActivationRequest>>,
    sender: SyncSender<ActivationRequest>,
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    ready: Condvar,
}

impl ActivationInbox {
    /// Install the GUI waker. Replacing it is intentional across renderer
    /// fallback attempts; the newest egui context is the one that can repaint.
    pub fn install_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(waker);
        *tm_core::sync::lock(&self.shared.waker) = Some(waker.clone());
        self.shared.ready.notify_all();
        waker();
    }

    /// Take the next request for the UI thread to apply.
    pub fn try_recv(&self) -> Option<ActivationRequest> {
        match tm_core::sync::lock(&self.shared.receiver).try_recv() {
            Ok(request) => Some(request),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    fn wake(&self) {
        let waker = tm_core::sync::lock(&self.shared.waker).clone();
        if let Some(waker) = waker {
            waker();
        }
    }

    fn wait_until_ready(&self, stop: &AtomicBool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut waker = tm_core::sync::lock(&self.shared.waker);
        while waker.is_none() {
            if stop.load(Ordering::Acquire) {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let waited = self.shared.ready.wait_timeout(waker, remaining);
            match waited {
                Ok((guard, result)) => {
                    waker = guard;
                    if result.timed_out() && waker.is_none() {
                        return false;
                    }
                }
                Err(error) => {
                    let (guard, result) = error.into_inner();
                    waker = guard;
                    if result.timed_out() && waker.is_none() {
                        return false;
                    }
                }
            }
        }
        true
    }
}

impl ActivationOwner {
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.shared.ready.notify_all();
    }

    fn shared(&self) -> &InboxShared {
        &self.inbox.shared
    }
}

impl Drop for ActivationOwner {
    fn drop(&mut self) {
        self.stop();
        signal_wake(&self.wake);
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
        remove_socket_if_owned(&self.socket_path, self.socket_identity);
    }
}

/// Try to hand this launch to the already-running instance.
///
/// All failures are converted to `Uncoordinated` because a task manager must
/// still open when its previous copy is wedged or the runtime directory has
/// been tampered with.
pub fn acquire() -> ActivationAcquisition {
    let runtime_dir = match runtime_dir() {
        Ok(path) => path,
        Err(error) => {
            return ActivationAcquisition::Uncoordinated {
                detail: error.to_string(),
            };
        }
    };
    match acquire_in(&runtime_dir) {
        Ok(acquisition) => acquisition,
        Err(error) => ActivationAcquisition::Uncoordinated {
            detail: error.to_string(),
        },
    }
}

fn acquire_in(runtime_dir: &Path) -> io::Result<ActivationAcquisition> {
    let lock_path = runtime_dir.join(LOCK_FILE_NAME);
    let socket_path = runtime_dir.join(SOCKET_FILE_NAME);
    let lock_file = open_lock_file(&lock_path)?;

    if try_acquire_lock(&lock_file)? {
        return establish_primary(runtime_dir, lock_file, socket_path);
    }

    let deadline = Instant::now() + HANDOFF_DEADLINE;
    let mut last_error = None;
    while Instant::now() < deadline {
        match activate_existing(&socket_path, deadline) {
            Ok(true) => return Ok(ActivationAcquisition::HandedOff),
            Ok(false) => {
                return Ok(ActivationAcquisition::Uncoordinated {
                    detail: "primary could not confirm window activation".into(),
                });
            }
            Err(error) => last_error = Some(error),
        }

        // The owner may have exited while we were connecting. Re-checking the
        // flock is authoritative; no PID from the lock record is trusted.
        if try_acquire_lock(&lock_file)? {
            return establish_primary(runtime_dir, lock_file, socket_path);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Unix-domain sockets have no separate readiness object. This short
        // retry exists solely under the fixed handoff deadline while a primary
        // is binding its listener; it is not a synchronization guarantee.
        thread::sleep(remaining.min(CONNECT_RETRY_INTERVAL));
    }

    if try_acquire_lock(&lock_file)? {
        return establish_primary(runtime_dir, lock_file, socket_path);
    }

    let mut detail = last_error
        .map(|error| error.to_string())
        .unwrap_or_else(|| "primary did not become ready".to_string());
    if let Some(record) = read_lock_record_file(&lock_file)
        && lock_record_is_live(record)
    {
        detail.push_str(&format!("; lock owner pid {} is live", record.pid));
    }
    Ok(ActivationAcquisition::Uncoordinated { detail })
}

fn read_lock_record_file(file: &File) -> Option<LockRecord> {
    let mut file = file.try_clone().ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.take(4096).read_to_end(&mut bytes).ok()?;
    parse_lock_record(&String::from_utf8_lossy(&bytes))
}

fn establish_primary(
    runtime_dir: &Path,
    mut lock_file: File,
    socket_path: PathBuf,
) -> io::Result<ActivationAcquisition> {
    write_lock_record(&mut lock_file)?;
    let wake = Arc::new(create_wake_fd()?);
    remove_stale_socket(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)?;
    if let Err(error) = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)) {
        let _ = fs::remove_file(&socket_path);
        return Err(error);
    }
    let socket_identity = match read_socket_identity(&socket_path) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = fs::remove_file(&socket_path);
            return Err(error);
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        let _ = fs::remove_file(&socket_path);
        return Err(error);
    }

    let (sender, receiver) = mpsc::sync_channel(8);
    let inbox = ActivationInbox {
        shared: Arc::new(InboxShared {
            receiver: Mutex::new(receiver),
            sender,
            waker: Mutex::new(None),
            ready: Condvar::new(),
        }),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let thread_inbox = inbox.clone();
    let thread_stop = stop.clone();
    let thread_wake = wake.clone();
    let thread_socket_path = socket_path.clone();
    let thread_socket_identity = socket_identity;
    let listener_thread = thread::Builder::new()
        .name("tm-linux-activation".into())
        .spawn(move || {
            serve_activation(
                listener,
                thread_inbox,
                &thread_stop,
                &thread_wake,
                &thread_socket_path,
                thread_socket_identity,
            );
        })
        .map_err(|error| {
            remove_socket_if_owned(&socket_path, socket_identity);
            io::Error::other(format!(
                "failed to start activation listener in {}: {error}",
                runtime_dir.display()
            ))
        })?;

    Ok(ActivationAcquisition::Primary {
        owner: ActivationOwner {
            stop,
            socket_path,
            socket_identity,
            inbox: inbox.clone(),
            wake,
            listener_thread: Some(listener_thread),
            _lock_file: lock_file,
        },
        inbox,
    })
}

fn serve_activation(
    listener: UnixListener,
    inbox: ActivationInbox,
    stop: &AtomicBool,
    wake: &OwnedFd,
    socket_path: &Path,
    socket_identity: SocketIdentity,
) {
    loop {
        let mut poll_fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: the poll-descriptor array is live for the duration of the
        // call and describes two descriptors owned by this thread.
        let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, -1) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            tracing::warn!(%error, "Linux activation listener poll failed");
            break;
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        if poll_fds[1].revents != 0 {
            break;
        }
        if poll_fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            tracing::warn!("Linux activation listener reported a fatal poll event");
            break;
        }
        if poll_fds[0].revents & libc::POLLIN != 0 {
            loop {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => serve_activation_connection(stream, &inbox, stop),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        tracing::warn!(%error, "Linux activation listener accept failed");
                        remove_socket_if_owned(socket_path, socket_identity);
                        return;
                    }
                }
            }
        }
    }
    remove_socket_if_owned(socket_path, socket_identity);
}

fn serve_activation_connection(mut stream: UnixStream, inbox: &ActivationInbox, stop: &AtomicBool) {
    let deadline = Instant::now() + HANDOFF_DEADLINE;
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    let Ok(credentials) = peer_credentials(&stream) else {
        return;
    };
    let uid = effective_uid();
    if credentials.uid != uid {
        tracing::warn!(
            peer_pid = credentials.pid,
            peer_uid = credentials.uid,
            uid,
            "rejected cross-user Linux activation request"
        );
        return;
    }

    let Ok(message) = read_bounded_until(&mut stream, deadline) else {
        return;
    };
    let Some(nonce) = parse_message(&message, REQUEST_PREFIX) else {
        return;
    };

    let ready_wait = deadline.saturating_duration_since(Instant::now());
    if !inbox.wait_until_ready(stop, ready_wait) || stop.load(Ordering::Acquire) {
        return;
    }
    let (reply, acknowledged) = mpsc::channel();
    if inbox
        .shared
        .sender
        .try_send(ActivationRequest { reply })
        .is_err()
    {
        return;
    }
    inbox.wake();

    let ack_wait = deadline
        .saturating_duration_since(Instant::now())
        .min(PER_CONNECTION_IO_TIMEOUT);
    match acknowledged.recv_timeout(ack_wait) {
        Ok(true) => {}
        Ok(false) => {
            let response = encode_message(REJECT_PREFIX, nonce);
            let _ = write_all_until(&mut stream, response.as_bytes(), deadline);
            let _ = stream.shutdown(Shutdown::Write);
            return;
        }
        Err(_) => {
            tracing::warn!(
                peer_pid = credentials.pid,
                "Linux activation request was not acknowledged by the UI"
            );
            return;
        }
    }
    if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
        return;
    }
    let response = encode_message(RESPONSE_PREFIX, nonce);
    let _ = write_all_until(&mut stream, response.as_bytes(), deadline);
    let _ = stream.shutdown(Shutdown::Write);
}

fn activate_existing(socket_path: &Path, deadline: Instant) -> io::Result<bool> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "activation deadline expired",
        ));
    }
    let mut stream = connect_unix(socket_path, remaining)?;
    let nonce = fresh_nonce();
    write_all_until(
        &mut stream,
        encode_message(REQUEST_PREFIX, nonce).as_bytes(),
        deadline,
    )?;
    stream.shutdown(Shutdown::Write)?;
    let response = read_bounded_until(&mut stream, deadline)?;
    Ok(parse_message(&response, RESPONSE_PREFIX) == Some(nonce))
}

fn connect_unix(path: &Path, connect_timeout: Duration) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid activation socket path",
        ));
    }
    // SAFETY: `sockaddr_un` is a plain C structure; zero is its unused state.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "activation socket path is too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }

    // SAFETY: socket has no pointer arguments and returns an owned fd or -1.
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw_fd` is a fresh owned descriptor and is transferred exactly
    // once into `fd`; all early returns below close it through `OwnedFd`.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    // SAFETY: `fd` is live and `address` points to a fully initialized
    // `sockaddr_un` for the duration of the call.
    let connect_result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &address as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if connect_result != 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == libc::EINPROGRESS || code == libc::EAGAIN => {
                wait_writable(fd.as_raw_fd(), connect_timeout)?;
                socket_error(fd.as_raw_fd())?;
            }
            _ => return Err(error),
        }
    }

    let raw_fd = fd.into_raw_fd();
    // SAFETY: the descriptor was removed from `OwnedFd` immediately above and
    // is now transferred exactly once into `UnixStream`.
    let stream = unsafe { UnixStream::from_raw_fd(raw_fd) };
    stream.set_nonblocking(true)?;
    Ok(stream)
}

fn wait_writable(fd: libc::c_int, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Linux activation connect timed out",
            ));
        }
        let millis = remaining.as_millis().min(i32::MAX as u128).max(1) as libc::c_int;
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: `poll_fd` is a live initialized poll descriptor.
        let result = unsafe { libc::poll(&mut poll_fd, 1, millis) };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Linux activation connect timed out",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn socket_error(fd: libc::c_int) -> io::Result<()> {
    let mut error: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the output pointer and length describe a live `c_int`.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut error as *mut libc::c_int as *mut libc::c_void,
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error))
    }
}

fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCredentials> {
    // SAFETY: `ucred` is a plain C output structure; getsockopt initializes it.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the output pointer and length describe a live `ucred`.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut libc::ucred as *mut libc::c_void,
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() || credentials.pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Unix peer credentials",
        ));
    }
    Ok(PeerCredentials {
        pid: credentials.pid as u32,
        uid: credentials.uid,
    })
}

fn wait_fd(fd: libc::c_int, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Linux activation I/O deadline expired",
            ));
        }
        let millis = remaining.as_millis().min(i32::MAX as u128).max(1) as libc::c_int;
        let mut poll_fd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: poll_fd is a live initialized descriptor entry.
        let result = unsafe { libc::poll(&mut poll_fd, 1, millis) };
        if result > 0 {
            if poll_fd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return socket_error(fd);
            }
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Linux activation I/O deadline expired",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn read_bounded_until(stream: &mut UnixStream, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = [0u8; 64];
    loop {
        wait_fd(stream.as_raw_fd(), libc::POLLIN | libc::POLLHUP, deadline)?;
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if out.len() + read > MAX_MESSAGE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "activation message is too large",
                    ));
                }
                out.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(out)
}

fn write_all_until(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        wait_fd(stream.as_raw_fd(), libc::POLLOUT, deadline)?;
        match stream.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "activation peer closed during write",
                ));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn encode_message(prefix: &str, nonce: u64) -> String {
    format!("{prefix}{nonce:016x}\n")
}

fn parse_message(message: &[u8], prefix: &str) -> Option<u64> {
    let expected_len = prefix.len() + 16 + 1;
    if message.len() != expected_len
        || !message.starts_with(prefix.as_bytes())
        || message.last() != Some(&b'\n')
    {
        return None;
    }
    let nonce = &message[prefix.len()..prefix.len() + 16];
    if !nonce.iter().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    std::str::from_utf8(nonce)
        .ok()
        .and_then(|text| u64::from_str_radix(text, 16).ok())
}

fn fresh_nonce() -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    time ^ ((u64::from(std::process::id())) << 32) ^ sequence
}

fn create_wake_fd() -> io::Result<OwnedFd> {
    // SAFETY: eventfd has no pointer arguments and returns an owned fd or -1.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh eventfd descriptor and is transferred exactly
    // once into `OwnedFd`.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn signal_wake(fd: &OwnedFd) {
    let value = 1u64.to_ne_bytes();
    loop {
        // SAFETY: `value` is a live 8-byte buffer and `fd` is a valid eventfd.
        let written = unsafe {
            libc::write(
                fd.as_raw_fd(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
            )
        };
        if written == value.len() as libc::ssize_t {
            break;
        }
        let error = io::Error::last_os_error();
        if written >= 0 || error.kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
}

fn runtime_dir() -> io::Result<PathBuf> {
    let uid = effective_uid();
    if let Some(configured) = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        if configured.is_absolute() && socket_path_fits(&configured.join(SOCKET_FILE_NAME)) {
            match validate_private_dir(&configured, uid) {
                Ok(()) => return Ok(configured),
                Err(error) => tracing::warn!(
                    path = %configured.display(),
                    %error,
                    "XDG_RUNTIME_DIR is not private; using the validated fallback"
                ),
            }
        } else if !configured.is_absolute() {
            tracing::warn!(
                path = %configured.display(),
                "XDG_RUNTIME_DIR is not absolute; using the validated fallback"
            );
        } else {
            tracing::warn!(
                path = %configured.display(),
                "XDG_RUNTIME_DIR is too long for a Unix socket; using the validated fallback"
            );
        }
    }

    let fallback = Path::new("/tmp").join(format!("taskman-runtime-{uid}"));
    ensure_fallback_dir(&fallback, uid)?;
    Ok(fallback)
}

fn socket_path_fits(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: `sockaddr_un` is a plain C structure for which all-zero is the
    // documented unused state; only its fixed-size path capacity is read here.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    !bytes.is_empty() && !bytes.contains(&0) && bytes.len() < address.sun_path.len()
}

fn ensure_fallback_dir(path: &Path, uid: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_private_dir(path, uid)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateDirError {
    Missing,
    NotDirectory,
    Symlink,
    WrongOwner,
    InsecureMode,
}

impl std::fmt::Display for PrivateDirError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "directory does not exist",
            Self::NotDirectory => "path is not a directory",
            Self::Symlink => "directory is a symbolic link",
            Self::WrongOwner => "directory is owned by another user",
            Self::InsecureMode => "directory permissions are not 0700",
        })
    }
}

fn validate_private_dir(path: &Path, expected_uid: u32) -> Result<(), PrivateDirError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(PrivateDirError::Missing);
        }
        Err(_) => return Err(PrivateDirError::Missing),
    };
    if metadata.file_type().is_symlink() {
        return Err(PrivateDirError::Symlink);
    }
    if !metadata.is_dir() {
        return Err(PrivateDirError::NotDirectory);
    }
    if metadata.uid() != expected_uid {
        return Err(PrivateDirError::WrongOwner);
    }
    if metadata.mode() & 0o7777 != 0o700 {
        return Err(PrivateDirError::InsecureMode);
    }
    Ok(())
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "activation lock is not a regular file",
        ));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "activation lock has multiple hard links",
        ));
    }
    if metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "activation lock is owned by another user",
        ));
    }
    if metadata.mode() & 0o7777 != 0o600 {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn try_acquire_lock(file: &File) -> io::Result<bool> {
    // SAFETY: `file` owns a live descriptor; flock does not retain pointers.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(false),
        _ => Err(error),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LockRecord {
    pid: u32,
    start_ticks: i64,
}

fn write_lock_record(file: &mut File) -> io::Result<()> {
    let pid = std::process::id();
    let start_ticks = super::linux_proc_start_ticks(pid).unwrap_or(-1);
    let record = format!("{LOCK_RECORD_HEADER}\npid={pid}\nstart_ticks={start_ticks}\n");
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(record.as_bytes())?;
    file.flush()
}

fn parse_lock_record(text: &str) -> Option<LockRecord> {
    let mut lines = text.lines();
    if lines.next() != Some(LOCK_RECORD_HEADER) {
        return None;
    }
    let pid = lines.next()?.strip_prefix("pid=")?.parse().ok()?;
    let start_ticks = lines.next()?.strip_prefix("start_ticks=")?.parse().ok()?;
    if lines.next().is_some() {
        return None;
    }
    Some(LockRecord { pid, start_ticks })
}

fn lock_record_is_live(record: LockRecord) -> bool {
    record.pid > 0 && super::linux_proc_start_ticks(record.pid) == Some(record.start_ticks)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

fn read_socket_identity(path: &Path) -> io::Result<SocketIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "activation socket is not an owned Unix socket",
        ));
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let identity = match read_socket_identity(path) {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match connect_unix(path, Duration::from_millis(100)) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "an activation socket is live despite a stale lock",
        )),
        Err(error)
            if error.kind() == io::ErrorKind::ConnectionRefused
                || error.kind() == io::ErrorKind::NotFound =>
        {
            remove_socket_if_owned(path, identity);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn remove_socket_if_owned(path: &Path, expected: SocketIdentity) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_socket()
        && metadata.uid() == effective_uid()
        && metadata.dev() == expected.device
        && metadata.ino() == expected.inode
    {
        let _ = fs::remove_file(path);
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments and cannot fail.
    unsafe { libc::geteuid() }
}

#[derive(Debug, Clone, Copy)]
struct PeerCredentials {
    pid: u32,
    uid: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    fn test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "taskman-linux-activation-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn private_directory_requires_owner_and_mode_0700() {
        let dir = test_dir("private-dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(validate_private_dir(&dir, effective_uid()), Ok(()));

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            validate_private_dir(&dir, effective_uid()),
            Err(PrivateDirError::InsecureMode)
        );
        assert_eq!(
            validate_private_dir(&dir, effective_uid().wrapping_add(1)),
            Err(PrivateDirError::WrongOwner)
        );

        let link = dir.with_extension("link");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert_eq!(
            validate_private_dir(&link, effective_uid()),
            Err(PrivateDirError::Symlink)
        );
        let _ = fs::remove_file(link);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fallback_directory_is_created_owner_only() {
        let root = test_dir("fallback-root");
        let dir = root.join(format!("taskman-runtime-{}", effective_uid()));
        ensure_fallback_dir(&dir, effective_uid()).unwrap();
        assert_eq!(validate_private_dir(&dir, effective_uid()), Ok(()));
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o7777, 0o700);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn activation_message_parser_is_exact_and_bounded() {
        let nonce = 0x0123_4567_89ab_cdef;
        let request = encode_message(REQUEST_PREFIX, nonce);
        assert_eq!(
            parse_message(request.as_bytes(), REQUEST_PREFIX),
            Some(nonce)
        );
        assert_eq!(
            parse_message(
                encode_message(RESPONSE_PREFIX, nonce).as_bytes(),
                RESPONSE_PREFIX,
            ),
            Some(nonce)
        );
        assert_eq!(
            parse_message(format!("{request}\n").as_bytes(), REQUEST_PREFIX,),
            None
        );
        assert_eq!(
            parse_message(request.trim_end().as_bytes(), REQUEST_PREFIX),
            None
        );
        assert_eq!(
            parse_message(request.replace('0', "x").as_bytes(), REQUEST_PREFIX),
            None
        );
        assert_eq!(
            parse_message(
                encode_message(RESPONSE_PREFIX, nonce).as_bytes(),
                REQUEST_PREFIX,
            ),
            None
        );
    }

    #[test]
    fn stale_socket_is_removed_only_after_refused_connect() {
        let dir = test_dir("stale-socket");
        let socket = dir.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(remove_stale_socket(&socket).is_err());
        assert!(socket.exists());

        drop(listener);
        remove_stale_socket(&socket).unwrap();
        assert!(!socket.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn advisory_lock_survives_pid_record_reuse_checks() {
        let dir = test_dir("lock");
        let path = dir.join(LOCK_FILE_NAME);
        let first = open_lock_file(&path).unwrap();
        assert!(try_acquire_lock(&first).unwrap());
        assert_eq!(first.metadata().unwrap().mode() & 0o777, 0o600);
        let second = open_lock_file(&path).unwrap();
        assert!(!try_acquire_lock(&second).unwrap());
        drop(first);
        assert!(try_acquire_lock(&second).unwrap());

        let pid = std::process::id();
        let start_ticks = super::super::linux_proc_start_ticks(pid).unwrap();
        let live = LockRecord { pid, start_ticks };
        assert!(lock_record_is_live(live));
        assert!(!lock_record_is_live(LockRecord {
            pid,
            start_ticks: start_ticks + 1,
        }));
        assert!(!lock_record_is_live(LockRecord {
            pid: u32::MAX,
            start_ticks,
        }));
        assert_eq!(
            parse_lock_record(&format!(
                "{LOCK_RECORD_HEADER}\npid={pid}\nstart_ticks={start_ticks}\n"
            )),
            Some(live)
        );
        drop(second);
        let _ = fs::remove_dir_all(dir);
    }
}
