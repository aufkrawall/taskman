//! Linux core-service transport.
//!
//! The wire contract, the protected manifest and every authorization decision
//! live in [`crate::linux_core_service`], which is platform-neutral and fully
//! unit-tested. This module only moves those frames: [`server`] is the
//! root-side broker inside `taskman-service`, [`client`] is the GUI-side
//! connection used when the current token cannot do the work itself.

pub mod client;
pub mod server;
