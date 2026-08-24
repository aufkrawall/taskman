//! Logging setup: rolling daily file sink + optional console layer.
//!
//! Level control (highest priority first):
//!   1. explicit `level` argument from CLI (`--verbose` / `--debug`)
//!   2. `RUST_LOG` env var (standard env-filter syntax)
//!   3. default `info`

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Where logs are written.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogConfig {
    pub console: bool,
    /// Explicit level overriding RUST_LOG.
    pub level: Option<tracing::level_filters::LevelFilter>,
}

static INIT: std::sync::Once = std::sync::Once::new();

fn default_targets() -> String {
    // Our crates at info; noisy third-party crates tamed.
    "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()
}

fn make_filter(cfg: &LogConfig) -> EnvFilter {
    match cfg.level {
        Some(l) => EnvFilter::new(format!("tm=trace,{l}")),
        None => {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_targets()))
        }
    }
}

fn quiet_panic_hook() -> impl Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync {
    move |info| {
        tracing::error!(payload = %info, "panic");
    }
}

/// Initialize tracing exactly once; later calls are no-ops. Returns the
/// appender guard that must be kept alive for the process lifetime
/// (flushes asynchronously).
pub fn init(cfg: LogConfig) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let mut result_guard = None;
    INIT.call_once(|| {
        let filter = make_filter(&cfg);

        // File sink: daily-rolling under the platform log dir.
        let file_layer = dirs::data_local_dir()
            .map(|d| d.join("taskman").join("logs"))
            .and_then(|log_dir| {
                std::fs::create_dir_all(&log_dir)
                    .map_err(|e| {
                        eprintln!("taskman: cannot create log dir {}: {e}", log_dir.display())
                    })
                    .ok()?;
                let appender = tracing_appender::rolling::daily(&log_dir, "taskman.log");
                let (writer, g) = tracing_appender::non_blocking(appender);
                result_guard = Some(g);
                Some(
                    fmt::layer()
                        .with_writer(std::sync::Mutex::new(writer))
                        .with_ansi(false)
                        .with_target(true)
                        .with_line_number(true)
                        .with_filter(filter.clone()),
                )
            });

        let console_layer = cfg.console.then(|| {
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(true)
                .with_line_number(true)
                .with_filter(filter.clone())
        });

        tracing_subscriber::registry()
            .with(file_layer)
            .with(console_layer)
            .init();

        std::panic::set_hook(Box::new(quiet_panic_hook()));
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            console = cfg.console,
            "logging initialized"
        );
    });
    result_guard
}
