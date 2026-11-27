//! Pure decision and mapping logic for the Linux core-service bridge.
//!
//! Compiled on every host (`cfg(any(target_os = "linux", test))`) on purpose:
//! these decisions are the security-relevant part of the GUI-side bridge —
//! which failures may still be attempted under another token, how a broker
//! answer becomes the model types the tabs already render, and what the
//! capability state means — so their tests must run in the ordinary gate, not
//! only on a Linux target.
//!
//! The socket transport lives in [`crate::linux::core_service::client`] and is
//! Linux-only; nothing here may touch a socket, `/proc`, or any platform API.

use crate::actions::{
    ConnectionProtocol, CoreServiceState, FileIoAccounting, FileIoAccountingState,
    ProcessConnection, ProcessFileIo, ProcessFileIoSnapshot, ProcessModule, ProcessThread,
    ProcessThreads, ServiceAction, TcpState, ThreadState,
};
use crate::linux_core_service::{
    ACTION_DEADLINE_MS, CLIENT_DEADLINE_MS, Refusal, Request, Response, ServiceFailure,
    ServiceStatusInfo, ServiceVerb, WireConnection, WireFileIo, WireFileIoEntry, WireModule,
    WireThread, WireThreadState, WireThreads,
};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tm_core::error::TmError;

/// Why a request did not produce a response.
///
/// The distinction is the fallback contract: only [`Self::Unavailable`] means
/// "no broker answered, try your own token"; a refusal or a failure is an
/// answer, and repeating the operation elsewhere would turn a safety decision
/// into permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// No broker is reachable (not installed, not running, socket missing), or a
    /// READ ran past its deadline. A fully delivered action never lands here —
    /// see [`Self::Failed`] — because it may already have run.
    Unavailable(String),
    /// The broker answered and refused. Callers must NOT fall back.
    Refused { code: Refusal, detail: String },
    /// The broker answered a well-formed request with a failure, or a delivered
    /// request's outcome could not be determined. Never retried.
    Failed(String),
}

impl ClientError {
    /// Whether a caller may still attempt the operation under its own token.
    ///
    /// A fully delivered action never reports [`Self::Unavailable`], so this
    /// predicate cannot authorize a second mutation; see
    /// [`may_retry_elsewhere`].
    pub fn may_fall_back(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    /// The refusal code when the broker refused.
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
            Self::Refused { code, detail } => {
                write!(
                    formatter,
                    "core service refused ({}): {detail}",
                    code.as_str()
                )
            }
            Self::Failed(detail) => write!(formatter, "core service failed: {detail}"),
        }
    }
}

/// Short label naming the payload a broker answer carried.
pub(crate) fn response_label(response: &Response) -> &'static str {
    match response {
        Response::Status(_) => "status",
        Response::Sensors(_) => "sensors",
        Response::Modules(_) => "modules",
        Response::Threads(_) => "threads",
        Response::Connections(_) => "connections",
        Response::FileIo(_) => "file-io",
        Response::Evidence(_) => "evidence",
        Response::Telemetry(_) => "telemetry",
        Response::Done => "done",
        Response::Error(_) => "error",
    }
}

/// Turn a broker failure into the error the action surface reports.
///
/// The broker's own message is preserved: it names what the service checked
/// and refused, which is the only way a user can tell "not permitted" from
/// "the target changed under us" without guesswork.
pub(crate) fn failure_to_error(context: &'static str, failure: &ServiceFailure) -> TmError {
    TmError::platform(
        context,
        format!("core service {}: {}", failure.code.as_str(), failure.detail),
    )
}

/// Turn any broker failure into the error a caller reports for one operation.
///
/// `context` is the operation label (`Request::label`), so the error names the
/// verb that failed instead of a generic "process action".
pub(crate) fn client_error(context: &'static str, error: &ClientError) -> TmError {
    match error {
        ClientError::Refused { code, detail } => failure_to_error(
            context,
            &ServiceFailure {
                code: *code,
                detail: detail.clone(),
            },
        ),
        other => TmError::platform(context, other.to_string()),
    }
}

/// Map one broker error response onto [`ClientError`].
///
/// The split is decided by one question: did the service make a decision about
/// the caller or the target, or did it merely fail to carry the request out?
///
/// - `unsupported`, `busy` and `failed` decided nothing: the operation is not
///   implemented, no worker was free, or the action ran into its own OS error.
///   They are a failure with the service's own reason.
/// - everything else is a decision — an unauthorized or unprotected client, a
///   swapped protocol, a malformed request, an action the service rejects, a
///   target whose creation identity no longer matches. Those are
///   [`ClientError::Refused`], which a caller must never work around.
pub(crate) fn service_failure_to_client_error(failure: ServiceFailure) -> ClientError {
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

/// The wire verb for one service action.
pub(crate) fn service_verb(action: ServiceAction) -> ServiceVerb {
    match action {
        ServiceAction::Start => ServiceVerb::Start,
        ServiceAction::Stop => ServiceVerb::Stop,
        ServiceAction::Restart => ServiceVerb::Restart,
    }
}

/// Files one process has open, mapped from the broker's answer.
///
/// `/proc/<pid>/fd` and `/proc/<pid>/io` are owner-only, so another user's
/// process needs the broker for the rows themselves. The accounting block is
/// decided by what the answer does NOT carry, and every field errs toward
/// "unknown":
///
/// - `state` stays [`FileIoAccountingState::Unavailable`]: Linux has no
///   per-file trace, so these rows are not the measured request accounting that
///   state describes.
/// - `counter_read_bytes`/`counter_write_bytes` stay `None`: the protocol
///   reports files, not the process-wide totals, and summing the rows would
///   silently drop every byte that went to a pipe, a socket or a device.
/// - `handles_complete` stays `false`: the wire has no completeness flag, so the
///   weaker fact is stated — the enumeration's coverage is not part of the
///   answer — instead of claiming a complete handle table.
pub(crate) fn file_io_from_wire(wire: WireFileIo) -> ProcessFileIoSnapshot {
    ProcessFileIoSnapshot {
        files: wire
            .files
            .into_iter()
            .map(file_io_entry_from_wire)
            .collect(),
        accounting: FileIoAccounting {
            state: FileIoAccountingState::Unavailable,
            counter_read_bytes: None,
            counter_write_bytes: None,
            handles_complete: false,
            // Every other counter belongs to a trace that does not exist here.
            ..FileIoAccounting::default()
        },
    }
}

fn file_io_entry_from_wire(wire: WireFileIoEntry) -> ProcessFileIo {
    ProcessFileIo {
        path: wire.path,
        read_bytes: wire.read_bytes,
        write_bytes: wire.write_bytes,
        read_ops: wire.read_ops,
        write_ops: wire.write_ops,
        file_size: wire.file_size,
        offset: wire.offset,
        access_known: wire.access_known,
        is_read: wire.is_read,
        is_write: wire.is_write,
        open_handles: wire.open_handles,
    }
}

/// How long one request may block before its answer is treated as missing.
///
/// A privileged action can legitimately run for tens of seconds — a
/// `systemctl restart` is bounded by the service's own 30 s control deadline —
/// and giving up earlier would classify a still-running mutation as
/// [`ClientError::Unavailable`], the one fallback-eligible class, and invite a
/// retry of something that already started. Reads answer in milliseconds and
/// must never park a sampling tick.
pub(crate) fn request_deadline(request: &Request) -> Duration {
    let millis = match request {
        Request::Action { .. } => ACTION_DEADLINE_MS,
        _ => CLIENT_DEADLINE_MS,
    };
    Duration::from_millis(millis)
}

/// Whether a failed exchange may still be attempted under another token.
///
/// A read can always be repeated: another identity only widens who may look. A
/// privileged action cannot, once its frame is on the wire — the service may
/// have executed it, so "I never got an answer" must be reported as a final
/// failure instead of becoming a second mutation.
pub(crate) fn may_retry_elsewhere(request: &Request, frame_written: bool) -> bool {
    !(frame_written && matches!(request, Request::Action { .. }))
}

/// What the client knows about the broker right now.
///
/// A status probe is asynchronous, so "no answer yet" and "the broker did not
/// answer" are different facts and must not share a state: the first is a
/// pending probe, the second is a completed negative. Collapsing them is what
/// made a healthy, running broker read as "stopped" until the first probe
/// landed.
///
/// Public (and re-exported by the client) because the client's `status()`
/// returns it: a public function cannot expose a crate-private type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusProbe {
    /// No probe has completed yet. The answer is still being fetched.
    Pending,
    /// A probe finished and produced this outcome.
    Answered(Result<ServiceStatusInfo, ClientError>),
}

/// The cache state one status read observed.
pub(crate) enum StatusCache {
    /// The last completed answer, and how long ago it was taken.
    Answered(Result<ServiceStatusInfo, ClientError>, Duration),
    /// No probe has completed yet.
    Empty,
    /// The cache could not be read: a probe is replacing the entry right now.
    Unreadable,
}

/// What one status read should do, decided from [`StatusCache`] alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusDecision {
    /// Report the cached answer; nothing needs to happen.
    Answer(Result<ServiceStatusInfo, ClientError>),
    /// Report the cached answer and refresh it behind the caller.
    AnswerAndProbe(Result<ServiceStatusInfo, ClientError>),
    /// Nothing usable is cached: start a probe and report it as pending.
    Probe,
}

/// Decide what one status read does.
///
/// The rules are deliberately boring, because the interesting part is what they
/// avoid: an expired answer is still an answer (a version does not change
/// between two frames), and an unreadable cache is *not* an answer — reporting
/// it as "no broker" would be a claim nobody made.
pub(crate) fn status_decision(cache: StatusCache, ttl: Duration) -> StatusDecision {
    match cache {
        StatusCache::Answered(result, age) if age < ttl => StatusDecision::Answer(result),
        StatusCache::Answered(result, _) => StatusDecision::AnswerAndProbe(result),
        StatusCache::Empty | StatusCache::Unreadable => StatusDecision::Probe,
    }
}

/// Classify the broker for the Settings surface.
///
/// The whole mapping lives here so it is decided once and table-tested:
///
/// - a missing manifest means the installer never ran: `NotInstalled`;
/// - a probe that has not finished yet is `Starting`, never `Stopped`;
/// - a live Status answer is `Running` with the service's own version;
/// - a completed probe with no answer is `Stopped`, because the manifest proves
///   the install and starting the unit is the fix;
/// - a refusal that names this client or this user (`unauthorized-user`,
///   `missing-client-image`, `client-not-listed`, `unprotected-client`) is
///   `ForeignClient`: reinstall or reauthorize this binary, do not start a
///   service that is running fine and will keep refusing;
/// - every other refusal and every failure are `Degraded` in the service's own
///   words, with the code's name only when the service gave no detail.
///
/// A missing *socket* is deliberately `Stopped` rather than `NotInstalled`: only
/// systemd knows whether the unit exists, and "install" would offer the wrong
/// remedy for a service that is simply not running.
pub(crate) fn core_service_state(manifest_installed: bool, probe: StatusProbe) -> CoreServiceState {
    if !manifest_installed {
        return CoreServiceState::NotInstalled;
    }
    match probe {
        StatusProbe::Pending => CoreServiceState::Starting,
        StatusProbe::Answered(Ok(info)) => CoreServiceState::Running {
            version: info.service_version,
        },
        StatusProbe::Answered(Err(ClientError::Unavailable(_))) => CoreServiceState::Stopped,
        StatusProbe::Answered(Err(ClientError::Failed(detail))) => {
            CoreServiceState::Degraded(detail)
        }
        StatusProbe::Answered(Err(ClientError::Refused { code, detail })) => match code {
            Refusal::UnauthorizedUser
            | Refusal::MissingClientImage
            | Refusal::ClientNotListed
            | Refusal::UnprotectedClient => CoreServiceState::ForeignClient,
            // Every other refusal keeps the service's own words; an empty
            // detail would render as a blank state, so the code names itself.
            _ if detail.is_empty() => CoreServiceState::Degraded(code.as_str().to_string()),
            _ => CoreServiceState::Degraded(detail),
        },
    }
}

/// The broker's rows with the counters only the direct read could measure.
///
/// The wire reports files, not `/proc/<pid>/io` totals, so the local counters
/// are the only source for them: a brokered answer must not blank a number the
/// GUI could already read. Nothing is ever filled in the other direction — a
/// value the direct read did not have stays unknown.
pub(crate) fn merge_file_io(
    brokered: ProcessFileIoSnapshot,
    direct: &ProcessFileIoSnapshot,
) -> ProcessFileIoSnapshot {
    let mut merged = brokered;
    merged.accounting.counter_read_bytes = direct.accounting.counter_read_bytes;
    merged.accounting.counter_write_bytes = direct.accounting.counter_write_bytes;
    merged
}

pub(crate) fn module_from_wire(wire: WireModule) -> ProcessModule {
    ProcessModule {
        name: wire.name,
        path: wire.path,
        base_address: wire.base_address,
        size_bytes: wire.size_bytes,
    }
}

pub(crate) fn threads_from_wire(wire: WireThreads) -> ProcessThreads {
    let base = wire.process_base_priority;
    ProcessThreads {
        process_base_priority: base,
        threads: wire
            .threads
            .into_iter()
            .map(|thread| thread_from_wire(thread, base))
            .collect(),
    }
}

fn thread_from_wire(wire: WireThread, process_base_priority: i32) -> ProcessThread {
    ProcessThread {
        tid: wire.tid,
        // Procfs exposes no thread entry routine on either path.
        start_address: None,
        priority: wire.priority,
        // The baseline is the thread's own nice value; when the service could
        // not read one, the thread group's baseline stands in rather than a
        // fabricated zero.
        base_priority: wire.nice.unwrap_or(process_base_priority),
        nice: wire.nice,
        context_switches: wire.context_switches,
        kernel_time_100ns: wire.kernel_time_100ns,
        user_time_100ns: wire.user_time_100ns,
        created_epoch_s: wire.created_epoch_s,
        state: thread_state_from_wire(wire.state),
        // Linux wait channels are names/addresses, not KWAIT_REASON ids.
        wait_reason: None,
        cycles: wire.cycles,
    }
}

/// The same Linux state letters, mapped onto the model's vocabulary.
///
/// `Running` collapses onto `Runnable` because Linux' `R` covers both on-CPU and
/// queued threads, exactly as the direct `/proc` reader maps it. An unnamed
/// kernel state keeps its raw byte, so a brokered row renders the same evidence
/// a direct read would.
fn thread_state_from_wire(state: WireThreadState) -> ThreadState {
    match state {
        WireThreadState::Running => ThreadState::Runnable,
        WireThreadState::Sleeping => ThreadState::Sleeping,
        WireThreadState::DiskSleep => ThreadState::Waiting,
        WireThreadState::Stopped | WireThreadState::Tracing => ThreadState::Stopped,
        WireThreadState::Zombie | WireThreadState::Dead => ThreadState::Terminated,
        WireThreadState::Idle => ThreadState::Idle,
        WireThreadState::Parked => ThreadState::Parked,
        WireThreadState::Other(raw) => ThreadState::Other(u32::from(raw)),
    }
}

/// Connection rows the model can carry, plus the endpoints it cannot parse.
///
/// An unparsable row is returned instead of being folded into an empty list: a
/// dropped row is a diagnostic, and "no connections" is a measurement nobody
/// made. A Unix row is not a defect and is not listed here — the model has no
/// way to express one, and the direct reader lists the IP tables only.
pub(crate) struct ConnectionMapping {
    pub(crate) connections: Vec<ProcessConnection>,
    /// The raw endpoint text of every row that breaks the pinned address format.
    pub(crate) unparsable: Vec<String>,
}

pub(crate) fn connections_from_wire(wire: Vec<WireConnection>) -> ConnectionMapping {
    let mut connections = Vec::with_capacity(wire.len());
    let mut unparsable = Vec::new();
    for row in wire {
        match connection_from_wire(row) {
            RowMapping::Mapped(connection) => connections.push(connection),
            // A Unix endpoint is a protocol this model does not carry; the
            // direct reader lists the four IP tables only, so this is the same
            // vocabulary, not a defect.
            RowMapping::NotExpressible => {}
            RowMapping::Unparsable(endpoint) => unparsable.push(endpoint),
        }
    }
    ConnectionMapping {
        connections,
        unparsable,
    }
}

/// What one wire row became.
enum RowMapping {
    Mapped(ProcessConnection),
    /// A protocol this model cannot carry, dropped like the direct reader.
    NotExpressible,
    /// An IP endpoint that breaks the pinned format: a protocol defect.
    Unparsable(String),
}

/// Map one row, or name the endpoint that could not be mapped.
fn connection_from_wire(wire: WireConnection) -> RowMapping {
    let protocol = match wire.protocol.as_str() {
        "tcp" => ConnectionProtocol::Tcp,
        "tcp6" => ConnectionProtocol::Tcp6,
        "udp" => ConnectionProtocol::Udp,
        "udp6" => ConnectionProtocol::Udp6,
        // A Unix endpoint has no `SocketAddr`, which is what this model carries.
        _ => return RowMapping::NotExpressible,
    };
    let Some(local) = parse_socket_addr(&wire.local) else {
        return RowMapping::Unparsable(wire.local);
    };
    // Only TCP has a state; a datagram endpoint has none, exactly as the direct
    // `/proc/net/udp*` walk leaves it unset.
    let state = if matches!(protocol, ConnectionProtocol::Tcp | ConnectionProtocol::Tcp6) {
        wire.state.as_deref().and_then(tcp_state_from_wire)
    } else {
        None
    };
    let remote = match wire.remote.as_deref() {
        Some(text) => {
            // A peer the row cannot name is an unparsable address, and the
            // direct reader drops that row rather than listing half of it.
            let Some(remote) = parse_socket_addr(text) else {
                return RowMapping::Unparsable(text.to_string());
            };
            named_remote(state, remote).then_some(remote)
        }
        None => None,
    };
    RowMapping::Mapped(ProcessConnection {
        protocol,
        local,
        remote,
        state,
    })
}

/// A peer the model can name: not a listener, not the all-zero placeholder the
/// kernel writes into a socket that was never connected.
fn named_remote(state: Option<TcpState>, remote: SocketAddr) -> bool {
    state != Some(TcpState::Listen) && remote.port() != 0 && !remote.ip().is_unspecified()
}

/// Parse one endpoint the broker wrote as a string.
///
/// The contract pins `SocketAddr`'s own grammar (`1.2.3.4:80`, `[::1]:80`); an
/// unbracketed IPv6 pair is accepted as well, because a formatter that prints
/// address and port separately produces exactly that.
fn parse_socket_addr(text: &str) -> Option<SocketAddr> {
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Some(addr);
    }
    let (address, port) = text.rsplit_once(':')?;
    let ip = address.parse::<IpAddr>().ok()?;
    Some(SocketAddr::new(ip, port.parse().ok()?))
}

/// Map the broker's state name onto the model's vocabulary.
///
/// The wire spells enum variants in kebab-case, which is what `TcpState`'s own
/// names become; an unknown name is not guessed at, because a row can show its
/// endpoint without inventing a state for it.
fn tcp_state_from_wire(name: &str) -> Option<TcpState> {
    let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
    Some(match normalized.as_str() {
        "closed" => TcpState::Closed,
        "listen" | "listening" => TcpState::Listen,
        "syn-sent" => TcpState::SynSent,
        "syn-received" | "syn-recv" => TcpState::SynReceived,
        "established" => TcpState::Established,
        "fin-wait-1" => TcpState::FinWait1,
        "fin-wait-2" => TcpState::FinWait2,
        "close-wait" => TcpState::CloseWait,
        "closing" => TcpState::Closing,
        "last-ack" => TcpState::LastAck,
        "time-wait" => TcpState::TimeWait,
        "delete-tcb" => TcpState::DeleteTcb,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux_core_service::{
        ActionRequest, ProcessEvidence, ServiceStatusInfo, WireConnection, WireFileIo, WireModule,
        WireThreads,
    };
    use tm_core::model::PriorityClass;

    fn failure(code: Refusal, detail: &str) -> ServiceFailure {
        ServiceFailure {
            code,
            detail: detail.into(),
        }
    }

    fn status_info() -> ServiceStatusInfo {
        ServiceStatusInfo {
            protocol: crate::linux_core_service::PROTOCOL_VERSION,
            service_version: "0.1.34".into(),
            pid: 1,
            uid: 0,
            started_unix_s: 0,
            uptime_s: 0,
            socket: crate::linux_core_service::SOCKET_PATH.into(),
            authorized_uids: vec![1000],
            sensors_available: false,
            nvml_available: false,
            install_verified: true,
            last_sensor_sample_unix_ms: 0,
            requests_served: 0,
            refusals: 0,
        }
    }

    #[test]
    fn only_a_missing_broker_may_fall_back() {
        assert!(ClientError::Unavailable("no socket".into()).may_fall_back());
        for error in [
            ClientError::Refused {
                code: Refusal::UnauthorizedUser,
                detail: "uid 1001".into(),
            },
            ClientError::Refused {
                code: Refusal::ClientNotListed,
                detail: "/tmp/taskman".into(),
            },
            ClientError::Failed("setpriority failed".into()),
        ] {
            assert!(!error.may_fall_back(), "{error:?} must not fall back");
        }
    }

    #[test]
    fn a_refusal_keeps_its_code_and_an_unavailable_does_not() {
        assert_eq!(
            ClientError::Refused {
                code: Refusal::NotFound,
                detail: "identity changed".into(),
            }
            .refusal(),
            Some(Refusal::NotFound)
        );
        assert_eq!(ClientError::Unavailable("gone".into()).refusal(), None);
        assert_eq!(ClientError::Failed("boom".into()).refusal(), None);
    }

    #[test]
    fn every_payload_has_a_distinct_label() {
        let labels = [
            response_label(&Response::Status(Box::new(status_info()))),
            response_label(&Response::Sensors(Box::default())),
            response_label(&Response::Modules(Vec::<WireModule>::new())),
            response_label(&Response::Threads(WireThreads {
                process_base_priority: 0,
                threads: Vec::new(),
            })),
            response_label(&Response::Connections(Vec::<WireConnection>::new())),
            response_label(&Response::FileIo(Box::new(WireFileIo {
                files: Vec::new(),
            }))),
            response_label(&Response::Evidence(Box::new(ProcessEvidence {
                pid: 1,
                start_ticks: 0,
                name: String::new(),
                exe: None,
                cwd: None,
                root: None,
                uid: None,
                gid: None,
                euid: None,
                egid: None,
                state: None,
                threads: None,
                capability_effective: None,
                capability_permitted: None,
                capability_bounding: None,
                seccomp_mode: None,
                no_new_privs: None,
                dumpable: None,
                oom_score: None,
                oom_score_adj: None,
                rss_bytes: None,
                pss_bytes: None,
                swap_bytes: None,
                open_fds: None,
                namespaces: Vec::new(),
                cgroup: None,
                security_label: None,
                limits: Vec::new(),
            }))),
            response_label(&Response::Done),
            response_label(&Response::Error(failure(Refusal::Busy, "queue full"))),
        ];
        let mut unique = std::collections::BTreeSet::new();
        for label in labels {
            assert!(!label.is_empty());
            assert!(unique.insert(label), "{label} is not distinct");
        }
    }

    #[test]
    fn a_broker_failure_names_the_refusal_and_the_detail() {
        let error = failure_to_error(
            "end task",
            &failure(Refusal::UnprotectedClient, "image is writable"),
        );
        let text = error.to_string();
        assert!(text.contains("end task"), "{text}");
        assert!(text.contains(Refusal::UnprotectedClient.as_str()), "{text}");
        assert!(text.contains("image is writable"), "{text}");
        assert!(matches!(error, TmError::Platform { .. }));
    }

    #[test]
    fn a_refusal_that_decided_nothing_is_a_failure() {
        assert_eq!(
            service_failure_to_client_error(failure(Refusal::Unsupported, "no such operation")),
            ClientError::Failed("unsupported operation: no such operation".into())
        );
        assert_eq!(
            service_failure_to_client_error(failure(Refusal::Busy, "pool full")),
            ClientError::Failed("busy: pool full".into())
        );
        // The action's own OS error is reported verbatim, not dressed as a code.
        assert_eq!(
            service_failure_to_client_error(failure(Refusal::Failed, "EPERM")),
            ClientError::Failed("EPERM".into())
        );
    }

    #[test]
    fn a_refusal_that_decided_something_stays_a_decision() {
        for code in [
            Refusal::UnauthorizedUser,
            Refusal::MissingClientImage,
            Refusal::ClientNotListed,
            Refusal::UnprotectedClient,
            Refusal::ProtocolMismatch,
            Refusal::MalformedRequest,
            Refusal::NotFound,
            Refusal::Rejected,
        ] {
            let error = service_failure_to_client_error(failure(code, "identity changed"));
            assert_eq!(
                error,
                ClientError::Refused {
                    code,
                    detail: "identity changed".into()
                },
                "{}",
                code.as_str()
            );
            assert!(!error.may_fall_back(), "{}", code.as_str());
        }
    }

    #[test]
    fn a_client_error_names_the_operation_the_broker_and_its_reason() {
        let refused = ClientError::Refused {
            code: Refusal::Rejected,
            detail: "creation identity changed".into(),
        };
        let text = client_error("kill", &refused).to_string();
        assert!(text.contains("kill"), "{text}");
        assert!(text.contains(Refusal::Rejected.as_str()), "{text}");
        assert!(text.contains("creation identity changed"), "{text}");

        let lost = client_error(
            "process-modules",
            &ClientError::Unavailable("no socket".into()),
        );
        let text = lost.to_string();
        assert!(text.contains("process-modules"), "{text}");
        assert!(text.contains("no socket"), "{text}");
    }

    #[test]
    fn service_verbs_follow_the_action() {
        assert_eq!(service_verb(ServiceAction::Start), ServiceVerb::Start);
        assert_eq!(service_verb(ServiceAction::Stop), ServiceVerb::Stop);
        assert_eq!(service_verb(ServiceAction::Restart), ServiceVerb::Restart);
    }

    fn every_action() -> Vec<Request> {
        vec![
            Request::Action {
                action: ActionRequest::Kill {
                    pid: 42,
                    start_ticks: 1,
                    tree: false,
                },
            },
            Request::Action {
                action: ActionRequest::Kill {
                    pid: 42,
                    start_ticks: 1,
                    tree: true,
                },
            },
            Request::Action {
                action: ActionRequest::Suspend {
                    pid: 42,
                    start_ticks: 1,
                },
            },
            Request::Action {
                action: ActionRequest::Resume {
                    pid: 42,
                    start_ticks: 1,
                },
            },
            Request::Action {
                action: ActionRequest::SetPriority {
                    pid: 42,
                    start_ticks: 1,
                    priority: PriorityClass::High,
                },
            },
            Request::Action {
                action: ActionRequest::SetAffinity {
                    pid: 42,
                    start_ticks: 1,
                    mask: 0b11,
                },
            },
            Request::Action {
                action: ActionRequest::Service {
                    unit: "sshd.service".into(),
                    verb: ServiceVerb::Restart,
                },
            },
        ]
    }

    fn every_read() -> Vec<Request> {
        vec![
            Request::Status,
            Request::Sensors,
            Request::ProcessModules {
                pid: 42,
                start_ticks: 1,
            },
            Request::ProcessThreads {
                pid: 42,
                start_ticks: 1,
            },
            Request::ProcessConnections {
                pid: 42,
                start_ticks: 1,
            },
            Request::ProcessFileIo {
                pid: 42,
                start_ticks: 1,
            },
            Request::ProcessEvidence {
                pid: 42,
                start_ticks: 1,
            },
        ]
    }

    #[test]
    fn an_action_gets_the_long_deadline_and_a_read_keeps_the_short_one() {
        // A `systemctl restart` outlives the read deadline by design, so the
        // two must never be conflated: giving up on a running action is what
        // turns it into a retryable "unavailable".
        const { assert!(ACTION_DEADLINE_MS > CLIENT_DEADLINE_MS) };
        for request in every_action() {
            assert_eq!(
                request_deadline(&request),
                Duration::from_millis(ACTION_DEADLINE_MS),
                "{request:?}"
            );
        }
        for request in every_read() {
            assert_eq!(
                request_deadline(&request),
                Duration::from_millis(CLIENT_DEADLINE_MS),
                "{request:?}"
            );
        }
    }

    #[test]
    fn a_written_action_frame_is_never_retried_elsewhere() {
        for request in every_action() {
            assert!(
                may_retry_elsewhere(&request, false),
                "an action that was never sent may still use the local path: {request:?}"
            );
            assert!(
                !may_retry_elsewhere(&request, true),
                "an action whose frame reached the service is final: {request:?}"
            );
        }
        for request in every_read() {
            assert!(may_retry_elsewhere(&request, false), "{request:?}");
            assert!(
                may_retry_elsewhere(&request, true),
                "a read may always be repeated: {request:?}"
            );
        }
    }

    #[test]
    fn a_status_read_answers_from_the_cache_or_asks_for_a_probe() {
        let ttl = Duration::from_secs(10);
        let fresh = Ok(status_info());
        let old = Err(ClientError::Unavailable("no socket".into()));
        let cases: Vec<(StatusCache, StatusDecision)> = vec![
            // A fresh answer is reported without touching the wire.
            (
                StatusCache::Answered(fresh.clone(), Duration::from_secs(0)),
                StatusDecision::Answer(fresh.clone()),
            ),
            (
                StatusCache::Answered(fresh.clone(), Duration::from_secs(9)),
                StatusDecision::Answer(fresh.clone()),
            ),
            // Expired is still an answer, and it refreshes behind the caller.
            (
                StatusCache::Answered(fresh.clone(), ttl),
                StatusDecision::AnswerAndProbe(fresh.clone()),
            ),
            (
                StatusCache::Answered(fresh.clone(), Duration::from_secs(3_600)),
                StatusDecision::AnswerAndProbe(fresh.clone()),
            ),
            // A cached negative expires on the same rule, so a broker that
            // appears later is picked up.
            (
                StatusCache::Answered(old.clone(), Duration::from_secs(60)),
                StatusDecision::AnswerAndProbe(old.clone()),
            ),
            // Nothing has been answered, or the answer cannot be read right
            // now: probe, and report it as pending rather than as a negative.
            (StatusCache::Empty, StatusDecision::Probe),
            (StatusCache::Unreadable, StatusDecision::Probe),
        ];
        for (cache, expected) in cases {
            assert_eq!(status_decision(cache, ttl), expected);
        }
    }

    #[test]
    fn a_brokered_file_list_keeps_the_counters_only_the_direct_read_had() {
        let direct = ProcessFileIoSnapshot {
            files: Vec::new(),
            accounting: FileIoAccounting {
                counter_read_bytes: Some(4096),
                counter_write_bytes: Some(512),
                handles_complete: false,
                ..FileIoAccounting::default()
            },
        };
        let brokered = file_io_from_wire(WireFileIo { files: Vec::new() });
        let merged = merge_file_io(brokered, &direct);
        assert_eq!(merged.accounting.counter_read_bytes, Some(4096));
        assert_eq!(merged.accounting.counter_write_bytes, Some(512));
        // Nothing is invented in the other direction.
        assert_eq!(merged.accounting.state, FileIoAccountingState::Unavailable);
        assert!(!merged.accounting.handles_complete);
        assert_eq!(merged.accounting.events_lost, None);

        // A direct read that could not measure them leaves them unknown.
        let empty = ProcessFileIoSnapshot::default();
        let merged = merge_file_io(file_io_from_wire(WireFileIo { files: Vec::new() }), &empty);
        assert_eq!(merged.accounting.counter_read_bytes, None);
        assert_eq!(merged.accounting.counter_write_bytes, None);
    }

    #[test]
    fn the_core_service_state_mapping_is_decided_in_one_place() {
        let ok = || StatusProbe::Answered(Ok(status_info()));
        let refused = |code: Refusal| {
            StatusProbe::Answered(Err(ClientError::Refused {
                code,
                detail: "service said why".into(),
            }))
        };
        let unavailable =
            || StatusProbe::Answered(Err(ClientError::Unavailable("no socket".into())));
        let cases: Vec<(bool, StatusProbe, CoreServiceState)> = vec![
            // A manifest-less machine is not installed, whatever arrived.
            (false, ok(), CoreServiceState::NotInstalled),
            (false, unavailable(), CoreServiceState::NotInstalled),
            (false, StatusProbe::Pending, CoreServiceState::NotInstalled),
            // A probe that has not finished is NOT a negative: the row says
            // starting, so a healthy broker is never painted as stopped.
            (true, StatusProbe::Pending, CoreServiceState::Starting),
            // A live answer is the only running evidence, and it carries the
            // version the service reports.
            (
                true,
                ok(),
                CoreServiceState::Running {
                    version: "0.1.34".into(),
                },
            ),
            // A completed probe with no answer: starting the unit is the fix.
            (true, unavailable(), CoreServiceState::Stopped),
            // A refusal that names this binary or this user is not a start
            // problem: reinstalling or reauthorizing is.
            (
                true,
                refused(Refusal::UnauthorizedUser),
                CoreServiceState::ForeignClient,
            ),
            (
                true,
                refused(Refusal::MissingClientImage),
                CoreServiceState::ForeignClient,
            ),
            (
                true,
                refused(Refusal::ClientNotListed),
                CoreServiceState::ForeignClient,
            ),
            (
                true,
                refused(Refusal::UnprotectedClient),
                CoreServiceState::ForeignClient,
            ),
            // Any other refusal keeps the service's own words, including the
            // protocol one.
            (
                true,
                StatusProbe::Answered(Err(ClientError::Refused {
                    code: Refusal::ProtocolMismatch,
                    detail: "service speaks protocol 2".into(),
                })),
                CoreServiceState::Degraded("service speaks protocol 2".into()),
            ),
            (
                true,
                refused(Refusal::Rejected),
                CoreServiceState::Degraded("service said why".into()),
            ),
            (
                true,
                StatusProbe::Answered(Err(ClientError::Failed("EPERM".into()))),
                CoreServiceState::Degraded("EPERM".into()),
            ),
            // An empty detail would render as a blank state, so the code speaks.
            (
                true,
                StatusProbe::Answered(Err(ClientError::Refused {
                    code: Refusal::ProtocolMismatch,
                    detail: String::new(),
                })),
                CoreServiceState::Degraded("protocol mismatch".into()),
            ),
            (
                true,
                StatusProbe::Answered(Err(ClientError::Refused {
                    code: Refusal::Busy,
                    detail: String::new(),
                })),
                CoreServiceState::Degraded("busy".into()),
            ),
        ];
        for (manifest_installed, probe, expected) in cases {
            assert_eq!(
                core_service_state(manifest_installed, probe),
                expected,
                "manifest={manifest_installed}"
            );
        }
    }

    #[test]
    fn a_brokered_module_keeps_the_same_identity_fields() {
        let module = module_from_wire(WireModule {
            name: "libc.so.6".into(),
            path: "/usr/lib/libc.so.6".into(),
            base_address: 0x7f00_0000,
            size_bytes: 0x2000,
        });
        assert_eq!(module.name, "libc.so.6");
        assert_eq!(module.path, "/usr/lib/libc.so.6");
        assert_eq!(module.base_address, 0x7f00_0000);
        assert_eq!(module.size_bytes, 0x2000);
    }

    #[test]
    fn brokered_file_io_claims_only_what_the_answer_carries() {
        let snapshot = file_io_from_wire(WireFileIo {
            files: vec![
                WireFileIoEntry {
                    path: "/var/log/syslog".into(),
                    read_bytes: Some(4096),
                    write_bytes: None,
                    read_ops: Some(2),
                    write_ops: None,
                    file_size: Some(8192),
                    offset: Some(4096),
                    access_known: true,
                    is_read: true,
                    is_write: false,
                    open_handles: 1,
                },
                WireFileIoEntry {
                    path: "/tmp/scratch".into(),
                    read_bytes: None,
                    write_bytes: Some(512),
                    read_ops: None,
                    write_ops: None,
                    file_size: None,
                    offset: None,
                    access_known: false,
                    is_read: false,
                    is_write: true,
                    open_handles: 2,
                },
            ],
        });
        assert_eq!(snapshot.files.len(), 2);
        assert_eq!(snapshot.files[0].path, "/var/log/syslog");
        assert_eq!(snapshot.files[0].read_bytes, Some(4096));
        assert_eq!(
            snapshot.files[1].read_bytes, None,
            "an unmeasured counter must stay unknown, never zero"
        );
        assert_eq!(snapshot.files[1].write_bytes, Some(512));
        assert_eq!(snapshot.files[1].open_handles, 2);
        // The wire carries files, not a trace and not process totals.
        assert_eq!(
            snapshot.accounting.state,
            FileIoAccountingState::Unavailable
        );
        assert_eq!(snapshot.accounting.counter_read_bytes, None);
        assert_eq!(snapshot.accounting.counter_write_bytes, None);
        assert!(!snapshot.accounting.handles_complete);
        assert_eq!(snapshot.accounting.events_lost, None);
    }

    fn wire_thread(tid: u32, state: WireThreadState) -> crate::linux_core_service::WireThread {
        crate::linux_core_service::WireThread {
            tid,
            priority: 20,
            nice: Some(-5),
            context_switches: Some(9),
            kernel_time_100ns: 11,
            user_time_100ns: 12,
            created_epoch_s: Some(1_700_000_123),
            state,
            cycles: Some(13),
        }
    }

    #[test]
    fn brokered_threads_keep_their_evidence_and_fill_a_missing_baseline() {
        let threads = threads_from_wire(WireThreads {
            process_base_priority: 5,
            threads: vec![
                wire_thread(41, WireThreadState::Running),
                crate::linux_core_service::WireThread {
                    tid: 42,
                    priority: 25,
                    nice: None,
                    context_switches: None,
                    kernel_time_100ns: 0,
                    user_time_100ns: 0,
                    created_epoch_s: None,
                    state: WireThreadState::DiskSleep,
                    cycles: None,
                },
            ],
        });
        assert_eq!(threads.process_base_priority, 5);
        let first = &threads.threads[0];
        assert_eq!(first.tid, 41);
        assert_eq!(first.state, ThreadState::Runnable);
        assert_eq!(first.base_priority, -5);
        assert_eq!(first.context_switches, Some(9));
        assert_eq!(first.cycles, Some(13));
        assert_eq!(first.created_epoch_s, Some(1_700_000_123));
        let second = &threads.threads[1];
        assert_eq!(
            second.base_priority, 5,
            "the thread group's baseline stands in for a missing nice value"
        );
        assert_eq!(second.nice, None);
        assert_eq!(second.state, ThreadState::Waiting);
        assert_eq!(
            second.context_switches, None,
            "an unavailable switch count must not become zero"
        );
        assert_eq!(second.cycles, None);
    }

    #[test]
    fn linux_state_letters_survive_the_broker_round_trip() {
        for (wire, expected) in [
            // Linux 'R' covers both on-CPU and queued threads.
            (WireThreadState::Running, ThreadState::Runnable),
            (WireThreadState::Sleeping, ThreadState::Sleeping),
            (WireThreadState::DiskSleep, ThreadState::Waiting),
            (WireThreadState::Stopped, ThreadState::Stopped),
            (WireThreadState::Tracing, ThreadState::Stopped),
            (WireThreadState::Zombie, ThreadState::Terminated),
            (WireThreadState::Dead, ThreadState::Terminated),
            (WireThreadState::Idle, ThreadState::Idle),
            (WireThreadState::Parked, ThreadState::Parked),
            // An unnamed state keeps its raw byte, exactly like the direct read.
            (
                WireThreadState::Other(b'?'),
                ThreadState::Other(u32::from(b'?')),
            ),
        ] {
            assert_eq!(thread_state_from_wire(wire), expected, "{wire:?}");
        }
    }

    fn wire_row(
        protocol: &str,
        local: &str,
        remote: Option<&str>,
        state: Option<&str>,
    ) -> WireConnection {
        WireConnection {
            protocol: protocol.into(),
            local: local.into(),
            remote: remote.map(Into::into),
            state: state.map(Into::into),
        }
    }

    #[test]
    fn brokered_connections_keep_only_what_the_model_can_name() {
        let mapping = connections_from_wire(vec![
            wire_row(
                "tcp",
                "127.0.0.1:8080",
                Some("10.0.0.2:443"),
                Some("established"),
            ),
            // The kernel's all-zero placeholder is not a peer.
            wire_row("tcp", "0.0.0.0:22", Some("0.0.0.0:0"), Some("listen")),
            wire_row("tcp6", "[::1]:9100", None, Some("time_wait")),
            wire_row("udp", "0.0.0.0:53", None, None),
            // A Unix endpoint has no `SocketAddr` in this model, and is not a
            // defect: it is dropped like the direct reader's IP-table walk.
            wire_row("unix", "/run/user/1000/bus", None, None),
        ]);
        let connections = mapping.connections;
        assert!(mapping.unparsable.is_empty());
        assert_eq!(connections.len(), 4);
        assert_eq!(connections[0].state, Some(TcpState::Established));
        assert_eq!(
            connections[0].remote,
            Some("10.0.0.2:443".parse().expect("peer"))
        );
        assert_eq!(connections[1].remote, None, "a listener has no peer");
        assert_eq!(connections[2].protocol, ConnectionProtocol::Tcp6);
        assert_eq!(connections[2].state, Some(TcpState::TimeWait));
        assert_eq!(connections[3].protocol, ConnectionProtocol::Udp);
        assert_eq!(
            connections[3].state, None,
            "a datagram endpoint has no state"
        );
    }

    #[test]
    fn an_unparsable_endpoint_is_named_and_dropped_alone() {
        let mapping = connections_from_wire(vec![
            wire_row("tcp", "127.0.0.1:8080", None, Some("listen")),
            wire_row("tcp", "not an address", None, Some("listen")),
            wire_row(
                "tcp",
                "127.0.0.1:9",
                Some("also not an address"),
                Some("established"),
            ),
        ]);
        assert_eq!(
            mapping.connections.len(),
            1,
            "the other rows must survive a defect"
        );
        assert_eq!(
            mapping.unparsable,
            [
                "not an address".to_string(),
                "also not an address".to_string()
            ]
        );
    }

    #[test]
    fn tcp_state_names_are_read_in_the_wire_spelling() {
        assert_eq!(tcp_state_from_wire("Listen"), Some(TcpState::Listen));
        assert_eq!(tcp_state_from_wire("syn_sent"), Some(TcpState::SynSent));
        assert_eq!(tcp_state_from_wire("close-wait"), Some(TcpState::CloseWait));
        assert_eq!(
            tcp_state_from_wire("something-new"),
            None,
            "an unknown name must not be guessed at"
        );
        assert_eq!(tcp_state_from_wire(""), None);
    }

    #[test]
    fn endpoints_are_parsed_in_every_address_form_the_contract_allows() {
        assert_eq!(
            parse_socket_addr("127.0.0.1:80"),
            Some("127.0.0.1:80".parse().expect("ipv4 endpoint"))
        );
        assert_eq!(
            parse_socket_addr("[::1]:80"),
            Some("[::1]:80".parse().expect("bracketed endpoint"))
        );
        assert_eq!(
            parse_socket_addr("::1:80"),
            Some("[::1]:80".parse().expect("unbracketed endpoint"))
        );
        assert_eq!(parse_socket_addr("127.0.0.1"), None);
        assert_eq!(parse_socket_addr(""), None);
    }
}
