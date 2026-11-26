//! Root-side broker: socket setup, peer authentication and request dispatch.
//!
//! Runs only inside `taskman-service` as a systemd system unit. The transport
//! contract is [`crate::linux_core_service`]; this module is the implementation
//! the service binary calls.
//!
//! This file is a stub in the foundation commit: the daemon task fills it in.
//! The public surface below is fixed, because `tm-service` and the GUI client
//! are written against it.

/// Run the broker until the process is asked to stop.
///
/// Fails only when the service cannot start at all (no manifest, unprotected
/// install, socket that cannot be bound). A per-connection error never stops
/// the broker; the accept loop reports and keeps serving.
pub fn run_service() -> Result<(), String> {
    Err("linux core-service broker is not implemented in this build".into())
}

/// Headless smoke test: validate the manifest and install paths, open and
/// close one local connection, and print a JSON summary. Must not start,
/// stop or install any unit.
pub fn selfcheck() -> Result<String, String> {
    Err("linux core-service self-check is not implemented in this build".into())
}
