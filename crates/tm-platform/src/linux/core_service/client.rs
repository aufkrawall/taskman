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
//! The connection mutex is held across one exchange, so the longest a second
//! caller waits is one [`CLIENT_DEADLINE_MS`] budget: connect, write and read
//! share one absolute deadline, and a connect that outlives it is abandoned on
//! its own thread instead of holding the lock (a Unix `connect` blocks while a
//! listener's backlog is full). Because a failure immediately starts the
//! cooldown, callers queued behind a wedged broker short-circuit instead of each
//! paying the deadline in turn. The status cache sits behind its own mutex that
//! is never held across IO, so a status row cannot be blocked by a request that
//! is on the wire.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::linux_core_service::{
    CLIENT_DEADLINE_MS, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION, Refusal, Request,
    RequestEnvelope, Response, ResponseEnvelope, SOCKET_PATH, ServiceFailure, ServiceStatusInfo,
    decode_frame, encode_frame, read_frame,
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

/// Why a request did not produce a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// No broker is reachable (not installed, not running, socket missing).
    /// Callers may try their own token.
    Unavailable(String),
    /// The broker answered and refused. Callers must NOT fall back.
    Refused { code: Refusal, detail: String },
    /// The broker answered with a failure of a well-formed request.
    Failed(String),
}

impl ClientError {
    /// Whether a caller may still attempt the operation under its own token.
    ///
    /// Only a broker that never answered leaves the operation open: any answer,
    /// even "I cannot do that", is evidence about this install that a second
    /// privileged path must not paper over.
    pub fn may_fall_back(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    /// The broker's own wire code, when it answered with a refusal.
    pub fn refusal(&self) -> Option<Refusal> {
        match self {
            Self::Refused { code, .. } => Some(*code),
            _ => None,
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(formatter, "core service unavailable: {detail}"),
            Self::Refused { code, detail } => write!(
                formatter,
                "core service refused the request ({}): {detail}",
                code.as_str()
            ),
            Self::Failed(detail) => {
                write!(
                    formatter,
                    "core service could not complete the request: {detail}"
                )
            }
        }
    }
}

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

/// One Status answer with the instant it was taken. `info` is `None` when the
/// broker answered without a status (a refusal, a failure) — that is a real
/// answer about this install and is cached like any other.
struct CachedStatus {
    at: Instant,
    info: Option<ServiceStatusInfo>,
}

static CONNECTION: Mutex<Connection> = Mutex::new(Connection::new());
static STATUS: Mutex<Option<CachedStatus>> = Mutex::new(None);

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

fn status_cache() -> MutexGuard<'static, Option<CachedStatus>> {
    match STATUS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            *guard = None;
            guard
        }
    }
}

/// Send one request through the shared connection.
pub fn request(request: &Request) -> Result<Response, ClientError> {
    let mut connection = connection();
    let response = exchange(&mut connection, request);
    match &response {
        Ok(Response::Status(info)) => {
            *status_cache() = Some(CachedStatus {
                at: Instant::now(),
                info: Some((**info).clone()),
            });
        }
        // A lost answer invalidates the cached report: the broker that was
        // running a moment ago no longer answers.
        Err(ClientError::Unavailable(_)) => *status_cache() = None,
        Ok(_) | Err(_) => {}
    }
    response
}

/// Cached capability report, or `None` when the broker is unreachable.
///
/// One bounded probe per [`STATUS_TTL`]; every other call answers from the
/// cache. The probe needs no privilege, so this is also what an unelevated
/// `--selfcheck`-style diagnostic can use.
pub fn status() -> Option<ServiceStatusInfo> {
    {
        let cache = status_cache();
        if let Some(entry) = cache.as_ref()
            && entry.at.elapsed() < STATUS_TTL
        {
            return entry.info.clone();
        }
    }
    let info = match request(&Request::Status) {
        Ok(Response::Status(info)) => Some(*info),
        // A refusal or a failure is still an answer about this install and is
        // cached as "no broker will serve us" for the same window.
        _ => None,
    };
    if info.is_none() {
        *status_cache() = Some(CachedStatus {
            at: Instant::now(),
            info: None,
        });
    }
    info
}

/// Whether a broker was reachable at the last probe.
///
/// A pure read of the state: it never connects, so a caller that may run on the
/// UI thread (capability reporting) cannot park on a wedged socket. A socket
/// that a transport failure already dropped reports `false`.
pub fn available() -> bool {
    // The cache lock is released before the connection lock is taken, so the
    // two never form a cycle with `request`, which takes them in the other
    // order.
    let answered = {
        let cache = status_cache();
        cache.as_ref().is_some_and(|entry| entry.info.is_some())
    };
    if !answered {
        return false;
    }
    match CONNECTION.try_lock() {
        Ok(connection) => connection.socket.is_some(),
        // Another thread is mid-exchange, which is only possible with a socket.
        Err(_) => true,
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
        Response::Error(failure) => Err(failure_to_error(failure)),
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

/// Map one broker refusal onto [`ClientError`].
///
/// The split is decided by one question: did the service make a decision about
/// the caller or the target, or did it merely fail to carry the request out?
///
/// - `unsupported`, `busy` and `failed` mean nothing was decided: the operation
///   is not implemented, no worker was free, or the action ran into its own OS
///   error. They are reported as a failure with the service's own reason.
/// - everything else is a decision — an unauthorized or unprotected client, a
///   swapped protocol, a malformed request, an action the service rejects, a
///   target whose creation identity no longer matches. Those are
///   [`ClientError::Refused`], which a caller must never work around.
fn failure_to_error(failure: ServiceFailure) -> ClientError {
    let ServiceFailure { code, detail } = failure;
    match code {
        // The detail of a `failed` action already carries the OS error; naming
        // the code again would only repeat "failed: failed".
        Refusal::Failed => ClientError::Failed(detail),
        Refusal::Unsupported | Refusal::Busy => {
            ClientError::Failed(format!("{}: {detail}", code.as_str()))
        }
        code => ClientError::Refused { code, detail },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: Refusal, detail: &str) -> ServiceFailure {
        ServiceFailure {
            code,
            detail: detail.into(),
        }
    }

    #[test]
    fn only_a_lost_answer_leaves_the_operation_open() {
        assert!(ClientError::Unavailable("no socket".into()).may_fall_back());
        for code in [
            Refusal::UnauthorizedUser,
            Refusal::MissingClientImage,
            Refusal::ClientNotListed,
            Refusal::UnprotectedClient,
            Refusal::ProtocolMismatch,
            Refusal::MalformedRequest,
            Refusal::Unsupported,
            Refusal::NotFound,
            Refusal::Rejected,
            Refusal::Busy,
            Refusal::Failed,
        ] {
            let error = failure_to_error(failure(code, "detail"));
            assert!(
                !error.may_fall_back(),
                "{} must never fall back",
                code.as_str()
            );
        }
    }

    #[test]
    fn authorization_answers_are_decisions_about_the_caller() {
        for code in [
            Refusal::UnauthorizedUser,
            Refusal::MissingClientImage,
            Refusal::ClientNotListed,
            Refusal::UnprotectedClient,
            Refusal::ProtocolMismatch,
            Refusal::MalformedRequest,
        ] {
            let error = failure_to_error(failure(code, "because"));
            assert_eq!(error.refusal(), Some(code));
            assert!(matches!(error, ClientError::Refused { .. }), "{code:?}");
        }
    }

    #[test]
    fn action_answers_stay_decisions_and_keep_their_reason() {
        for code in [Refusal::Rejected, Refusal::NotFound] {
            let error = failure_to_error(failure(code, "identity changed"));
            assert_eq!(error.refusal(), Some(code));
            let message = error.to_string();
            assert!(message.contains(code.as_str()), "{message}");
            assert!(message.contains("identity changed"), "{message}");
        }
    }

    #[test]
    fn a_broker_that_could_not_do_it_is_not_a_decision() {
        let unsupported = failure_to_error(failure(Refusal::Unsupported, "no such operation"));
        assert!(matches!(unsupported, ClientError::Failed(_)));
        assert!(
            unsupported.to_string().contains("unsupported operation"),
            "{unsupported}"
        );
        assert!(
            unsupported.to_string().contains("no such operation"),
            "{unsupported}"
        );

        let busy = failure_to_error(failure(Refusal::Busy, "worker pool full"));
        assert!(
            busy.to_string().contains("busy: worker pool full"),
            "{busy}"
        );

        // The action's own OS error is reported verbatim instead of being
        // dressed up as a code.
        let failed = failure_to_error(failure(Refusal::Failed, "EPERM: not permitted"));
        assert_eq!(failed, ClientError::Failed("EPERM: not permitted".into()));
        assert!(failed.to_string().contains("EPERM"), "{failed}");
    }

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
