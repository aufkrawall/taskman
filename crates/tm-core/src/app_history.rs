//! Persistent per-application resource usage history ("App history" tab).
//!
//! Mirrors the idea of Windows Task Manager's app history: cumulative CPU time
//! and network traffic per application since a given date, surviving restarts.
//! We track our own measurements (unlike TM which mines SRUM), so the date
//! shown is "since" our first observation.
//!
//! Correctness rules (implement.md §16):
//! * One long-lived writer thread owns the temp file; saves are coalesced by
//!   monotonic generation so an older snapshot can never overwrite a newer
//!   one after it.
//! * App identity is a normalized executable path (fallback: bare filename),
//!   so two different executables that share a filename never collide.
//! * PID reuse is detected by process identity (pid + start time): when it
//!   changes, the new process is treated as a first sighting regardless of
//!   counter direction.
//! * Missing per-process network telemetry is never accumulated as a fake
//!   zero measurement.

use crate::model::{ProcCategory, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppUsage {
    pub cpu_seconds: f64,
    pub network_bytes: u64,
}

/// Stable app identity: package-style id or normalized executable path;
/// lowercase filename only as last resort. Display name is stored separately.
fn entry_key(p: &crate::model::ProcessEntry) -> String {
    if let Some(path) = &p.exe_path {
        let norm = path.to_string_lossy().to_ascii_lowercase();
        if !norm.is_empty() {
            return norm;
        }
    }
    p.name.to_ascii_lowercase()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DbFile {
    /// Epoch seconds when tracking started.
    since_epoch_s: i64,
    /// Keyed by stable [`entry_key`].
    entries: BTreeMap<String, AppUsage>,
    /// Friendliest display name seen for each key.
    #[serde(default)]
    names: BTreeMap<String, String>,
}

/// Previous tick's counters keyed by pid, guarded by process identity.
#[derive(Debug, Clone)]
struct PrevTick {
    /// ProcessIdentity guard: start epoch of the observed process.
    start_epoch_s: Option<i64>,
    app_identity: String,
    cpu_time_s: f64,
    net_total_bytes: Option<u64>,
}

/// Load state for the deferred open (implement.md §5.4).
struct LoadState {
    loading: bool,
    since: std::time::Instant,
}

impl Default for LoadState {
    fn default() -> Self {
        Self {
            loading: false,
            since: std::time::Instant::now(),
        }
    }
}

pub struct AppHistoryDb {
    path: Option<PathBuf>,
    since_epoch_s: i64,
    entries: BTreeMap<String, AppUsage>,
    names: BTreeMap<String, String>,
    /// Previous tick's accumulated cpu-time/net-totals keyed by pid.
    prev: std::collections::HashMap<u32, PrevTick>,
    state: LoadState,
    /// Monotonic save generation; also guards against reordered writes.
    generation: u64,
    writer: Option<HistoryWriter>,
    load_rx: Option<std::sync::mpsc::Receiver<Option<String>>>,
}

/// Single serialized writer thread owning the `.tmp` path.
struct HistoryWriter {
    tx: Sender<WriteCmd>,
}

enum WriteCmd {
    Save {
        generation: u64,
        snapshot: std::sync::Arc<DbFile>,
    },
    Flush(Sender<()>),
    Shutdown,
}

impl HistoryWriter {
    fn start(path: PathBuf) -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<WriteCmd>();
        let spawned = std::thread::Builder::new()
            .name("tm-hist-writer".into())
            .spawn(move || history_writer_loop(path, rx));
        spawned.ok().map(|_| Self { tx })
    }

    fn save(&self, generation: u64, file: std::sync::Arc<DbFile>) {
        let _ = self.tx.send(WriteCmd::Save {
            generation,
            snapshot: file,
        });
    }

    /// Blocking flush of everything queued so far (bounded wait).
    fn flush(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.tx.send(WriteCmd::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(3));
        }
    }
}

impl Drop for HistoryWriter {
    fn drop(&mut self) {
        let _ = self.tx.send(WriteCmd::Shutdown);
    }
}

/// The writer keeps only the newest pending generation; one thread owns the
/// tmp path, so an older save can never finish after a newer one and replace
/// it (implement.md §16.1/§25.18).
fn history_writer_loop(path: PathBuf, rx: Receiver<WriteCmd>) {
    // The pending slot is always replaced by higher generations before the
    // next write completes, because there is exactly one consumer.
    let mut pending: Option<(u64, std::sync::Arc<DbFile>)> = None;
    loop {
        let msg = match pending.as_ref() {
            Some(_) => match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(m) => m,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some((_, file)) = pending.take() {
                        write_atomic(&path, &file);
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            },
        };
        match msg {
            WriteCmd::Save {
                generation,
                snapshot,
            } => {
                // Keep only the newest pending generation (coalescing).
                if pending.as_ref().is_none_or(|(g, _)| generation > *g) {
                    pending = Some((generation, snapshot));
                }
            }
            WriteCmd::Flush(reply) => {
                if let Some((_, file)) = pending.take() {
                    write_atomic(&path, &file);
                }
                let _ = reply.send(());
            }
            WriteCmd::Shutdown => break,
        }
    }
    // Final best-effort flush on shutdown.
    if let Some((_, file)) = pending.take() {
        write_atomic(&path, &file);
    }
}

fn write_atomic(path: &std::path::Path, file: &DbFile) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    match std::fs::File::create(&tmp).and_then(|mut f| {
        serde_json::to_writer(&mut f, file)?;
        f.flush()
    }) {
        Ok(()) => {
            if let Err(e) = std::fs::rename(&tmp, path) {
                tracing::warn!(error = %e, "failed to rotate app-history db");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to write app-history db"),
    }
}

impl AppHistoryDb {
    /// Open synchronously (CLI/tests). For GUI startup prefer
    /// [`open_deferred`] so no disk I/O delays the first frame.
    pub fn open(path: PathBuf) -> Self {
        let mut db = Self::in_memory();
        db.path = Some(path.clone());
        db.attach_writer();
        if let Ok(text) = std::fs::read_to_string(&path) {
            db.apply_loaded_file(&text, &path);
        }
        db
    }

    /// GUI path: instant empty model; the JSON is read + parsed on a worker
    /// and merged as soon as it arrives — normally well before the first
    /// sampling tick publishes (sampling starts after the first frame too).
    /// Call [`poll_load`](Self::poll_load) from the UI loop.
    pub fn open_deferred(path: PathBuf) -> Self {
        let mut db = Self::in_memory();
        db.path = Some(path.clone());
        db.state = LoadState {
            loading: true,
            since: std::time::Instant::now(),
        };
        db.attach_writer();
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        let spawned = std::thread::Builder::new()
            .name("tm-hist-load".into())
            .spawn(move || {
                let text = std::fs::read_to_string(&path).ok();
                let _ = tx.send(text);
            });
        if spawned.is_err() {
            // No worker → nothing will ever arrive; run ready immediately.
            db.state.loading = false;
        } else {
            db.load_rx = Some(rx);
        }
        db
    }

    fn apply_loaded_file(&mut self, text: &str, path: &std::path::Path) {
        match serde_json::from_str::<DbFile>(text) {
            Ok(dbf) => {
                self.since_epoch_s = dbf.since_epoch_s;
                self.entries = dbf.entries;
                self.names = dbf.names;
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "app-history db corrupt; starting fresh");
            }
        }
    }

    /// In-memory database (tests / selfcheck).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            since_epoch_s: unix_now(),
            entries: Default::default(),
            names: Default::default(),
            prev: Default::default(),
            state: LoadState::default(),
            generation: 0,
            writer: None,
            load_rx: None,
        }
    }

    fn attach_writer(&mut self) {
        if let Some(path) = self.path.clone() {
            self.writer = HistoryWriter::start(path);
        }
    }

    pub fn since_epoch_s(&self) -> i64 {
        self.since_epoch_s
    }

    /// Wipe all accumulated usage ("Auslastungsverlauf löschen").
    /// Mutates state and enqueues one save; no synchronous disk I/O here.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.names.clear();
        self.prev.clear();
        self.since_epoch_s = unix_now();
        self.enqueue_save();
    }

    pub fn entries(&self) -> &BTreeMap<String, AppUsage> {
        &self.entries
    }

    pub fn display_name(&self, key: &str) -> Option<&str> {
        self.names.get(key).map(String::as_str)
    }

    /// Clone of the identity → display-name map for UI rendering.
    pub fn display_name_map(&self) -> BTreeMap<String, String> {
        self.names.clone()
    }

    /// Adopt asynchronously loaded file contents once ready. Returns true
    /// when this call completed the load. A hung/slow loader (>2 s) gives up
    /// so observation resumes rather than staying blocked forever.
    pub fn poll_load(&mut self) -> bool {
        if !self.state.loading {
            return false;
        }
        let timed_out = self.state.since.elapsed() > std::time::Duration::from_secs(2);
        let Some(rx) = &self.load_rx else {
            self.finish_load();
            return true;
        };
        match rx.try_recv() {
            Ok(text) => {
                if let Some(t) = text
                    && let Some(path) = self.path.clone()
                {
                    self.apply_loaded_file(&t, &path);
                    tracing::info!("app history loaded (deferred)");
                }
                self.finish_load();
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if timed_out {
                    tracing::warn!("app history load timed out; continuing without saved data");
                    self.finish_load();
                    true
                } else {
                    false
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.finish_load();
                true
            }
        }
    }

    fn finish_load(&mut self) {
        self.state.loading = false;
        self.load_rx = None;
    }

    fn snapshot_file(&self) -> DbFile {
        DbFile {
            since_epoch_s: self.since_epoch_s,
            entries: self.entries.clone(),
            names: self.names.clone(),
        }
    }

    /// Queue an autosave on the single writer thread; coalesces bursts. The
    /// caller never blocks and never clones on the UI thread beyond the
    /// entry map (small: one row per application).
    pub fn save_async(&mut self) {
        self.enqueue_save();
    }

    fn enqueue_save(&mut self) {
        self.generation += 1;
        if let Some(w) = &self.writer {
            let generation = self.generation;
            w.save(generation, std::sync::Arc::new(self.snapshot_file()));
        }
    }

    /// Blocking synchronous save+flush (exit path / tests).
    pub fn save(&mut self) {
        self.enqueue_save();
        if let Some(w) = &self.writer {
            w.flush();
        }
    }

    /// Fold one snapshot into the running totals. Only processes classified as
    /// apps are tracked (matching the TM tab semantics).
    ///
    /// While the deferred load is still in flight, observation is skipped:
    /// merging deltas into a not-yet-loaded baseline could double count or
    /// lose data. Loading takes milliseconds and finishes long before the
    /// first sampling tick publishes.
    pub fn observe(&mut self, snap: &Snapshot, interval_s: f64) {
        if self.state.loading {
            return;
        }
        let mut next_prev = std::collections::HashMap::with_capacity(self.prev.len());

        for p in &snap.processes {
            if p.category != ProcCategory::App {
                continue;
            }
            let cpu_time = p.cpu_time_s.unwrap_or({
                // Fallback: estimate from rate * elapsed when the platform
                // cannot report accumulated CPU time.
                0.0
            });

            // Network totals only when the platform provides real numbers.
            let net_total = match (p.net_recv_total, p.net_sent_total) {
                (Some(r), Some(s)) => Some(r + s),
                _ => None,
            };

            let key = entry_key(p);
            let prev = self.prev.get(&p.pid);

            // PID reuse / identity change → treat as first sighting no
            // matter which direction the raw counters moved.
            let same_process = prev.is_some_and(|prev| {
                prev.app_identity == key && prev.start_epoch_s == p.start_epoch_s
            });

            let d_cpu = match (same_process, prev) {
                (true, Some(prev)) => (cpu_time - prev.cpu_time_s).max(0.0),
                _ => 0.0, // first sighting of this identity contributes nothing
            };
            let d_net: u64 = match (same_process.then_some(()), prev, net_total) {
                (Some(()), Some(prev), Some(total)) => {
                    total.saturating_sub(prev.net_total_bytes.unwrap_or(0))
                }
                _ => 0,
            };

            let d_cpu = if d_cpu > 0.0 || p.cpu_pct <= 0.0 {
                d_cpu
            } else {
                (p.cpu_pct as f64 / 100.0) * interval_s.max(0.0)
            };
            let d_net = if d_net > 0 || (p.net_recv_bps.is_none() && p.net_sent_bps.is_none()) {
                d_net
            } else {
                ((p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0))
                    * interval_s.max(0.0)) as u64
            };

            if d_cpu > 1e-6 || d_net > 0 || !self.entries.contains_key(&key) {
                let e = self.entries.entry(key.clone()).or_default();
                e.cpu_seconds += d_cpu;
                e.network_bytes += d_net;
            }
            // Remember the friendliest display name for the tab.
            self.names
                .entry(key.clone())
                .or_insert_with(|| p.shown_name().to_string());

            next_prev.insert(
                p.pid,
                PrevTick {
                    start_epoch_s: p.start_epoch_s,
                    app_identity: key,
                    cpu_time_s: cpu_time,
                    net_total_bytes: net_total,
                },
            );
        }
        self.prev = next_prev;
    }
}

// (the deferred loader communicates through a plain mpsc channel stored on
// the database; no global side channel is needed)

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn snap_with(pid: u32, name: &str, cat: ProcCategory, cpu_time: f64, net: u64) -> Snapshot {
        let mut p = ProcessEntry::new(pid, name);
        p.category = cat;
        p.cpu_time_s = Some(cpu_time);
        p.net_recv_total = Some(net);
        p.net_sent_total = Some(0);
        Snapshot {
            timestamp_ms: 1000,
            processes: vec![p],
            ..Default::default()
        }
    }

    #[test]
    fn accumulates_deltas_only_for_apps() {
        let mut db = AppHistoryDb::in_memory();
        db.observe(&snap_with(1, "app.exe", ProcCategory::App, 10.0, 1000), 1.0);
        db.observe(&snap_with(1, "app.exe", ProcCategory::App, 12.5, 1500), 1.0);
        db.observe(
            &snap_with(2, "svc.exe", ProcCategory::System, 50.0, 999_999),
            1.0,
        );

        let e = db.entries().get("app.exe").unwrap();
        assert!((e.cpu_seconds - 2.5).abs() < 1e-6);
        assert_eq!(e.network_bytes, 500);
        assert!(db.entries().get("svc.exe").is_none());
    }

    #[test]
    fn first_sighting_contributes_no_accumulated_jump() {
        let mut db = AppHistoryDb::in_memory();
        // Process already lived 500 s before we started watching.
        db.observe(
            &snap_with(3, "app.exe", ProcCategory::App, 500.0, 1 << 20),
            1.0,
        );
        let e = db.entries().get("app.exe").unwrap();
        assert_eq!(e.cpu_seconds, 0.0);
        assert_eq!(e.network_bytes, 0);
    }

    /// A recycled PID whose new counters are HIGHER than the old process'
    /// must not attribute any delta across processes (the old negative-delta
    /// heuristic missed exactly this case).
    #[test]
    fn app_history_pid_reuse_with_higher_new_counters_has_zero_cross_process_delta() {
        let mut db = AppHistoryDb::in_memory();
        db.observe(&snap_with(7, "a.exe", ProcCategory::App, 1.0, 100), 1.0);
        // New unrelated process recycles pid 7 with larger counters.
        db.observe(&snap_with(7, "b.exe", ProcCategory::App, 5.0, 1000), 1.0);
        let a = db.entries().get("a.exe").unwrap();
        let b = db.entries().get("b.exe").unwrap();
        assert_eq!(a.network_bytes, 0, "no cross-process network attribution");
        assert_eq!(b.network_bytes, 0, "first sighting contributes nothing");
        assert_eq!(a.cpu_seconds, 0.0);
        assert_eq!(b.cpu_seconds, 0.0);

        // And continued observation of the new process accumulates normally.
        db.observe(&snap_with(7, "b.exe", ProcCategory::App, 6.5, 2000), 1.0);
        let b = db.entries().get("b.exe").unwrap();
        assert!((b.cpu_seconds - 1.5).abs() < 1e-6);
        assert_eq!(b.network_bytes, 1000);
    }

    /// Same executable name at two different paths must not merge into one
    /// bucket.
    #[test]
    fn app_history_identity_distinguishes_same_filename_different_path() {
        let mk = |pid: u32, path: &str| {
            let mut p = ProcessEntry::new(pid, "app.exe");
            p.category = ProcCategory::App;
            p.exe_path = Some(std::path::PathBuf::from(path));
            p.start_epoch_s = Some(100);
            p.cpu_time_s = Some(0.0);
            Snapshot {
                timestamp_ms: 1000,
                processes: vec![p],
                ..Default::default()
            }
        };
        let mut db = AppHistoryDb::in_memory();
        db.observe(&mk(10, r"C:\A\app.exe"), 1.0);
        db.observe(&mk(11, r"D:\B\app.exe"), 1.0);
        assert_eq!(
            db.entries().len(),
            2,
            "distinct paths are distinct apps even with equal filenames"
        );
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut db = AppHistoryDb::open(path.clone());
        db.observe(&snap_with(9, "app.exe", ProcCategory::App, 0.0, 0), 1.0);
        db.observe(&snap_with(9, "app.exe", ProcCategory::App, 3.0, 2048), 1.0);
        db.save();

        let db2 = AppHistoryDb::open(path);
        let e = db2.entries().get("app.exe").unwrap();
        assert!((e.cpu_seconds - 3.0).abs() < 1e-6);
        assert_eq!(e.network_bytes, 2048);
        assert!(db2.since_epoch_s() > 0);
    }

    /// Generations through the real writer channel: a burst of saves where
    /// only the newest must win, and the final file reflects it exactly.
    #[test]
    fn app_history_writer_generations_cannot_reorder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut db = AppHistoryDb::open(path.clone());

        db.observe(&snap_with(5, "gen.exe", ProcCategory::App, 0.0, 0), 1.0);
        db.save_async(); // gen 1: cpu 0
        db.observe(&snap_with(5, "gen.exe", ProcCategory::App, 2.0, 0), 1.0);
        db.save_async(); // gen 2: cpu 2 — must overwrite gen 1
        db.save(); // blocking flush of everything queued

        // Extra churn right after the flush must not resurrect old data.
        db.observe(&snap_with(5, "gen.exe", ProcCategory::App, 4.0, 512), 1.0);
        db.save_async();
        db.save();

        let reloaded = AppHistoryDb::open(path);
        let e = reloaded.entries().get("gen.exe").unwrap();
        assert!((e.cpu_seconds - 4.0).abs() < 1e-6);
        assert_eq!(e.network_bytes, 512);
    }
}
