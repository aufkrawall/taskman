//! Measured startup impact for the Startup tab.
//!
//! Task Manager's "Startup impact" is a CPU-time and disk-I/O measurement
//! taken while an app starts — Microsoft documents the thresholds (Low:
//! < 300 ms CPU AND < 292 KB disk; High: > 1 s CPU OR > 3 MB disk; Medium in
//! between) but not where Windows stores the per-entry data: it is NOT in
//! SRUM, whose tables carry hourly aggregates for App History, not startup
//! resource use. So the honest implementation measures it where the data
//! already exists: this app's own sampler, which reports each process's CPU
//! time and I/O bytes from the kernel's process table.
//!
//! What that means concretely:
//!
//! * The tracker folds per-tick DELTAS of `cpu_time_s` and
//!   `disk_read_total + disk_write_total` into per-image-path totals for the
//!   whole startup window (the first [`STARTUP_WINDOW_S`] seconds of uptime).
//! * It can only measure a startup the app was running for. Started later —
//!   or installed afterwards — the column stays "Not measured" until the next
//!   boot. Nothing is extrapolated from what happened to be running.
//! * A process that starts and exits between two ticks is not attributed;
//!   the alternative would be guessing which image it was.
//!
//! Results are keyed by the same normalized image path the settings file uses
//! for per-image rules ([`crate::settings::process_rule_key`]), so a startup
//! item's command line can be resolved to the process it launched.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Snapshot, StartupImpact};

/// Length of the window measured after boot. Long enough to cover a slow
/// sign-in's app launches, short enough that the numbers stay "what this app
/// cost at startup" rather than "what it has used since".
pub const STARTUP_WINDOW_S: u64 = 120;

/// One image's accumulated startup cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ImpactSample {
    pub cpu_ms: f64,
    pub disk_bytes: u64,
    /// Boot epoch this measurement belongs to, so the UI can say when.
    pub boot_epoch_s: i64,
}

impl ImpactSample {
    /// Microsoft's documented classification.
    pub fn classify(&self) -> StartupImpact {
        classify(self.cpu_ms, self.disk_bytes)
    }
}

/// Low/Medium/High per the documented thresholds. High wins over Low when the
/// two signals disagree, because either one can be the reason a boot feels
/// slow.
pub fn classify(cpu_ms: f64, disk_bytes: u64) -> StartupImpact {
    const LOW_CPU_MS: f64 = 300.0;
    const LOW_DISK_BYTES: u64 = 292 * 1024;
    const HIGH_CPU_MS: f64 = 1000.0;
    const HIGH_DISK_BYTES: u64 = 3 * 1024 * 1024;

    if cpu_ms > HIGH_CPU_MS || disk_bytes > HIGH_DISK_BYTES {
        StartupImpact::High
    } else if cpu_ms < LOW_CPU_MS && disk_bytes < LOW_DISK_BYTES {
        StartupImpact::Low
    } else {
        StartupImpact::Medium
    }
}

/// Per-process cumulative counters as of the previous observation.
#[derive(Debug, Clone, Copy)]
struct PrevCounters {
    start_epoch_s: Option<i64>,
    cpu_s: f64,
    io_bytes: u64,
}

/// Folds startup-window samples into per-image totals.
#[derive(Debug, Default)]
pub struct StartupImpactTracker {
    window_s: u64,
    boot_epoch_s: Option<i64>,
    totals: HashMap<String, ImpactSample>,
    prev: HashMap<u32, PrevCounters>,
    /// False once the window closed (or the app started too late to see one).
    active: bool,
    dirty: bool,
}

impl StartupImpactTracker {
    /// `uptime_s` and `boot_epoch_s` come from the first snapshot: the window
    /// is open only while the machine is still inside its startup.
    pub fn new(uptime_s: u64, boot_epoch_s: i64) -> Self {
        Self {
            window_s: STARTUP_WINDOW_S,
            boot_epoch_s: Some(boot_epoch_s),
            totals: HashMap::new(),
            prev: HashMap::new(),
            active: uptime_s < STARTUP_WINDOW_S,
            dirty: false,
        }
    }

    /// Override the window length (tests).
    pub fn with_window_s(mut self, window_s: u64) -> Self {
        self.window_s = window_s;
        self
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn boot_epoch_s(&self) -> Option<i64> {
        self.boot_epoch_s
    }

    /// Feed one snapshot. Returns true when anything changed (the caller then
    /// knows the store is worth persisting).
    pub fn observe(&mut self, snapshot: &Snapshot) -> bool {
        if !self.active {
            return false;
        }
        if snapshot.system.uptime_s >= self.window_s {
            self.active = false;
            return self.dirty;
        }
        let mut changed = false;
        let mut live: HashMap<u32, PrevCounters> = HashMap::with_capacity(snapshot.processes.len());
        for process in &snapshot.processes {
            if process.synthetic {
                continue;
            }
            let Some(path) = process.exe_path.as_deref() else {
                continue;
            };
            let (Some(cpu_s), Some(io_bytes)) = (
                process.cpu_time_s,
                process
                    .disk_read_total
                    .checked_add(process.disk_write_total),
            ) else {
                continue;
            };
            let key = crate::settings::process_rule_key(path);
            // A recycled pid must not bridge two different processes: without
            // a matching start time the baseline is reset, never subtracted.
            // `None == None` still matches, so a platform that cannot publish
            // a creation token keeps accumulating deltas for a stable process.
            let previous = self
                .prev
                .get(&process.pid)
                .filter(|prev| prev.start_epoch_s == process.creation_identity());
            // A process first seen here that STARTED after boot spent its
            // whole life inside the window, so its counters are charged from
            // zero. Recording only a baseline lost everything it did before
            // this tracker first saw it — the burst of an app that launches,
            // reads its files and goes idle within one tick, and the whole
            // startup of anything that started before TaskMan did. A process
            // older than boot (or of unknown age) only contributes deltas.
            let started_after_boot = matches!(
                (process.start_epoch_s, self.boot_epoch_s),
                (Some(start), Some(boot))
                    if crate::model::epoch_timestamp_to_unix_seconds(start) >= boot
            );
            let baseline = match previous {
                Some(prev) => Some((prev.cpu_s, prev.io_bytes)),
                None if started_after_boot => Some((0.0, 0)),
                None => None,
            };
            if let Some((base_cpu_s, base_io_bytes)) = baseline {
                let cpu_delta_ms = (cpu_s - base_cpu_s).max(0.0) * 1000.0;
                let io_delta = io_bytes.saturating_sub(base_io_bytes);
                if cpu_delta_ms > 0.0 || io_delta > 0 {
                    let entry = self.totals.entry(key).or_insert(ImpactSample {
                        boot_epoch_s: self.boot_epoch_s.unwrap_or(0),
                        ..Default::default()
                    });
                    entry.cpu_ms += cpu_delta_ms;
                    entry.disk_bytes = entry.disk_bytes.saturating_add(io_delta);
                    changed = true;
                }
            }
            live.insert(
                process.pid,
                PrevCounters {
                    start_epoch_s: process.creation_identity(),
                    cpu_s,
                    io_bytes,
                },
            );
        }
        self.prev = live;
        self.dirty |= changed;
        changed
    }

    /// The accumulated per-image totals (keys are [`process_rule_key`] values).
    pub fn totals(&self) -> &HashMap<String, ImpactSample> {
        &self.totals
    }
}

/// What the startup-impact store holds, as persisted.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, ImpactSample>,
}

/// Persisted measurements, shared by the Startup tab.
#[derive(Debug)]
pub struct ImpactStore {
    entries: BTreeMap<String, ImpactSample>,
    path: Option<PathBuf>,
    dirty: bool,
}

/// Cap on the store read, the same one the app-history store uses: the file is
/// a map of small records, and this load runs on the deferred startup path in
/// the GUI process, so a corrupt or hostile multi-gigabyte file must fail like a
/// corrupt one instead of being allocated whole.
const MAX_STORE_BYTES: usize = 16 * 1024 * 1024;

/// The store's text, at most [`MAX_STORE_BYTES`].
///
/// A file over the cap is an `InvalidData` error rather than an unbounded
/// allocation, so [`ImpactStore::open`] handles it through the same arm that
/// keeps a structurally corrupt file aside: it is real bytes the user may
/// still want to recover, not something to delete.
fn read_store_text(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(8 * 1024);
    file.take((MAX_STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STORE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} exceeds the {MAX_STORE_BYTES}-byte limit",
                path.display()
            ),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

impl ImpactStore {
    /// Load from `path`; a missing or corrupt file yields an empty store
    /// rather than an error — losing measurements must never block startup.
    /// A corrupt file with bytes in it is kept aside rather than overwritten;
    /// one that says nothing — no bytes, whitespace only, or NUL-filled — is
    /// removed instead, since moving it aside would only take the `.bad` name
    /// from a copy that still has measurements. A file larger than
    /// [`MAX_STORE_BYTES`] counts as corrupt: it is kept aside, never read
    /// whole.
    pub fn open(path: PathBuf) -> Self {
        let entries = match read_store_text(&path) {
            Ok(text) => match serde_json::from_str::<StoreFile>(&text) {
                Ok(file) => file.entries,
                Err(_) => {
                    if crate::persist::says_nothing(&text) {
                        crate::persist::discard_contentless(&path, "startup-impact store");
                    } else {
                        crate::persist::set_aside(&path, "startup-impact store");
                    }
                    BTreeMap::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                // Name the reason before `set_aside` logs what it did: an
                // oversized or non-UTF-8 store is otherwise indistinguishable
                // from a permissions problem when someone reports that their
                // measurements keep disappearing.
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "startup-impact store could not be read; keeping it aside"
                );
                crate::persist::set_aside(&path, "startup-impact store");
                BTreeMap::new()
            }
        };
        Self {
            entries,
            path: Some(path),
            dirty: false,
        }
    }

    /// A store that never touches disk (tests, `--mock`, and platforms with
    /// no data dir).
    pub fn in_memory() -> Self {
        Self {
            entries: BTreeMap::new(),
            path: None,
            dirty: false,
        }
    }

    pub fn get(&self, key: &str) -> Option<&ImpactSample> {
        self.entries.get(key)
    }

    /// Merge one boot's measurements in. Entries from an older boot are
    /// replaced per image: the column shows the most recent startup.
    pub fn merge(&mut self, totals: &HashMap<String, ImpactSample>) {
        for (key, sample) in totals {
            let replace = self
                .entries
                .get(key)
                .is_none_or(|existing| existing.boot_epoch_s <= sample.boot_epoch_s);
            if replace {
                self.entries.insert(key.clone(), *sample);
                self.dirty = true;
            }
        }
        // Drop entries that no longer resolve to a plausible measurement; a
        // file that grows forever with dead images would never be read again.
        self.entries
            .retain(|_, sample| sample.cpu_ms > 0.0 || sample.disk_bytes > 0);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write synchronously (shutdown path; the file is small).
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = self.path.clone() else {
            self.dirty = false;
            return;
        };
        let file = StoreFile {
            version: 1,
            entries: self.entries.clone(),
        };
        if write_atomic(&path, &file) {
            self.dirty = false;
        }
    }
}

fn write_atomic(path: &Path, file: &StoreFile) -> bool {
    let written = crate::persist::replace_file(path, "json.tmp", |out| {
        serde_json::to_writer(out, file).map_err(std::io::Error::from)
    });
    match written {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "failed to write startup-impact store");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A one-process snapshot at 10 s uptime with cumulative counters.
    fn snapshot_with(cpu_s: f64, io_bytes: u64, uptime_s: u64) -> Snapshot {
        let mut snap = crate::mock::snapshot(1);
        snap.system.uptime_s = uptime_s;
        snap.system.boot_epoch_s = 1_000;
        snap.processes.truncate(1);
        let process = &mut snap.processes[0];
        process.pid = 42;
        process.start_epoch_s = Some(500);
        process.exe_path = Some(PathBuf::from(r"C:\Apps\Launcher.EXE"));
        process.synthetic = false;
        process.cpu_time_s = Some(cpu_s);
        process.disk_read_total = io_bytes;
        process.disk_write_total = 0;
        snap
    }

    #[test]
    fn tracker_accumulates_deltas_only_inside_the_window() {
        let key = crate::settings::process_rule_key(Path::new(r"C:\Apps\Launcher.EXE"));
        let first = snapshot_with(1.0, 1_000, 10);
        let mut tracker = StartupImpactTracker::new(first.system.uptime_s, 1_000);
        assert!(tracker.is_active());
        assert!(
            !tracker.observe(&first),
            "the first tick only records baselines - a process that was already \
             running at startup must not be charged its lifetime total"
        );

        // 250 ms CPU and 4 KiB more I/O since the baseline.
        assert!(tracker.observe(&snapshot_with(1.25, 5_096, 11)));
        let sample = tracker.totals().get(&key).copied().expect("measured");
        assert!((sample.cpu_ms - 250.0).abs() < 0.001, "{sample:?}");
        assert_eq!(sample.disk_bytes, 4_096);
        assert_eq!(sample.boot_epoch_s, 1_000);

        // Once uptime passes the window, nothing further is accumulated even
        // though the counters keep growing.
        assert!(tracker.observe(&snapshot_with(9.9, 900_000, STARTUP_WINDOW_S + 1)));
        assert!(!tracker.is_active());
        assert!(!tracker.observe(&snapshot_with(20.0, 9_900_000, STARTUP_WINDOW_S + 60)));
        assert!((tracker.totals().get(&key).unwrap().cpu_ms - 250.0).abs() < 0.001);
    }

    /// An app that started after boot is charged everything it did, not only
    /// what happened after TaskMan first saw it: a launcher that reads 5 MB
    /// in its first second and then idles is a High-impact startup item.
    #[test]
    fn a_process_started_after_boot_is_charged_from_its_start() {
        let key = crate::settings::process_rule_key(Path::new(r"C:\Apps\Launcher.EXE"));
        let mut first = snapshot_with(0.8, 5 * 1024 * 1024, 30);
        first.processes[0].start_epoch_s = Some(1_020);
        let mut tracker = StartupImpactTracker::new(first.system.uptime_s, 1_000);
        assert!(tracker.observe(&first));
        let sample = tracker.totals().get(&key).copied().expect("measured");
        assert!((sample.cpu_ms - 800.0).abs() < 0.001, "{sample:?}");
        assert_eq!(sample.disk_bytes, 5 * 1024 * 1024);
        assert_eq!(sample.classify(), StartupImpact::High);

        // Later ticks add only the deltas.
        let mut second = snapshot_with(0.9, 5 * 1024 * 1024, 31);
        second.processes[0].start_epoch_s = Some(1_020);
        assert!(tracker.observe(&second));
        assert!((tracker.totals()[&key].cpu_ms - 900.0).abs() < 0.001);
    }

    #[test]
    fn a_recycled_pid_does_not_bridge_two_processes() {
        let first = snapshot_with(1.0, 1_000, 10);
        let mut tracker = StartupImpactTracker::new(first.system.uptime_s, 1_000);
        tracker.observe(&first);

        // Same pid, new creation time: the baseline must reset instead of
        // charging the difference between two unrelated lifetimes.
        let mut reused = snapshot_with(500.0, 10_000_000, 11);
        reused.processes[0].start_epoch_s = Some(900);
        assert!(!tracker.observe(&reused));
        assert!(tracker.totals().is_empty());
    }

    /// Regression: an unknown *current* identity (the Linux/macOS bracket
    /// deliberately publishes `None` when the before/after reads disagree)
    /// must reset the baseline, not keep the previous process's counters and
    /// subtract them from the new occupant of the pid.
    #[test]
    fn an_unknown_current_identity_does_not_reuse_the_old_baseline() {
        let first = snapshot_with(1.0, 1_000, 10);
        let mut tracker = StartupImpactTracker::new(first.system.uptime_s, 1_000);
        tracker.observe(&first);

        let mut reused = snapshot_with(500.0, 10_000_000, 11);
        reused.processes[0].start_epoch_s = None;
        reused.processes[0].boot_start_ticks = None;
        assert!(
            !tracker.observe(&reused),
            "an identity-less current process must not be charged the \
             previous occupant's lifetime difference"
        );
        assert!(tracker.totals().is_empty());
    }

    #[test]
    fn classification_matches_the_documented_thresholds() {
        // Low: below both.
        assert_eq!(classify(299.0, 292 * 1024 - 1), StartupImpact::Low);
        assert_eq!(classify(0.0, 0), StartupImpact::Low);
        // Medium: at or above one, below the high bars.
        assert_eq!(classify(300.0, 0), StartupImpact::Medium);
        assert_eq!(classify(0.0, 292 * 1024), StartupImpact::Medium);
        assert_eq!(classify(1000.0, 0), StartupImpact::Medium);
        // High: above either.
        assert_eq!(classify(1000.1, 0), StartupImpact::High);
        assert_eq!(classify(0.0, 3 * 1024 * 1024 + 1), StartupImpact::High);
        // Disagreeing signals: the slower one decides.
        assert_eq!(classify(5.0, 4 * 1024 * 1024), StartupImpact::High);
        assert_eq!(classify(5.0, 1000), StartupImpact::Low);
    }

    #[test]
    fn store_round_trips_and_replaces_older_boots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("startup-impact.json");
        let mut store = ImpactStore::open(path.clone());
        assert!(store.is_empty());

        let mut first = HashMap::new();
        first.insert(
            r"c:\apps\launcher.exe".to_string(),
            ImpactSample {
                cpu_ms: 1200.0,
                disk_bytes: 10,
                boot_epoch_s: 100,
            },
        );
        store.merge(&first);
        store.save();
        let reloaded = ImpactStore::open(path.clone());
        assert_eq!(
            reloaded.get(r"c:\apps\launcher.exe").map(|s| s.classify()),
            Some(StartupImpact::High)
        );

        // A newer boot's value replaces the old one.
        let mut second = HashMap::new();
        second.insert(
            r"c:\apps\launcher.exe".to_string(),
            ImpactSample {
                cpu_ms: 50.0,
                disk_bytes: 1000,
                boot_epoch_s: 200,
            },
        );
        let mut store = ImpactStore::open(path);
        store.merge(&second);
        assert_eq!(
            store.get(r"c:\apps\launcher.exe").map(|s| s.classify()),
            Some(StartupImpact::Low)
        );
        // ...and an older boot's value does not.
        let mut stale = HashMap::new();
        stale.insert(
            r"c:\apps\launcher.exe".to_string(),
            ImpactSample {
                cpu_ms: 900.0,
                disk_bytes: 0,
                boot_epoch_s: 150,
            },
        );
        store.merge(&stale);
        assert_eq!(
            store.get(r"c:\apps\launcher.exe").map(|s| s.cpu_ms),
            Some(50.0)
        );
    }

    /// The store's own load decides this, not the size: whitespace has no
    /// entries to recover, so the file is removed where it lies and the copy an
    /// earlier failure kept stays byte-identical.
    #[test]
    fn a_whitespace_only_store_leaves_an_existing_aside_copy_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("startup-impact.json");
        let aside = dir.path().join("startup-impact.json.bad");
        std::fs::write(&aside, b"the last recoverable copy").unwrap();
        std::fs::write(&path, " \r\n\t \r\n").unwrap();

        let store = ImpactStore::open(path.clone());

        assert!(store.is_empty());
        assert!(
            !path.exists(),
            "the unusable file must not be left for the next load"
        );
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "the last recoverable copy",
            "a store that says nothing must not replace the copy kept aside earlier"
        );
    }

    /// `path` holding a valid, empty store padded with spaces to exactly `len`
    /// bytes. The JSON is real at any size, so the only thing a load can fail on
    /// is the cap.
    fn write_padded_store(path: &Path, len: usize) {
        use std::io::Write;

        let head = b"{\"version\":1,\"entries\":{}}";
        assert!(len >= head.len(), "{len} cannot hold the head");
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(head).unwrap();
        let padding = [b' '; 64 * 1024];
        let mut written = head.len();
        while written < len {
            let chunk = (len - written).min(padding.len());
            file.write_all(&padding[..chunk]).unwrap();
            written += chunk;
        }
        file.flush().unwrap();
    }

    /// The cap is exact: a store of exactly [`MAX_STORE_BYTES`] still loads, one
    /// byte more is kept aside. That `>` is the whole difference between "read
    /// it" and "don't", so both sides of it are pinned — a future `>=` slip
    /// fails here instead of quietly refusing every full-size store.
    ///
    /// The oversized half is also the regression: the file used to be read with
    /// `read_to_string`, i.e. allocated whole on the GUI's deferred startup path,
    /// so a runaway or hostile file could exhaust memory before the corrupt-file
    /// branch that keeps it aside was ever reached.
    #[test]
    fn the_store_read_cap_is_exact_and_an_oversized_file_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("startup-impact.json");
        let aside = dir.path().join("startup-impact.json.bad");

        write_padded_store(&path, MAX_STORE_BYTES);
        let store = ImpactStore::open(path.clone());
        assert!(store.is_empty(), "the padded store holds no entries");
        assert!(
            path.exists(),
            "a store of exactly the cap is read, not kept aside"
        );
        assert!(
            !aside.exists(),
            "a readable store leaves no recovery copy behind"
        );

        std::fs::remove_file(&path).unwrap();
        write_padded_store(&path, MAX_STORE_BYTES + 1);
        let store = ImpactStore::open(path.clone());
        assert!(store.is_empty());
        assert!(
            !path.exists(),
            "a file the loader refuses must not stay where the next save writes"
        );
        assert_eq!(
            std::fs::metadata(&aside).unwrap().len(),
            (MAX_STORE_BYTES + 1) as u64,
            "real bytes are kept for recovery, not deleted"
        );
    }
}
