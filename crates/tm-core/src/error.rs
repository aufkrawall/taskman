//! Error types shared across the workspace.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, TmError>;

#[derive(Debug, Error)]
pub enum TmError {
    #[error("operation not supported on this platform: {0}")]
    Unsupported(&'static str),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("engine channel closed")]
    ChannelClosed,

    #[error("platform API failure: {context}: {detail}")]
    Platform {
        context: &'static str,
        detail: String,
    },

    #[error("process {pid} not found")]
    ProcessNotFound { pid: u32 },

    #[error("service '{0}' not found")]
    ServiceNotFound(String),
}

impl TmError {
    /// Convenience constructor for platform errors.
    pub fn platform(context: &'static str, detail: impl Into<String>) -> Self {
        TmError::Platform {
            context,
            detail: detail.into(),
        }
    }
}
