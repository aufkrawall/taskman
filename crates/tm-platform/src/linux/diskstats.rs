//! /proc/diskstats parsing with delta tracking.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct DiskStat {
    pub device: String,
    pub reads_completed: u64,
    pub writes_completed: u64,
    /// Milliseconds spent on completed reads / writes since boot.
    pub read_ms: u64,
    pub write_ms: u64,
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
    ios: u64,
    io_ms: u64,
}

/// Rates need two samples: on first sight of a device there is no previous
/// counter, so every rate is `None` (unmeasured), never an idle 0.
impl DiskStat {
    pub fn active_pct(&self, interval_s: f64) -> Option<f32> {
        let prev = self.prev?;
        let d = self.io_ticks_ms.saturating_sub(prev.io_ticks_ms);
        Some(((d as f64 / 1000.0 / interval_s.max(0.001)) * 100.0).clamp(0.0, 100.0) as f32)
    }

    pub fn read_bps(&self, interval_s: f64) -> Option<f64> {
        let prev = self.prev?;
        Some(
            (self.read_sectors.saturating_sub(prev.read_sectors)) as f64 * 512.0
                / interval_s.max(0.001),
        )
    }

    /// Average milliseconds per completed I/O in the window (iostat's
    /// `await`); `None` on a first sample or a window without a completed I/O.
    pub fn avg_resp_ms(&self) -> Option<f64> {
        let prev = self.prev?;
        crate::linux_disks::average_response_ms(
            self.io_ms().saturating_sub(prev.io_ms),
            self.ios().saturating_sub(prev.ios),
        )
    }

    fn ios(&self) -> u64 {
        self.reads_completed.saturating_add(self.writes_completed)
    }

    fn io_ms(&self) -> u64 {
        self.read_ms.saturating_add(self.write_ms)
    }

    pub fn write_bps(&self, interval_s: f64) -> Option<f64> {
        let prev = self.prev?;
        Some(
            (self.write_sectors.saturating_sub(prev.write_sectors)) as f64 * 512.0
                / interval_s.max(0.001),
        )
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
            // A device that reappears under the same name (a re-created `dm-N`,
            // a controller reset) restarts its counters. Differencing across
            // that restart would report one interval of measured zero activity,
            // so the window is unmeasured instead — the same rule `cpu_stat`
            // applies to a regressed counter.
            s.prev = carried_prev(&s, prev_map.get(&s.device).copied());
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
                    ios: s.ios(),
                    io_ms: s.io_ms(),
                },
            )
        })
        .collect();
    out
}

/// The previous counters one reading may be differenced against.
///
/// All three counters must have moved forward. A device that reappeared under
/// the same name (a re-created `dm-N`, a controller reset) restarts them, and
/// differencing across that restart would report one interval of measured zero
/// activity instead of an unmeasured one.
fn carried_prev(current: &DiskStat, previous: Option<Prev>) -> Option<Prev> {
    previous.filter(|prev| {
        current.io_ticks_ms >= prev.io_ticks_ms
            && current.read_sectors >= prev.read_sectors
            && current.write_sectors >= prev.write_sectors
            && current.ios() >= prev.ios
            && current.io_ms() >= prev.io_ms
    })
}

fn parse_raw() -> Vec<DiskStat> {
    std::fs::read_to_string("/proc/diskstats")
        .map(|text| parse_diskstats(&text))
        .unwrap_or_default()
}

fn parse_diskstats(text: &str) -> Vec<DiskStat> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 14 {
                return None;
            }
            let device = fields[2];
            if device.starts_with("loop") || device.starts_with("ram") {
                return None;
            }
            // Every counter must parse. A field this build cannot read is not a
            // zero: skipping the row keeps its rate unavailable instead of
            // reporting one measured 0 and then storing that 0 as the next
            // window's baseline, which turns the next delta into the device's
            // whole lifetime of traffic.
            let num = |index: usize| fields.get(index)?.parse::<u64>().ok();
            Some(DiskStat {
                device: device.to_string(),
                reads_completed: num(3)?,
                writes_completed: num(7)?,
                read_ms: num(6)?,
                write_ms: num(10)?,
                read_sectors: num(5)?,
                write_sectors: num(9)?,
                io_ticks_ms: num(12)?,
                prev: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_unknown_on_first_sight() {
        let mut s = DiskStat {
            device: "nvme0n1".into(),
            reads_completed: 10,
            writes_completed: 20,
            read_ms: 30,
            write_ms: 50,
            read_sectors: 100,
            write_sectors: 200,
            io_ticks_ms: 50,
            prev: None,
        };
        assert_eq!(s.active_pct(1.0), None);
        assert_eq!(s.read_bps(1.0), None);
        assert_eq!(s.write_bps(1.0), None);

        // Simulate second tick.
        s.prev = Some(Prev {
            io_ticks_ms: 40,
            read_sectors: 60,
            write_sectors: 120,
            ios: 26,
            io_ms: 64,
        });
        assert_eq!(s.active_pct(1.0), Some(1.0)); // 10ms of IO in 1s
        assert_eq!(s.read_bps(1.0), Some((100 - 60) as f64 * 512.0));
        assert_eq!(s.write_bps(1.0), Some((200 - 120) as f64 * 512.0));
        // 16 ms over 4 completed I/Os.
        assert_eq!(s.avg_resp_ms(), Some(4.0));
    }

    /// A row with a counter this build cannot read is not a zero counter: it is
    /// dropped, so its rate stays unavailable instead of becoming a measurement
    /// (and then the next window's baseline).
    #[test]
    fn an_unreadable_counter_drops_the_row_instead_of_reading_zero() {
        let text = "\
   8       0 sda 1234 0 5678 90 1112 0 3344 55 0 66 77 0 0 0 0 0 0
   8      16 sdb 1234 0 5678 90 1112 0 nope 55 0 66 77 0 0 0 0 0 0
   8      32 loop0 1 0 1 0 1 0 1 0 0 0 0 0 0 0 0 0 0
   8      48 ram0 1 0 1 0 1 0 1 0 0 0 0 0 0 0 0 0 0
";
        let parsed = parse_diskstats(text);
        assert_eq!(
            parsed.len(),
            1,
            "only the complete row survives: {parsed:?}"
        );
        assert_eq!(parsed[0].device, "sda");
        assert_eq!(parsed[0].read_sectors, 5678);
        assert_eq!(parsed[0].write_sectors, 3344);
        assert_eq!(parsed[0].io_ticks_ms, 66);
    }

    /// A re-created device keeps its name but restarts its counters. Carrying
    /// the old baseline forward would publish one interval of measured zero
    /// activity instead of an unmeasured one.
    #[test]
    fn a_regressed_counter_leaves_the_window_unmeasured() {
        let current = DiskStat {
            device: "dm-0".into(),
            reads_completed: 0,
            writes_completed: 0,
            read_ms: 0,
            write_ms: 0,
            read_sectors: 5,
            write_sectors: 0,
            io_ticks_ms: 3,
            prev: None,
        };
        let restarted = Some(Prev {
            io_ticks_ms: 900,
            read_sectors: 4_000,
            write_sectors: 2_000,
            ios: 0,
            io_ms: 0,
        });
        assert!(carried_prev(&current, restarted).is_none());

        let mut unmeasured = current.clone();
        unmeasured.prev = carried_prev(&unmeasured, restarted);
        assert_eq!(unmeasured.active_pct(1.0), None);
        assert_eq!(unmeasured.read_bps(1.0), None);
        assert_eq!(unmeasured.write_bps(1.0), None);

        // A monotonic pair is carried, so its delta is a measurement.
        let monotonic = Some(Prev {
            io_ticks_ms: 1,
            read_sectors: 1,
            write_sectors: 0,
            ios: 0,
            io_ms: 0,
        });
        assert!(carried_prev(&current, monotonic).is_some());
    }
}
