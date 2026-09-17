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
            let previous = self.prev.get(&process.pid).filter(|prev| {
                process.start_epoch_s.is_none() || prev.start_epoch_s == process.start_epoch_s
            });
            if let Some(prev) = previous {
                let cpu_delta_ms = (cpu_s - prev.cpu_s).max(0.0) * 1000.0;
                let io_delta = io_bytes.saturating_sub(prev.io_bytes);
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
            // A process first seen INSIDE the window started during startup:
            // its first delta (measured against this baseline) carries its
            // cost, so nothing special is needed beyond recording the baseline.
            live.insert(
                process.pid,
                PrevCounters {
                    start_epoch_s: process.start_epoch_s,
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

impl ImpactStore {
    /// Load from `path`; a missing or corrupt file yields an empty store
    /// rather than an error — losing measurements must never block startup.
    pub fn open(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<StoreFile>(&text).ok())
            .map(|file| file.entries)
            .unwrap_or_default();
        Self {
            entries,
            path: Some(path),
            dirty: false,
        }
    }

    /// A store that never touches disk (tests, and platforms with no data dir).
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
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return false;
    }
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::File::create(&tmp).and_then(|mut handle| {
        use std::io::Write as _;
        serde_json::to_writer(&mut handle, file)?;
        handle.flush()
    });
    match written {
        Ok(()) => match std::fs::rename(&tmp, path) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "failed to rotate startup-impact store");
                false
            }
        },
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
}
