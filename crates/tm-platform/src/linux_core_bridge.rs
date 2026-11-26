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

use crate::linux_core_service::{Refusal, Response, ServiceFailure};
use tm_core::error::TmError;

/// Why a request did not produce a response.
///
/// The distinction is the fallback contract: only [`Self::Unavailable`] means
/// "no broker answered, try your own token"; a refusal or a failure is an
/// answer, and repeating the operation elsewhere would turn a safety decision
/// into permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// No broker is reachable (not installed, not running, socket missing, or
    /// the exchange ran past its deadline).
    Unavailable(String),
    /// The broker answered and refused. Callers must NOT fall back.
    Refused { code: Refusal, detail: String },
    /// The broker answered a well-formed request with a failure.
    Failed(String),
}

impl ClientError {
    /// Whether a caller may still attempt the operation under its own token.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux_core_service::{
        ProcessEvidence, ServiceStatusInfo, WireConnection, WireFileIo, WireModule, WireThreads,
    };

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
}
