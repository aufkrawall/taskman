//! tm-core — platform-agnostic heart of the task manager.
//!
//! Contains the snapshot data model, the background sampling engine,
//! process classification, formatting helpers, settings persistence and
//! the app-history database. All OS specifics live in `tm-platform`.

pub mod app_history;
pub mod classify;
pub mod engine;
pub mod error;
pub mod format;
pub mod logging;
pub mod mock;
pub mod model;
pub mod ring;
pub mod settings;

pub use app_history::AppHistoryDb;
pub use engine::{EngineCmd, EngineHandle, EngineState};
pub use error::{Result, TmError};
pub use model::*;
