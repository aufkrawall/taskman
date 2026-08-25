//! /proc/diskstats parsing with delta tracking.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct DiskStat {
    pub device: String,
    #[allow(dead_code)] // informational; kept for future columns
    pub reads_completed: u64,
    #[allow(dead_code)]
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
        (self.write_sectors.saturating_sub(prev.write_sectors)) as f64 * 512.0
            / interval_s.max(0.001)
    }
}

fn prev_store() -> &'static Mutex<HashMap<String, Prev>> {
    static STORE: std::sync::OnceLock<Mutex<HashMap<String, Prev>>> = std::sync::OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Parse /proc/diskstats and attach previous-tick deltas.
pub fn read() -> Vec<DiskStat> {
    let raw = parse_raw();
    let mut prev_map = tm_core::sync::lock(prev_store());

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
            let num = |i: usize| {
                fields
                    .get(i)
                    .and_then(|f| f.parse::<u64>().ok())
                    .unwrap_or(0)
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_zero_on_first_sight() {
        let mut s = DiskStat {
            device: "nvme0n1".into(),
            reads_completed: 10,
            writes_completed: 20,
            read_sectors: 100,
            write_sectors: 200,
            io_ticks_ms: 50,
            prev: None,
        };
        assert_eq!(s.active_pct(1.0), 0.0);
        assert_eq!(s.read_bps(1.0), 0.0);

        // Simulate second tick.
        s.prev = Some(Prev {
            io_ticks_ms: 40,
            read_sectors: 60,
            write_sectors: 120,
        });
        assert_eq!(s.active_pct(1.0), 1.0); // 10ms of IO in 1s
        assert_eq!(s.read_bps(1.0), (100 - 60) as f64 * 512.0);
        assert_eq!(s.write_bps(1.0), (200 - 120) as f64 * 512.0);
    }
}
