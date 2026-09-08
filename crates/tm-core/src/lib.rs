//! tm-core — platform-agnostic heart of the task manager.
//!
//! Contains the snapshot data model, the background sampling engine,
//! process classification, formatting helpers, settings persistence and
//! the app-history database. All OS specifics live in `tm-platform`.

// The platform-agnostic core must stay free of `unsafe`; OS interaction is
// tm-platform's job. This is an invariant, not a style preference.
#![deny(unsafe_code)]

pub mod app_history;
pub mod classify;
pub mod demand;
pub mod engine;
pub mod error;
pub mod format;
pub mod i18n;
pub mod locale;
pub mod logging;
pub mod mock;
pub mod model;
pub mod settings;
pub mod sync;

pub use app_history::AppHistoryDb;
pub use demand::TelemetryDemand;
pub use engine::{CollectorFactory, EngineCmd, EngineHandle, EngineState, NotifyFn};
pub use error::{Result, TmError};
pub use model::*;
