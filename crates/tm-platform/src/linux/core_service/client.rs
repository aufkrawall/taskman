//! GUI-side client for the Linux core service.
//!
//! One process-wide connection, serialized behind a mutex, with a bounded
//! deadline per request: sampling must never park on a wedged broker. A
//! refused request is returned as [`ClientError::Refused`] and never retried
//! through another path — an explicit broker rejection must not degrade into
//! "try it under the GUI token anyway".
//!
//! ## What an error means
//!
//! [`ClientError`] itself lives in [`crate::linux_core_bridge`], together with
//! the refusal-to-error mapping, because those decisions are the same on every
//! host and their tests run in the ordinary gate. This file only produces them.
//!
//! [`ClientError`] keeps three cases apart because the caller's next step
//! depends on them:
//!
//! - [`ClientError::Unavailable`] — no answer. The socket is missing, the
//!   connect/write/read hit [`CLIENT_DEADLINE_MS`], or the two sides answered
//!   with different protocol versions. Nothing was decided about the request,
//!   so a caller may still use the path it used before this service existed.
//!   A transport failure also drops the socket and starts
//!   [`UNAVAILABLE_COOLDOWN`], so an absent broker costs one failing `connect`
//!   per window instead of one connect timeout per tab read or sampling tick.
//! - [`ClientError::Refused`] — the broker answered and declined: an
//!   authorization or identity decision, an action its own policy rejects, a
//!   request it could not parse. That is final. Working it around would turn
//!   "you may not ask me" into "ask the user for root instead".
//! - [`ClientError::Failed`] — the broker answered but could not carry the
//!   request out: an operation it does not implement, no free worker, or the
//!   OS error of an action it did attempt. Also final, for a different reason:
//!   an action that may have partially run must not be repeated under another
//!   token, and the reason must stay visible instead of dissolving into an
//!   interactive prompt that cannot address the same target.
//!
//! [`ClientError::may_fall_back`] is the one predicate a caller needs, and only
//! the transport level satisfies it.
//!
//! ## Locking and the deadline
//!
//! The connection mutex is held from the start of an exchange until its answer
//! arrives, so a second caller waits for the request in flight — bounded by the
//! remaining part of one [`CLIENT_DEADLINE_MS`] budget that connect, write and
//! read share. A `connect` that outlives that budget is left to its own thread,
//! because a Unix `connect` blocks while a listener's backlog is full; the lock
//! is NOT released while the wait continues, the helper thread only caps how
//! long the caller can be held by it. Because a failure immediately starts the
//! cooldown, callers queued behind a wedged broker short-circuit instead of
//! each paying the deadline in turn.
//!
//! The status cache is a separate mutex that is only ever held for the moment
//! it takes to read or replace one value, never across IO, and its probe runs on
//! its own thread — so a status row cannot be blocked by a request on the wire.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::linux_core_bridge::response_label;
use crate::linux_core_service::{
    CLIENT_DEADLINE_MS, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION, Request,
    RequestEnvelope, Response, ResponseEnvelope, SOCKET_PATH, ServiceStatusInfo, decode_frame,
    encode_frame, read_frame,
};

/// Hard ceiling for one request, connect included. Sampling must never park on
/// a wedged service, so this bounds every blocking step of an exchange.
const DEADLINE: Duration = Duration::from_millis(CLIENT_DEADLINE_MS);

/// How long a transport failure keeps the client from trying again. Long enough
/// that a sampler tick cannot pay a connect timeout on every frame, short
/// enough that a service started after the GUI is picked up without a restart.
const UNAVAILABLE_COOLDOWN: Duration = Duration::from_secs(4);

/// How long one Status answer is reused. An install's version and capabilities
/// do not change between two frames, and a refused status must not turn a
/// per-frame caller into a per-frame connect.
const STATUS_TTL: Duration = Duration::from_secs(10);

/// The broker client's error type, defined once in the neutral bridge so its
/// decisions and their tests are not Linux-only.
pub use crate::linux_core_bridge::ClientError;

/// The one connection, held across an exchange.
struct Connection {
    socket: Option<UnixStream>,
    /// Set after a transport failure; no request is attempted before it passes.
    retry_after: Option<Instant>,
}

impl Connection {
    const fn new() -> Self {
        Self {
            socket: None,
            retry_after: None,
        }
    }

    fn in_cooldown(&self) -> bool {
        self.retry_after.is_some_and(|at| Instant::now() < at)
    }

    /// Drop the socket after a transport failure and start the cooldown.
    fn fail(&mut self, detail: String) -> ClientError {
        self.socket = None;
        self.retry_after = Some(Instant::now() + UNAVAILABLE_COOLDOWN);
        ClientError::Unavailable(detail)
    }
}

/// One status outcome and the instant it was produced.
///
/// A refusal or a failure is a real answer about this install and is cached
/// like a success, so a broker that will not serve us is not probed on every
/// frame.
struct CachedStatus {
    at: Instant,
    result: Result<ServiceStatusInfo, ClientError>,
}

static CONNECTION: Mutex<Connection> = Mutex::new(Connection::new());
static STATUS: Mutex<Option<CachedStatus>> = Mutex::new(None);
/// Whether a status probe is on the wire. At most one runs at a time.
static PROBING: AtomicBool = AtomicBool::new(false);

fn connection() -> MutexGuard<'static, Connection> {
    match CONNECTION.lock() {
        Ok(guard) => guard,
        // A panicking request must not disable the broker for the rest of the
        // session. The state is one socket and one deadline, and the socket may
        // hold a half-written frame, so it is dropped instead of trusted.
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            *guard = Connection::new();
            guard
        }
    }
}

/// Send one request through the shared connection.
///
/// The answer, including a refusal, belongs to the caller; the status cache is
/// filled by [`probe_once`] alone, so a per-process read cannot overwrite what
/// the status row is showing.
pub fn request(request: &Request) -> Result<Response, ClientError> {
    let mut connection = connection();
    exchange(&mut connection, request)
}

/// The last status answer, refreshed behind the caller.
///
/// This never touches the connection mutex and never waits for an exchange: a
/// status row is drawn on the render path, and a wedged broker must not freeze
/// a frame. An empty or expired entry starts one background probe — at most one
/// at a time — and the previous answer is returned unchanged.
///
/// Only the probe writes the cache, so a per-process read cannot overwrite the
/// status row; conversely a broker that dies between two probes keeps reporting
/// its last answer until that window expires.
///
/// The probe needs no privilege, so this is also what an unelevated
/// `--selfcheck`-style diagnostic uses.
pub fn status() -> Result<ServiceStatusInfo, ClientError> {
    let cached = match STATUS.try_lock() {
        Ok(cache) => cache
            .as_ref()
            .map(|entry| (entry.at.elapsed(), entry.result.clone())),
        // A probe is replacing the entry right now. Waiting would put a caller
        // behind a deadline, and inventing an answer is worse than none.
        Err(TryLockError::WouldBlock) => {
            return Err(ClientError::Unavailable(
                "the service status is being refreshed".into(),
            ));
        }
        // A panic while writing the cache must not disable the status surface:
        // the entry is one value, so it is dropped and the next probe rebuilds
        // it.
        Err(TryLockError::Poisoned(poisoned)) => {
            let mut cache = poisoned.into_inner();
            *cache = None;
            None
        }
    };
    match cached {
        Some((age, result)) if age < STATUS_TTL => result,
        // Expired: answer with what is known and refresh behind the caller.
        Some((_, stale)) => {
            request_probe();
            stale
        }
        None => {
            request_probe();
            Err(ClientError::Unavailable(
                "the service has not answered a status probe yet".into(),
            ))
        }
    }
}

/// Start one status probe unless one is already running.
fn request_probe() {
    if PROBING.swap(true, Ordering::AcqRel) {
        // A probe is on the wire; it refreshes the same entry, so a second one
        // would only cost another deadline.
        return;
    }
    if std::thread::Builder::new()
        .name("tm-core-status".into())
        .spawn(probe_once)
        .is_err()
    {
        // A refused thread leaves the cached answer in place; the next caller
        // retries rather than the client staying stuck with the flag set.
        PROBING.store(false, Ordering::Release);
    }
}

fn probe_once() {
    let result = match request(&Request::Status) {
        Ok(Response::Status(info)) => Ok(*info),
        // A payload that is not a status is a protocol defect, not "no broker".
        Ok(other) => Err(ClientError::Failed(format!(
            "the service answered a status request with a {} payload",
            response_label(&other)
        ))),
        Err(error) => Err(error),
    };
    let mut cache = match STATUS.lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    *cache = Some(CachedStatus {
        at: Instant::now(),
        result,
    });
    PROBING.store(false, Ordering::Release);
}

/// Whether the last status probe reached a broker.
///
/// A pure cache read that never connects, so a caller on the UI thread cannot
/// park on a wedged socket. A lock that cannot be taken and a lock a panic
/// poisoned both prove nothing and therefore answer `false`; the poisoned case
/// also drops the broken entry so the next probe rebuilds it.
pub fn available() -> bool {
    match STATUS.try_lock() {
        Ok(cache) => cache.as_ref().is_some_and(|entry| entry.result.is_ok()),
        // A probe is swapping one value right now: nothing is known yet.
        Err(TryLockError::WouldBlock) => false,
        // A poisoned cache is never evidence of a live broker.
        Err(TryLockError::Poisoned(poisoned)) => {
            let mut cache = poisoned.into_inner();
            *cache = None;
            false
        }
    }
}

fn exchange(connection: &mut Connection, request: &Request) -> Result<Response, ClientError> {
    if connection.in_cooldown() {
        return Err(ClientError::Unavailable(
            "the service did not answer recently; waiting out the cooldown".into(),
        ));
    }
    if let Err(problem) = request.validate() {
        // Our own bug rather than the broker's decision: nothing was sent.
        return Err(ClientError::Failed(format!("invalid request: {problem}")));
    }
    // One absolute budget for connect, write and read together.
    let deadline = Instant::now() + DEADLINE;
    if connection.socket.is_none() {
        let socket = match connect_bounded(deadline) {
            Ok(socket) => socket,
            // Not installed, not running, or no search permission on the
            // socket directory: in every case there is nobody to ask.
            Err(error) => return Err(connection.fail(format!("connect {SOCKET_PATH}: {error}"))),
        };
        if let Err(error) = configure(&socket) {
            return Err(connection.fail(format!("socket setup failed: {error}")));
        }
        connection.socket = Some(socket);
    }
    // Take the socket out so a failure can drop it without borrowing the
    // connection twice; it is handed back only once the exchange succeeded.
    let socket = match connection.socket.take() {
        Some(socket) => socket,
        None => return Err(connection.fail("the connection lost its socket".into())),
    };
    let body = match transact(&socket, request, deadline) {
        Ok(body) => {
            connection.socket = Some(socket);
            body
        }
        Err(error) => {
            return Err(connection.fail(format!("{} failed: {error}", request.label())));
        }
    };
    let envelope: ResponseEnvelope = match decode_frame(&body) {
        Ok(envelope) => envelope,
        Err(error) => return Err(connection.fail(format!("unreadable response: {error}"))),
    };
    if envelope.protocol != PROTOCOL_VERSION {
        // A version mismatch on the wire is a broken install, not a decision
        // about this request: the caller may still use its own path.
        return Err(connection.fail(format!(
            "this client speaks protocol {PROTOCOL_VERSION}, the service answered with {}",
            envelope.protocol
        )));
    }
    match envelope.response {
        // The refusal-to-error split is a decision table, so it lives in the
        // neutral bridge where the ordinary gate tests it.
        Response::Error(failure) => Err(crate::linux_core_bridge::service_failure_to_client_error(
            failure,
        )),
        response => Ok(response),
    }
}

fn configure(socket: &UnixStream) -> io::Result<()> {
    socket.set_read_timeout(Some(DEADLINE))?;
    socket.set_write_timeout(Some(DEADLINE))?;
    Ok(())
}

/// What is left of one request's budget, or the deadline error once it is gone.
fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the service request deadline expired",
        ));
    }
    Ok(left)
}

/// Whether a connection attempt is still in flight.
///
/// At most one is ever outstanding: a listener that stopped accepting cannot
/// accumulate helper threads or hold the connection mutex while it is wedged.
static CONNECTING: AtomicBool = AtomicBool::new(false);

/// Connect to the broker without letting a wedged listener hold the lock.
///
/// `std` offers no connect timeout, and a Unix `connect` blocks while the
/// listener's backlog is full. Doing the attempt on a short-lived thread keeps
/// that wait off the connection mutex; an attempt that outlives the deadline is
/// simply abandoned, and its socket is closed whenever the thread returns.
fn connect_bounded(deadline: Instant) -> io::Result<UnixStream> {
    let budget = remaining(deadline)?;
    if CONNECTING.swap(true, Ordering::AcqRel) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "a connection attempt is still in flight",
        ));
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    // Dropping the handle only detaches: an abandoned attempt exits on its own
    // and closes whatever socket it managed to obtain.
    let _attempt = match std::thread::Builder::new()
        .name("tm-core-connect".into())
        .spawn(move || {
            let result = UnixStream::connect(SOCKET_PATH);
            // A failed send only means the caller already gave up.
            let _ = sender.send(result);
            CONNECTING.store(false, Ordering::Release);
        }) {
        Ok(attempt) => attempt,
        Err(error) => {
            CONNECTING.store(false, Ordering::Release);
            return Err(error);
        }
    };
    match receiver.recv_timeout(budget) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the service did not accept the connection in time",
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "the connection attempt ended without an answer",
        )),
    }
}

/// A socket that refuses to block past one absolute deadline.
///
/// A plain `SO_RCVTIMEO` bounds each `read` call, so a service that dribbles a
/// frame one byte per timeout could keep a caller far past [`CLIENT_DEADLINE_MS`].
/// Re-arming the timeout with what is left of the request's budget makes the
/// deadline absolute without re-implementing the protocol's framing.
struct Deadlined<'a> {
    socket: &'a UnixStream,
    deadline: Instant,
}

impl Read for Deadlined<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket
            .set_read_timeout(Some(remaining(self.deadline)?))?;
        // `Read` is implemented for `&UnixStream`, so the shared reference is
        // enough; the socket is not consumed by a read.
        let mut socket = self.socket;
        socket.read(buf)
    }
}

impl Write for Deadlined<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket
            .set_write_timeout(Some(remaining(self.deadline)?))?;
        let mut socket = self.socket;
        socket.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut socket = self.socket;
        socket.flush()
    }
}

fn transact(socket: &UnixStream, request: &Request, deadline: Instant) -> io::Result<Vec<u8>> {
    let frame = encode_frame(
        &RequestEnvelope {
            protocol: PROTOCOL_VERSION,
            request: request.clone(),
        },
        MAX_REQUEST_BYTES,
    )?;
    let mut wire = Deadlined { socket, deadline };
    wire.write_all(&frame)?;
    wire.flush()?;
    read_frame(&mut wire, MAX_RESPONSE_BYTES)?.ok_or_else(|| {
        // A clean end of stream where the answer belongs is a lost answer, not
        // an empty one: the service never said it did anything.
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the service closed the connection without answering",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The refusal-to-error split, `may_fall_back` and the failure wording are
    // covered by `crate::linux_core_bridge`'s tests, which run on every host.

    #[test]
    fn an_oversized_or_invalid_request_is_refused_before_anything_is_sent() {
        let invalid = Request::ProcessThreads {
            pid: 0,
            start_ticks: 1,
        };
        let error = exchange(&mut Connection::new(), &invalid).expect_err("pid 0 is not a process");
        assert!(matches!(error, ClientError::Failed(_)), "{error}");
        assert!(error.to_string().contains("invalid request"), "{error}");
    }

    #[test]
    fn a_cooldown_answers_without_touching_the_socket() {
        let mut connection = Connection::new();
        assert!(!connection.in_cooldown());
        let error = connection.fail("connect failed".into());
        assert!(connection.in_cooldown());
        assert!(connection.socket.is_none());
        assert!(error.may_fall_back());

        // The cooldown answer names itself instead of paying another connect.
        let cooled = exchange(&mut connection, &Request::Status).expect_err("cooled down");
        assert!(cooled.may_fall_back());
        assert!(cooled.to_string().contains("cooldown"), "{cooled}");
    }
}
