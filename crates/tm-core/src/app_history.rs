//! Persistent per-application resource usage history ("App history" tab).
//!
//! Mirrors the idea of Windows Task Manager's app history: cumulative CPU time
//! and network traffic per application since a given date, surviving restarts.
//! We track our own measurements (unlike TM which mines SRUM), so the date
//! shown is "since" our first observation.

use crate::model::{ProcCategory, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppUsage {
    pub cpu_seconds: f64,
    pub network_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DbFile {
    /// Epoch seconds when tracking started.
    since_epoch_s: i64,
    /// Keyed by lowercase executable name.
    entries: BTreeMap<String, AppUsage>,
}

pub struct AppHistoryDb {
    path: Option<PathBuf>,
    since_epoch_s: i64,
    entries: BTreeMap<String, AppUsage>,
    /// Previous tick's accumulated cpu-time/net-totals keyed by pid for delta computation.
    prev: std::collections::HashMap<u32, PrevTick>,
}

#[derive(Debug, Clone, Copy)]
struct PrevTick {
    cpu_time_s: f64,
    net_total_bytes: u64,
}

impl AppHistoryDb {
    pub fn open(path: PathBuf) -> Self {
        let (since, entries) = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<DbFile>(&text) {
                Ok(db) => (db.since_epoch_s, db.entries),
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "app-history db corrupt; starting fresh");
                    (unix_now(), Default::default())
                }
            },
            Err(_) => (unix_now(), Default::default()),
        };
        Self {
            path: Some(path),
            since_epoch_s: since,
            entries,
            prev: Default::default(),
        }
    }

    /// In-memory database (tests / selfcheck).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            since_epoch_s: unix_now(),
            entries: Default::default(),
            prev: Default::default(),
        }
    }

    pub fn since_epoch_s(&self) -> i64 {
        self.since_epoch_s
    }

    pub fn entries(&self) -> &BTreeMap<String, AppUsage> {
        &self.entries
    }

    /// Fold one snapshot into the running totals. Only processes classified as
    /// apps are tracked (matching the TM tab semantics).
    pub fn observe(&mut self, snap: &Snapshot, interval_s: f64) {
        let mut next_prev = std::collections::HashMap::with_capacity(self.prev.len());

        for p in &snap.processes {
            if p.category != ProcCategory::App {
                continue;
            }
            let cpu_time = p.cpu_time_s.unwrap_or_else(|| {
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
            let prev = self.prev.get(&p.pid).copied();

            let d_cpu = match prev {
                Some(prev) => (cpu_time - prev.cpu_time_s).max(0.0),
                None => 0.0, // first sighting of this pid contributes nothing
            };
            let d_net: u64 = match (prev, net_total) {
                (Some(prev), Some(total)) => total.saturating_sub(prev.net_total_bytes),
                _ => 0,
            };

            let d_cpu = if d_cpu > 0.0 || p.cpu_pct <= 0.0 {
                d_cpu
            } else {
                (p.cpu_pct as f64 / 100.0) * interval_s.max(0.0)
            };
            let d_net = if d_net > 0 || (p.net_recv_bps.is_none() && p.net_sent_bps.is_none())
            {
                d_net
            } else {
                ((p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0))
                    * interval_s.max(0.0)) as u64
            };

            if d_cpu > 1e-6 {
                self.entries.entry(key.clone()).or_default().cpu_seconds += d_cpu;
            }
            if d_net > 0 {
                self.entries.entry(key.clone()).or_default().network_bytes += d_net;
            }
            // Ensure every observed app shows up in the table even at zero usage.
            self.entries.entry(key).or_default();

            next_prev.insert(
                p.pid,
                PrevTick {
                    cpu_time_s: cpu_time,
                    net_total_bytes: net_total.unwrap_or(0),
                },
            );
        }
        self.prev = next_prev;
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        let file = DbFile {
            since_epoch_s: self.since_epoch_s,
            entries: self.entries.clone(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        match std::fs::File::create(&tmp).and_then(|mut f| {
            serde_json::to_writer(&mut f, &file)?;
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
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn entry_key(p: &crate::model::ProcessEntry) -> String {
    if let Some(path) = &p.exe_path {
        if let Some(name) = path.file_name() {
            return name.to_string_lossy().to_ascii_lowercase();
        }
    }
    p.name.to_ascii_lowercase()
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
        db.observe(&snap_with(2, "svc.exe", ProcCategory::System, 50.0, 999_999), 1.0);

        let e = db.entries().get("app.exe").unwrap();
        assert!((e.cpu_seconds - 2.5).abs() < 1e-6);
        assert_eq!(e.network_bytes, 500);
        assert!(db.entries().get("svc.exe").is_none());
    }

    #[test]
    fn first_sighting_contributes_no_accumulated_jump() {
        let mut db = AppHistoryDb::in_memory();
        // Process already lived 500 s before we started watching.
        db.observe(&snap_with(3, "app.exe", ProcCategory::App, 500.0, 1 << 20), 1.0);
        let e = db.entries().get("app.exe").unwrap();
        assert_eq!(e.cpu_seconds, 0.0);
        assert_eq!(e.network_bytes, 0);
    }

    #[test]
    fn pid_reuse_does_not_create_negative_deltas() {
        let mut db = AppHistoryDb::in_memory();
        db.observe(&snap_with(7, "a.exe", ProcCategory::App, 100.0, 10_000), 1.0);
        // New process with recycled pid starts from low counters again.
        db.observe(&snap_with(7, "b.exe", ProcCategory::App, 1.0, 200), 1.0);
        let a = db.entries().get("a.exe").unwrap();
        let b = db.entries().get("b.exe").unwrap();
        assert_eq!(a.network_bytes, 0); // negative delta clamps to nothing
        assert_eq!(b.cpu_seconds, 0.0);
        assert_eq!(b.network_bytes, 0);
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
}
