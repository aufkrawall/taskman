//! Logging setup: rolling daily file sink + optional console layer.
//!
//! Startup architecture: the GUI must not touch the log-directory filesystem
//! before the first frame. Two entry points:
//!
//! * [`init_early`] — installs a subscriber backed by a small bounded
//!   in-memory ring sink (plus an optional console layer). No disk I/O.
//! * [`attach_file_logging`] — called from `app.rs` once the first frame has
//!   been submitted; opens the rolling file appender, replays the buffered
//!   early records into it, and from then on records stream straight to disk.
//!
//! CLI paths (`--selfcheck`, tools) can keep using synchronous [`init`].
//!
//! **The appender guard is owned here, in a process-lifetime static, and is
//! never returned to callers.** `tracing_appender`'s `WorkerGuard` shuts the
//! writer thread down when it drops, and the non-blocking writer is *lossy*:
//! every record after that drop is silently discarded. A returned guard bound
//! to a `let _guard` inside an `if` block did exactly that here, so for
//! several releases every log file on disk held one single line. Take the
//! guard back out through [`shutdown`] at a controlled exit; nothing else may
//! own it.
//!
//! Level control (highest priority first):
//!   1. explicit `level` argument from CLI (`--verbose` / `--debug`)
//!   2. `RUST_LOG` env var (standard env-filter syntax)
//!   3. default `info`

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

const MAX_LOG_FILES: usize = 14;

/// Where logs are written.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogConfig {
    pub console: bool,
    /// Explicit level overriding RUST_LOG.
    pub level: Option<tracing::level_filters::LevelFilter>,
}

static INIT: std::sync::Once = std::sync::Once::new();

/// The appender's worker-thread guard, owned for the process lifetime.
///
/// Dropping it shuts the writer thread down and the lossy non-blocking writer
/// then discards every later record, so it is never handed to a caller who
/// could bind it to a short-lived `let _guard`. [`shutdown`] takes it back out
/// at a controlled exit, which is the flush.
static GUARD: std::sync::Mutex<Option<tracing_appender::non_blocking::WorkerGuard>> =
    std::sync::Mutex::new(None);

fn store_guard(guard: tracing_appender::non_blocking::WorkerGuard) {
    *GUARD.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);
}

/// Flush and stop the file appender. Call once, from a controlled shutdown;
/// logging after it is discarded. Crashes do not come through here — see
/// [`write_crash_record`].
pub fn shutdown() {
    let guard = GUARD.lock().unwrap_or_else(|e| e.into_inner()).take();
    drop(guard);
}

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
        // Release machine-wide resources FIRST. Release builds are
        // `panic = "abort"`, so no destructor will ever run, and some of what
        // this process holds outlives it: an ETW session is a kernel object
        // that keeps tracing every disk and network operation on the machine
        // until something stops it by name or the user reboots. Losing a
        // crash record is bad; leaving a system-wide kernel trace running on
        // a user's machine is worse.
        if let Some(cleanup) = PANIC_CLEANUP.get() {
            cleanup();
        }
        tracing::error!(payload = %info, "panic");
        write_crash_record(info);
    }
}

/// Machine-wide teardown to run before aborting; see [`set_panic_cleanup`].
static PANIC_CLEANUP: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Register the teardown the panic hook runs before the process aborts.
///
/// For resources that OUTLIVE the process and are not reclaimed by it dying —
/// today that means the ETW sessions in `tm-platform`, which survive their
/// host and go on filling kernel buffers. Ordinary process-scoped state does
/// not belong here: Windows reclaims handles, memory and window hooks on its
/// own, and every instruction in a panic hook is one more chance to fault
/// before the record is written.
///
/// The callback runs on the panicking thread with `panic = "abort"` pending,
/// so it must not lock anything that thread could already hold, must not
/// allocate if it can avoid it, and must not panic. First registration wins.
pub fn set_panic_cleanup(cleanup: fn()) {
    let _ = PANIC_CLEANUP.set(cleanup);
}

// ------------------------------------------------------------ crash records

/// Where [`write_crash_record`] writes, and under which prefix. Set by
/// whichever init path resolved a log directory; the GUI's `init_early` runs
/// before any directory is known, so an unset cell falls back to the default
/// data dir at crash time.
static CRASH_TARGET: std::sync::OnceLock<(std::path::PathBuf, String)> = std::sync::OnceLock::new();

/// Keep one crash file from growing without bound across repeated crashes.
const CRASH_LOG_MAX_BYTES: u64 = 512 * 1024;

fn crash_target() -> (std::path::PathBuf, String) {
    CRASH_TARGET.get().cloned().unwrap_or_else(|| {
        (
            crate::settings::taskman_data_dir().join("logs"),
            "taskman.log".to_string(),
        )
    })
}

/// Append a panic — with a backtrace — straight to disk, synchronously.
///
/// This deliberately bypasses `tracing` entirely. Release builds are
/// `panic = "abort"`, so the hook returns into `abort()` and nothing ever
/// drops the appender guard; `NonBlocking::flush` is a no-op and the worker
/// thread is not scheduled again, so a panic routed only through the async
/// sink is lost precisely when it matters. Everything here is fallible-free
/// (`let _ =`, no `unwrap`): a panic inside a panic hook aborts with no
/// record at all.
pub fn write_crash_record(info: &std::panic::PanicHookInfo<'_>) {
    let (dir, prefix) = crash_target();
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<unnamed>").to_string();
    let backtrace = std::backtrace::Backtrace::force_capture().to_string();

    // The ring holds everything logged before the file sink attached; on a
    // startup crash it is the only history that exists. `try_lock`, never
    // `lock`: the panicking thread may already hold either mutex (both are
    // taken in `DeferredFileLayer::on_event`) and blocking on one inside a
    // panic hook hangs the process instead of aborting it.
    let buffered = if sink().try_lock().is_ok_and(|g| g.is_none()) {
        EARLY.try_lock().ok().map(|lines| lines.clone())
    } else {
        None
    };

    let record = crash_record_text(
        &format!("{info}"),
        &thread_name,
        &backtrace,
        buffered.as_deref(),
    );
    append_crash_record(&dir, &prefix, &record);
}

/// Format one crash record. Pure, so its shape is testable without panicking.
fn crash_record_text(
    payload: &str,
    thread_name: &str,
    backtrace: &str,
    buffered: Option<&[String]>,
) -> String {
    use std::fmt::Write as _;
    let mut record = String::with_capacity(4096);
    // Epoch ms, matching `format_event`, so the replayed ring lines below
    // correlate with the panic line above them.
    let _ = writeln!(record, "==== panic {} ====", now_stamp_ms());
    let _ = writeln!(record, "version: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(record, "thread:  {thread_name}");
    let _ = writeln!(record, "payload: {payload}");
    let _ = writeln!(
        record,
        "backtrace:
{backtrace}"
    );
    if let Some(lines) = buffered.filter(|l| !l.is_empty()) {
        let _ = writeln!(record, "-- {} buffered record(s) --", lines.len());
        for line in lines {
            let _ = writeln!(record, "{line}");
        }
    }
    let _ = writeln!(record);
    record
}

/// Append to `<dir>/<prefix>.crash`, rotating once past [`CRASH_LOG_MAX_BYTES`].
/// Infallible by construction: there is no caller that could handle an error.
fn append_crash_record(dir: &std::path::Path, prefix: &str, record: &str) {
    use std::io::Write as _;
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join(format!("{prefix}.crash"));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > CRASH_LOG_MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("crash.old"));
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = file.write_all(record.as_bytes());
    let _ = file.sync_data();
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
                .with_filter(filter.clone())
        });
        // The deferred layer MUST carry the same filter. Unfiltered, it makes
        // every level interesting to the whole subscriber, which defeats
        // `tracing`'s global max-level short-circuit on the sampler's hot path
        // and buries our own records under wgpu/naga trace spam once the file
        // sink is attached.
        tracing_subscriber::registry()
            .with(DeferredFileLayer.with_filter(filter))
            .with(console_layer)
            .init();
        std::panic::set_hook(Box::new(quiet_panic_hook()));
        tracing::info!(version = env!("CARGO_PKG_VERSION"), "early logging active");
    });
}

/// Attach the rolling file sink (GUI: after the first presented frame).
/// Replays buffered early records in order. Falls back to synchronous
/// [`init`] when the subscriber wasn't set up through [`init_early`].
///
/// Returns whether the file sink is now live. The guard stays in [`GUARD`];
/// see the module docs for why it is not returned.
pub fn attach_file_logging(cfg: LogConfig) -> bool {
    if !INIT.is_completed() || SINK.get().is_none() {
        return init(cfg);
    }
    if sink().lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        // Already attached; a second replay would duplicate the ring.
        return true;
    }
    let log_dir = crate::settings::taskman_data_dir().join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("taskman: cannot create log dir {}: {e}", log_dir.display());
        return false;
    }
    let appender = match tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("taskman.log")
        .max_log_files(MAX_LOG_FILES)
        .build(&log_dir)
    {
        Ok(appender) => appender,
        Err(error) => {
            eprintln!(
                "taskman: cannot open log appender {}: {error}",
                log_dir.display()
            );
            return false;
        }
    };
    let (writer, guard) = tracing_appender::non_blocking(appender);
    store_guard(guard);
    let _ = CRASH_TARGET.set((log_dir.clone(), "taskman.log".to_string()));

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
    true
}

/// Initialize tracing exactly once with synchronous file setup; later calls
/// are no-ops. Returns whether a file sink came up; the guard stays in
/// [`GUARD`]. Use for CLI/headless paths.
pub fn init(cfg: LogConfig) -> bool {
    let log_dir = crate::settings::taskman_data_dir().join("logs");
    init_in_dir(cfg, &log_dir, "taskman.log")
}

/// Initialize synchronous logging at an explicit protected directory. This
/// is used by non-interactive components such as the Windows service, whose
/// logs must not inherit the interactive user's writable data directory.
/// Also the crash-record target, so a service panic lands beside its log.
pub fn init_in_dir(cfg: LogConfig, log_dir: &std::path::Path, file_name: &str) -> bool {
    let mut attached = false;
    INIT.call_once(|| {
        let filter = make_filter(&cfg);

        // File sink: daily-rolling under the platform log dir.
        let file_layer = {
            match std::fs::create_dir_all(log_dir) {
                Ok(()) => {
                    match tracing_appender::rolling::RollingFileAppender::builder()
                        .rotation(tracing_appender::rolling::Rotation::DAILY)
                        .filename_prefix(file_name)
                        .max_log_files(MAX_LOG_FILES)
                        .build(log_dir)
                    {
                        Ok(appender) => {
                            let (writer, guard) = tracing_appender::non_blocking(appender);
                            store_guard(guard);
                            let _ =
                                CRASH_TARGET.set((log_dir.to_path_buf(), file_name.to_string()));
                            attached = true;
                            Some(
                                fmt::layer()
                                    .with_writer(std::sync::Mutex::new(writer))
                                    .with_ansi(false)
                                    .with_target(true)
                                    .with_line_number(true)
                                    .with_filter(filter.clone()),
                            )
                        }
                        Err(error) => {
                            eprintln!(
                                "taskman: cannot open log appender {}: {error}",
                                log_dir.display()
                            );
                            None
                        }
                    }
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
    attached
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_log_dir(dir: &std::path::Path) -> String {
        std::fs::read_dir(dir)
            .expect("log dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".log"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .collect()
    }

    #[test]
    fn crash_record_carries_payload_thread_and_backtrace() {
        let text = crash_record_text("boom at src/x.rs:1", "sampler", "0: frame_one", None);
        assert!(text.contains("payload: boom at src/x.rs:1"), "{text}");
        assert!(text.contains("thread:  sampler"), "{text}");
        assert!(text.contains("0: frame_one"), "{text}");
        assert!(
            text.contains(env!("CARGO_PKG_VERSION")),
            "a crash record without the version cannot be matched to a build: {text}"
        );
    }

    #[test]
    fn crash_record_replays_the_pre_attach_ring() {
        // A panic before the file sink attaches leaves the ring as the only
        // history of the run, so it has to travel with the record.
        let ring = vec![
            "1 tm [INFO] early one".to_string(),
            "2 tm [WARN] two".into(),
        ];
        let text = crash_record_text("boom", "main", "bt", Some(&ring));
        assert!(text.contains("-- 2 buffered record(s) --"), "{text}");
        assert!(text.contains("early one") && text.contains("two"), "{text}");
    }

    #[test]
    fn crash_log_appends_and_rotates_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taskman.log.crash");

        append_crash_record(
            dir.path(),
            "taskman.log",
            "first
",
        );
        append_crash_record(
            dir.path(),
            "taskman.log",
            "second
",
        );
        let both = std::fs::read_to_string(&path).unwrap();
        assert!(
            both.contains("first") && both.contains("second"),
            "records must append, not overwrite: {both}"
        );

        // Past the cap the current file is rotated aside rather than grown.
        std::fs::write(&path, vec![b'x'; (CRASH_LOG_MAX_BYTES + 1) as usize]).unwrap();
        append_crash_record(
            dir.path(),
            "taskman.log",
            "after rotate
",
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "after rotate
",
            "the live crash file must start fresh after a rotation"
        );
        assert!(
            dir.path().join("taskman.log.crash.old").is_file(),
            "the rotated-away records must be kept"
        );
    }

    /// The regression this module exists for.
    ///
    /// `init_in_dir` used to hand the appender's `WorkerGuard` back to the
    /// caller, and `tm-app`'s `main` bound it to a `let _log_guard` inside an
    /// `if` block. The guard dropped one line later, the non-blocking writer
    /// is lossy, and so every shipped log file contained exactly one line: the
    /// "logging initialized" record emitted just before the drop. Anything
    /// that hands the guard back out will fail here.
    ///
    /// This is the ONLY test that may initialize logging: the subscriber and
    /// its `INIT` Once are process-global, so a second initializing test would
    /// silently no-op.
    #[test]
    fn logging_keeps_recording_after_init_returns() {
        let dir = tempfile::tempdir().unwrap();
        let attached = init_in_dir(LogConfig::default(), dir.path(), "taskman-test.log");
        assert!(attached, "file sink must come up in a writable temp dir");

        for i in 0..8 {
            tracing::info!(seq = i, "record after init returned");
        }
        shutdown();

        let body = read_log_dir(dir.path());
        let recorded = body.matches("record after init returned").count();
        assert_eq!(
            recorded, 8,
            "every record after init must reach the file, got {recorded} in:
{body}"
        );

        // While this test owns the process-global logging state, exercise the
        // panic hook `init_in_dir` installed. It is the only place the full
        // chain — hook -> crash_target() -> append_crash_record — runs, and
        // release builds abort straight afterwards, so a break here is
        // invisible until someone needs the record.
        let crash = dir.path().join("taskman-test.log.crash");
        let panicked = std::panic::catch_unwind(|| panic!("crash-record end to end"));
        assert!(panicked.is_err(), "the test panic must have been caught");
        let record = std::fs::read_to_string(&crash)
            .unwrap_or_else(|e| panic!("no crash record at {}: {e}", crash.display()));
        assert!(
            record.contains("crash-record end to end"),
            "the payload must reach the file: {record}"
        );
        assert!(
            record.contains("backtrace:"),
            "a crash record without a backtrace is why this work happened: {record}"
        );
    }
}
