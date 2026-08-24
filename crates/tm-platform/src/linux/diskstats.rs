//! /proc/diskstats parsing with delta tracking.

use parking_lot::Mutex;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct DiskStat {
    pub device: String,
    pub reads_completed: u64,
    pub writes_completed: u64,
    pub read_sectors: u64,
    pub write_sectors: u64,
    /// Milliseconds spent doing I/O since boot.
    pub io_ticks_ms: u64,
    /// Previous tick's counters for delta computation.
    pub prev: Option<Prev>,
}

#[derive(Debug, Clone, Copy)]
pub struct Prev {
    io_ticks_ms: u64,
    read_sectors: u64,
    write_sectors: u64,
}

impl DiskStat {
    pub fn active_pct(&self, interval_s: f64) -> f32 {
        let Some(prev) = self.prev else { return 0.0 };
        let d = self.io_ticks_ms.saturating_sub(prev.io_ticks_ms);
        ((d as f64 / 1000.0 / interval_s.max(0.001)) * 100.0).clamp(0.0, 100.0) as f32
    }

    pub fn read_bps(&self, interval_s: f64) -> f64 {
        let Some(prev) = self.prev else { return 0.0 };
        (self.read_sectors.saturating_sub(prev.read_sectors)) as f64 * 512.0 / interval_s.max(0.001)
    }

    pub fn write_bps(&self, interval_s: f64) -> f64 {
        let Some(prev) = self.prev else { return 0.0 };
        (self.write_sectors.saturating_sub(prev.write_sectors)) as f64 * 512.0 / interval_s.max(0.001)
    }
}

/// Parse /proc/diskstats and attach previous-tick deltas.
pub fn read() -> Vec<DiskStat> {
    static PREV: Mutex<HashMap<String, Prev>> = Mutex::new(HashMap::new());
    let raw = parse_raw();
    let mut prev_map = PREV.lock();

    let out: Vec<DiskStat> = raw
        .into_iter()
        .map(|mut s| {
            s.prev = prev_map.get(&s.device).copied();
            s
        })
        .collect();

    // Store current counters for the next tick.
    *prev_map = out
        .iter()
        .map(|s| {
            (
                s.device.clone(),
                Prev {
                    io_ticks_ms: s.io_ticks_ms,
                    read_sectors: s.read_sectors,
                    write_sectors: s.write_sectors,
                },
            )
        })
        .collect();
    out
}

fn parse_raw() -> Vec<DiskStat> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/proc/diskstats") {
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 14 {
                continue;
            }
            let device = fields[2].to_string();
            if device.starts_with("loop") || device.starts_with("ram") {
                continue;
            }
            let num = |i: usize| fields.get(i).and_then(|f| f.parse::<u64>().ok()).unwrap_or(0);
            out.push(DiskStat {
                device,
                reads_completed: num(3),
                writes_completed: num(7),
                read_sectors: num(5),
                write_sectors: num(9),
                io_ticks_ms: num(12),
                prev: None,
            });
        }
    }
    out
}
