//! Logging setup: rolling daily file sink + optional console layer.
//!
//! Startup architecture (see implement.md §5.2): the GUI must not touch the
//! log-directory filesystem before the first frame. Two entry points:
//!
//! * [`init_early`] — installs a subscriber backed by a small bounded
//!   in-memory ring sink (plus an optional console layer). No disk I/O.
//! * [`attach_file_logging`] — called after the first UI frame; opens the
//!   rolling file appender, replays the buffered early records into it, and
//!   from then on records stream straight to disk.
//!
//! CLI paths (`--selfcheck`, tools) can keep using synchronous [`init`].
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

// ------------------------------------------------------------ deferred sink

/// Shared state of the deferred file sink: `None` until
/// [`attach_file_logging`] runs.
type SinkCell = std::sync::Mutex<Option<tracing_appender::non_blocking::NonBlocking>>;
static SINK: std::sync::OnceLock<SinkCell> = std::sync::OnceLock::new();

fn sink() -> &'static SinkCell {
    SINK.get_or_init(|| std::sync::Mutex::new(None))
}

/// Bounded pre-file buffer of formatted early records.
const EARLY_CAP: usize = 512;
static EARLY: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn now_stamp_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

struct FormatFields<'a>(&'a mut String);

impl tracing::field::Visit for FormatFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.0, "{value}");
        } else {
            let _ = write!(self.0, " {}=\"{value}\"", field.name());
        }
    }
}

fn format_event(buf: &mut String, event: &tracing::Event<'_>) {
    use std::fmt::Write as _;
    let ts = now_stamp_ms();
    let _ = write!(
        buf,
        "{ts} {} [{}] ",
        event.metadata().target(),
        event.metadata().level()
    );
    event.record(&mut FormatFields(buf));
}

/// One layer serving both phases:
/// * before attach — records go into the bounded ring,
/// * after attach — records stream to the rolling file writer.
///
/// Console output (verbose/selfcheck) is a separate standard fmt layer.
struct DeferredFileLayer;

impl<S> tracing_subscriber::layer::Layer<S> for DeferredFileLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut buf = String::with_capacity(128);
        format_event(&mut buf, event);

        let mut sink_guard = sink().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(writer) = sink_guard.as_mut() {
            use std::io::Write as _;
            let _ = writeln!(writer, "{buf}");
            return;
        }
        drop(sink_guard);
        // Pre-attach: bounded ring. When full, keep a single truncation
        // marker instead of growing unbounded or blocking.
        let mut lines = EARLY.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() < EARLY_CAP {
            lines.push(buf);
        } else if let Some(last) = lines.last_mut()
            && !last.ends_with('…')
        {
            last.push_str(" …");
        }
    }
}

/// Install a no-disk-IO subscriber (ring sink + optional console). Safe to
/// call once at process start for GUI runs; [`attach_file_logging`] later
/// opens the real file sink without reinitializing the global subscriber.
pub fn init_early(console: bool) {
    INIT.call_once(|| {
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_targets()));
        let console_layer = console.then(|| {
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(true)
                .with_line_number(true)
                .with_filter(filter)
        });
        tracing_subscriber::registry()
            .with(DeferredFileLayer)
            .with(console_layer)
            .init();
        std::panic::set_hook(Box::new(quiet_panic_hook()));
        tracing::info!(version = env!("CARGO_PKG_VERSION"), "early logging active");
    });
}

/// Attach the rolling file sink (GUI: after the first presented frame).
/// Replays buffered early records in order. Returns the worker guard that
/// must be kept alive for the process lifetime (flushes asynchronously).
/// Falls back to synchronous [`init`] when the subscriber wasn't set up
/// through [`init_early`].
pub fn attach_file_logging(cfg: LogConfig) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    if !INIT.is_completed() || SINK.get().is_none() {
        return init(cfg);
    }
    let log_dir = crate::settings::taskman_data_dir().join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("taskman: cannot create log dir {}: {e}", log_dir.display());
        return None;
    }
    let appender = tracing_appender::rolling::daily(&log_dir, "taskman.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    // Replay before publishing the writer so ordering stays monotonic.
    let replayed: Vec<String> =
        std::mem::take(&mut *EARLY.lock().unwrap_or_else(|e| e.into_inner()));
    {
        let mut w = writer.clone();
        use std::io::Write as _;
        for line in &replayed {
            let _ = writeln!(w, "{line}");
        }
    }
    *sink().lock().unwrap_or_else(|e| e.into_inner()) = Some(writer);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        dir = %log_dir.display(),
        early_records = replayed.len(),
        console = cfg.console,
        "file logging attached"
    );
    Some(guard)
}

/// Initialize tracing exactly once with synchronous file setup; later calls
/// are no-ops. Returns the appender guard that must be kept alive for the
/// process lifetime (flushes asynchronously). Use for CLI/headless paths.
pub fn init(cfg: LogConfig) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let mut result_guard = None;
    INIT.call_once(|| {
        let filter = make_filter(&cfg);

        // File sink: daily-rolling under the platform log dir.
        let log_dir = crate::settings::taskman_data_dir().join("logs");
        let file_layer = {
            match std::fs::create_dir_all(&log_dir) {
                Ok(()) => {
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
                }
                Err(e) => {
                    eprintln!("taskman: cannot create log dir {}: {e}", log_dir.display());
                    None
                }
            }
        };

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
