//! Error types shared across the workspace.
//!
//! Hand-written `Display`/`Error` impls (no proc-macro dependency needed).

pub type Result<T> = std::result::Result<T, TmError>;

#[derive(Debug)]
pub enum TmError {
    Unsupported(&'static str),
    Io(std::io::Error),
    Json(serde_json::Error),
    ChannelClosed,
    Platform {
        context: &'static str,
        detail: String,
    },
    ProcessNotFound {
        pid: u32,
    },
    ServiceNotFound(String),
}

impl std::fmt::Display for TmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TmError::Unsupported(what) => {
                write!(f, "operation not supported on this platform: {what}")
            }
            TmError::Io(e) => write!(f, "I/O error: {e}"),
            TmError::Json(e) => write!(f, "serialization error: {e}"),
            TmError::ChannelClosed => write!(f, "engine channel closed"),
            TmError::Platform { context, detail } => {
                write!(f, "platform API failure: {context}: {detail}")
            }
            TmError::ProcessNotFound { pid } => write!(f, "process {pid} not found"),
            TmError::ServiceNotFound(name) => write!(f, "service '{name}' not found"),
        }
    }
}

impl std::error::Error for TmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TmError::Io(e) => Some(e),
            TmError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TmError {
    fn from(e: std::io::Error) -> Self {
        TmError::Io(e)
    }
}

impl From<serde_json::Error> for TmError {
    fn from(e: serde_json::Error) -> Self {
        TmError::Json(e)
    }
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
