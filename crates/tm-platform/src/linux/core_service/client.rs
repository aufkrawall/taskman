//! GUI-side client for the Linux core service.
//!
//! One process-wide connection, serialized behind a mutex, with a bounded
//! deadline per request: sampling must never park on a wedged broker. A
//! refused request is returned as [`ClientError::Refused`] and never retried
//! through another path — an explicit broker rejection must not degrade into
//! "try it under the GUI token anyway".
//!
//! This file is a stub in the foundation commit: the client task fills it in.
//! The public surface below is fixed, because the Linux action surface, the
//! sampler and the daemon self-check are written against it.

use crate::linux_core_service::{Refusal, Request, Response, ServiceStatusInfo};

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
    pub fn may_fall_back(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// Send one request through the shared connection.
pub fn request(_request: &Request) -> Result<Response, ClientError> {
    Err(ClientError::Unavailable(
        "linux core-service client is not implemented in this build".into(),
    ))
}

/// Cached capability report, or `None` when the broker is unreachable.
pub fn status() -> Option<ServiceStatusInfo> {
    None
}

/// Whether a broker was reachable at the last probe.
pub fn available() -> bool {
    false
}
