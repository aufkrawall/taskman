//! Per-process CPU time accounting over our own `/proc` sampling window.
//!
//! Sources, all cumulative counters since boot:
//!
//! * `/proc/<pid>/stat` fields 14+15 (`utime`+`stime`) in jiffies. For a
//!   thread-group leader the kernel reports the WHOLE group there (verified
//!   against `/proc/<pid>/task/*/stat`: the leader's figure equals the sum over
//!   its tasks and keeps the time of threads that already exited), so reading
//!   only group leaders counts every thread exactly once.
//! * `/proc/stat`'s aggregate `cpu` line for the machine-wide capacity the
//!   window covered, measured by `super::cpu_stat::CpuStatSampler` over the
//!   same two ticks.
//!
//! Semantics, matching the Windows accountant (`win/cpu_load.rs`):
//!
//! * `pct` in [0, 100] = the process' CPU-time delta divided by the whole
//!   machine's time capacity in the window ("share of total capacity",
//!   Task Manager Processes/Details style). Numerator and denominator come
//!   from the same window, so it stays correct across CPU hotplug without
//!   dividing by a CPU count, and it needs no clock frequency.
//! * `cpu_time_s` = cumulative user+system time since process start. A
//!   cumulative counter, so it is available even where the window is not.
//!
//! Unavailable instead of fabricated: every value is `None` rather than 0 %
//! when it is not a measurement — no measurable `/proc/stat` window (first
//! tick, counter reset, CPU set change), no earlier reading of the same
//! process generation, a reading that was skipped for a tick, or counters that
//! regressed. The honest gaps against Windows: a process first seen in this
//! window reports nothing (Windows credits newborn processes their in-window
//! time), and a process that exited inside the window takes its last ticks
//! with it, so the rows can sum to slightly less than the machine total.

use std::collections::{HashMap, HashSet};

/// The `/proc/<pid>/stat` fields this accountant needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidCpuStat {
    /// Fields 14+15: cumulative user+system CPU jiffies of the thread group.
    pub cpu_ticks: u64,
    /// Field 22: boot-relative creation tick. Guards against PID reuse turning
    /// another process' counters into this process' delta.
    pub start_ticks: i64,
}

/// One process' CPU facts for the current window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcCpu {
    /// Share of total machine CPU capacity in the measured window, in
    /// [0, 100]. `None` when this window cannot attribute time to the process.
    pub pct: Option<f32>,
    /// Cumulative user+system CPU time since process start, in seconds.
    /// `None` when `CLK_TCK` is unknown (jiffies then have no known unit).
    pub cpu_time_s: Option<f64>,
}

/// The previous reading of one process generation.
#[derive(Debug, Clone, Copy)]
struct ProcSample {
    /// Window the reading belongs to, so a skipped tick is detectable.
    tick: u64,
    start_ticks: i64,
    cpu_ticks: u64,
}

/// Keeps the previous `/proc/<pid>/stat` counters and turns them into one
/// measured per-process window.
#[derive(Debug, Default)]
pub struct ProcessCpuSampler {
    /// `_SC_CLK_TCK`, the jiffy rate of the `/proc` counters.
    clk_tck: Option<u64>,
    /// Jiffies the current window covered across all online CPUs, from the
    /// `/proc/stat` aggregate delta. `None` when that window is unmeasurable.
    window_total_ticks: Option<u64>,
    /// Monotonic window counter for [`ProcSample::tick`].
    tick: u64,
    previous: HashMap<u32, ProcSample>,
}

impl ProcessCpuSampler {
    pub fn new(clk_tck: Option<u64>) -> Self {
        Self {
            clk_tck: clk_tck.filter(|&hz| hz > 0),
            ..Self::default()
        }
    }

    /// Starts a tick. `window_total_ticks` is the machine-wide CPU jiffies the
    /// window covered, `None` when the global sampler could not measure it.
    /// A window of zero jiffies is no measurement either, so it is rejected
    /// here instead of becoming a divisor below.
    pub fn start(&mut self, window_total_ticks: Option<u64>) {
        self.window_total_ticks = window_total_ticks.filter(|&total| total > 0);
        self.tick = self.tick.wrapping_add(1);
    }

    /// Records one process' cumulative counters and derives its load.
    pub fn observe(&mut self, pid: u32, stat: PidCpuStat) -> ProcCpu {
        let tick = self.tick;
        // Insert first: this reading is the next window's baseline no matter
        // what the delta below can say about it.
        let old = self.previous.insert(
            pid,
            ProcSample {
                tick,
                start_ticks: stat.start_ticks,
                cpu_ticks: stat.cpu_ticks,
            },
        );
        ProcCpu {
            pct: self.share(old, &stat),
            cpu_time_s: self.clk_tck.map(|hz| stat.cpu_ticks as f64 / hz as f64),
        }
    }

    fn share(&self, old: Option<ProcSample>, stat: &PidCpuStat) -> Option<f32> {
        let total = self.window_total_ticks?;
        let old = old?;
        // A reading from an older window would spread this window's CPU over
        // the previous window's capacity and inflate the share.
        if self.tick.wrapping_sub(old.tick) != 1 || old.start_ticks != stat.start_ticks {
            return None;
        }
        let delta = stat.cpu_ticks.checked_sub(old.cpu_ticks)?;
        Some((delta as f64 * 100.0 / total as f64).clamp(0.0, 100.0) as f32)
    }

    /// Forgets every process that is no longer alive, so a long session does
    /// not accumulate one entry per PID ever seen.
    pub fn retain_live(&mut self, live: &HashSet<u32>) {
        self.previous.retain(|pid, _| live.contains(pid));
    }
}

/// Parses the fields this module needs from `/proc/<pid>/stat`.
///
/// Field two is the command name and may contain spaces, `)` bytes and
/// non-UTF-8 data. Fixed fields therefore begin only after the final `)`.
pub fn parse_pid_cpu_stat(input: &[u8]) -> Option<PidCpuStat> {
    let close = input.iter().rposition(|&byte| byte == b')')?;
    let fields = input[close + 1..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let parse_u64 = |index: usize| {
        std::str::from_utf8(fields.get(index).copied()?)
            .ok()?
            .parse::<u64>()
            .ok()
    };
    // Field 14 (utime), field 15 (stime) and field 22 (starttime); index zero
    // is field 3.
    let utime = parse_u64(11)?;
    let stime = parse_u64(12)?;
    Some(PidCpuStat {
        cpu_ticks: utime.checked_add(stime)?,
        start_ticks: i64::try_from(parse_u64(19)?).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/proc/<pid>/stat` line with a hostile `comm` and the two counters
    /// this module reads. Fields after `comm` start at field 3 (`state`).
    fn stat_line(utime: u64, stime: u64, start_ticks: u64) -> Vec<u8> {
        let mut line =
            b"4242 (worker (with) \xff parens) S 41 4242 4242 0 -1 4194304 10 0 2 0 ".to_vec();
        line.extend_from_slice(
            format!("{utime} {stime} 0 0 20 -5 7 0 {start_ticks} 1000 50\n").as_bytes(),
        );
        line
    }

    fn stat(cpu_ticks: u64, start_ticks: i64) -> PidCpuStat {
        PidCpuStat {
            cpu_ticks,
            start_ticks,
        }
    }

    #[test]
    fn pid_stat_survives_spaces_parens_and_non_utf8_in_comm() {
        let parsed = parse_pid_cpu_stat(&stat_line(123, 456, 98765)).expect("pid stat");
        assert_eq!(parsed.cpu_ticks, 579);
        assert_eq!(parsed.start_ticks, 98765);
    }

    #[test]
    fn a_truncated_pid_stat_is_unavailable() {
        assert_eq!(parse_pid_cpu_stat(b"4242 (short) S 1 2"), None);
        assert_eq!(parse_pid_cpu_stat(b""), None);
    }

    #[test]
    fn the_first_window_has_no_process_share_but_keeps_cpu_time() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(2400));
        let cpu = sampler.observe(4242, stat(150, 10));
        assert_eq!(cpu.pct, None, "one reading is not a window");
        assert_eq!(cpu.cpu_time_s, Some(1.5), "cumulative time needs no window");
    }

    #[test]
    fn a_share_is_the_process_delta_over_the_machine_window() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(2400));
        assert_eq!(sampler.observe(4242, stat(150, 10)).pct, None);
        sampler.start(Some(2400));
        // 300 more jiffies out of 2400 jiffies of capacity = 12.5 %.
        assert_eq!(sampler.observe(4242, stat(450, 10)).pct, Some(12.5));
    }

    #[test]
    fn one_busy_core_is_one_core_of_the_machine_capacity() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(1200));
        sampler.observe(7, stat(0, 10));
        sampler.start(Some(1200));
        // 100 jiffies of CPU in a 1200-jiffy window = one core of twelve.
        let pct = sampler.observe(7, stat(100, 10)).pct.expect("measured");
        assert!((pct - 8.333_333).abs() < 0.001, "got {pct}");
    }

    #[test]
    fn a_new_process_is_unavailable_until_it_has_two_readings() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(2400));
        sampler.observe(1, stat(10, 1));
        sampler.start(Some(2400));
        // PID 2 appears with counters that already cover its whole life; there
        // is no earlier reading to difference against.
        assert_eq!(sampler.observe(2, stat(900, 5000)).pct, None);
        sampler.start(Some(2400));
        assert_eq!(sampler.observe(2, stat(960, 5000)).pct, Some(2.5));
    }

    #[test]
    fn a_reused_pid_starts_over_instead_of_inheriting_a_delta() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(2400));
        sampler.observe(4242, stat(5000, 10));
        sampler.start(Some(2400));
        assert_eq!(
            sampler.observe(4242, stat(20, 9000)).pct,
            None,
            "a different creation tick is a different process"
        );
    }

    #[test]
    fn regressing_counters_are_unavailable() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(2400));
        sampler.observe(4242, stat(500, 10));
        sampler.start(Some(2400));
        assert_eq!(sampler.observe(4242, stat(400, 10)).pct, None);
    }

    #[test]
    fn a_skipped_reading_does_not_span_two_windows() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(2400));
        sampler.observe(4242, stat(100, 10));
        sampler.start(Some(2400));
        // Nothing observed in this window: the next delta would cover two
        // windows while the denominator covers one.
        sampler.start(Some(2400));
        assert_eq!(sampler.observe(4242, stat(900, 10)).pct, None);
        sampler.start(Some(2400));
        assert_eq!(sampler.observe(4242, stat(1020, 10)).pct, Some(5.0));
    }

    #[test]
    fn an_unmeasurable_global_window_leaves_every_share_unavailable() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(2400));
        sampler.observe(4242, stat(100, 10));
        sampler.start(None);
        let cpu = sampler.observe(4242, stat(200, 10));
        assert_eq!(cpu.pct, None);
        assert_eq!(cpu.cpu_time_s, Some(2.0));
        sampler.start(Some(0));
        assert_eq!(sampler.observe(4242, stat(300, 10)).pct, None);
    }

    #[test]
    fn a_share_never_exceeds_the_whole_machine() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(100));
        sampler.observe(7, stat(0, 10));
        sampler.start(Some(100));
        // Counter skew must not render as more than the machine's capacity.
        assert_eq!(sampler.observe(7, stat(400, 10)).pct, Some(100.0));
    }

    #[test]
    fn an_unknown_clock_tick_leaves_cpu_time_unavailable() {
        let mut sampler = ProcessCpuSampler::new(None);
        sampler.start(Some(2400));
        assert_eq!(sampler.observe(7, stat(150, 10)).cpu_time_s, None);
    }

    #[test]
    fn dead_processes_are_forgotten() {
        let mut sampler = ProcessCpuSampler::new(Some(100));
        sampler.start(Some(2400));
        sampler.observe(7, stat(100, 10));
        sampler.start(Some(2400));
        sampler.observe(8, stat(100, 10));
        sampler.retain_live(&HashSet::from([8]));
        assert!(!sampler.previous.contains_key(&7));
        assert!(sampler.previous.contains_key(&8));
        // The pruned process comes back as a first sighting, not as a delta.
        sampler.start(Some(2400));
        assert_eq!(sampler.observe(7, stat(900, 10)).pct, None);
    }
}
