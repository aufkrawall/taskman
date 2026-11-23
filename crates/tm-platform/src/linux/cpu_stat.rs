//! `/proc/stat` CPU counters with delta-based utilization tracking.
//!
//! `/proc/stat` publishes cumulative jiffies since boot. A single reading is
//! not a utilization measurement, so the first sample (and the first sample
//! after a CPU set change) is reported as unavailable rather than as 0 %.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CpuTimes {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl CpuTimes {
    fn total(self) -> u64 {
        self.user
            .saturating_add(self.nice)
            .saturating_add(self.system)
            .saturating_add(self.idle)
            .saturating_add(self.iowait)
            .saturating_add(self.irq)
            .saturating_add(self.softirq)
            .saturating_add(self.steal)
    }

    fn idle_total(self) -> u64 {
        self.idle.saturating_add(self.iowait)
    }

    fn kernel_total(self) -> u64 {
        self.system
            .saturating_add(self.irq)
            .saturating_add(self.softirq)
    }

    fn counter_regressed(self, previous: Self) -> bool {
        self.user < previous.user
            || self.nice < previous.nice
            || self.system < previous.system
            || self.idle < previous.idle
            || self.iowait < previous.iowait
            || self.irq < previous.irq
            || self.softirq < previous.softirq
            || self.steal < previous.steal
    }
}

#[derive(Debug, Clone, Default)]
struct ProcStat {
    aggregate: Option<CpuTimes>,
    cores: BTreeMap<u32, CpuTimes>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CpuSample {
    pub utilization_pct: f32,
    pub per_core_pct: Vec<f32>,
    pub per_core_kernel_pct: Vec<f32>,
    pub kernel_pct: f32,
}

/// Keeps the previous `/proc/stat` counters and turns them into one measured
/// CPU window.
#[derive(Debug, Default)]
pub struct CpuStatSampler {
    previous: Option<ProcStat>,
}

impl CpuStatSampler {
    pub fn sample(&mut self, proc_stat: &str) -> Option<CpuSample> {
        let current = parse_proc_stat(proc_stat)?;
        let previous = self.previous.replace(current.clone());

        // A topology change and a counter reset both make this window
        // unmeasurable. Return no data; the next tick will have a valid
        // baseline.
        if previous
            .as_ref()
            .is_some_and(|old| old.cores.keys().copied().ne(current.cores.keys().copied()))
        {
            return None;
        }
        let previous = previous?;

        let aggregate = percentages(previous.aggregate?, current.aggregate?)?;
        let mut per_core_pct = Vec::with_capacity(current.cores.len());
        let mut per_core_kernel_pct = Vec::with_capacity(current.cores.len());
        for (id, times) in &current.cores {
            let old = previous.cores.get(id)?;
            let (total, kernel) = percentages(*old, *times)?;
            per_core_pct.push(total);
            per_core_kernel_pct.push(kernel);
        }

        Some(CpuSample {
            utilization_pct: aggregate.0,
            per_core_pct,
            per_core_kernel_pct,
            kernel_pct: aggregate.1,
        })
    }
}

/// Returns `(busy percent, kernel percent)` for one counter delta. Both are
/// shares of all elapsed CPU jiffies in the window, so the kernel value never
/// exceeds the utilization value.
fn percentages(previous: CpuTimes, current: CpuTimes) -> Option<(f32, f32)> {
    if current.counter_regressed(previous) {
        return None;
    }
    let total = current.total().saturating_sub(previous.total());
    if total == 0 {
        return None;
    }
    let idle = current.idle_total().saturating_sub(previous.idle_total());
    let kernel = current
        .kernel_total()
        .saturating_sub(previous.kernel_total());
    let busy = total.saturating_sub(idle);
    let utilization = (busy as f64 * 100.0 / total as f64).clamp(0.0, 100.0) as f32;
    let kernel = (kernel as f64 * 100.0 / total as f64).clamp(0.0, f64::from(utilization)) as f32;
    Some((utilization, kernel))
}

fn parse_proc_stat(text: &str) -> Option<ProcStat> {
    let mut out = ProcStat::default();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(label) = fields.next() else {
            continue;
        };
        let cpu = if label == "cpu" {
            None
        } else if let Some(id) = label.strip_prefix("cpu") {
            let Ok(id) = id.parse::<u32>() else {
                continue;
            };
            Some(id)
        } else {
            continue;
        };
        let values: Vec<u64> = fields
            .take(8)
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        // Kernels before Linux 2.6.11 exposed only user/nice/system/idle.
        // Missing newer counters are unknown zeroes for those fields.
        if values.len() < 4 {
            continue;
        }
        let value = |index: usize| values.get(index).copied().unwrap_or(0);
        let times = CpuTimes {
            user: value(0),
            nice: value(1),
            system: value(2),
            idle: value(3),
            iowait: value(4),
            irq: value(5),
            softirq: value(6),
            steal: value(7),
        };
        match cpu {
            Some(id) => {
                out.cores.insert(id, times);
            }
            None => out.aggregate = Some(times),
        }
    }
    out.aggregate?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "\
cpu 100 10 50 840 0 0 0 0 0 0
cpu0 60 0 10 930 0 0 0 0 0 0
cpu1 40 10 40 910 0 0 0 0 0 0
";

    const SECOND: &str = "\
cpu 120 10 70 1000 0 0 0 0 0 0
cpu0 80 0 15 1005 0 0 0 0 0 0
cpu1 40 10 55 995 0 0 0 0 0 0
";

    #[test]
    fn first_sample_is_unknown_instead_of_idle() {
        let mut sampler = CpuStatSampler::default();
        assert_eq!(sampler.sample(FIRST), None);
    }

    #[test]
    fn deltas_report_busy_and_kernel_shares_per_core() {
        let mut sampler = CpuStatSampler::default();
        assert!(sampler.sample(FIRST).is_none());
        let sample = sampler.sample(SECOND).expect("second sample is measured");
        assert_eq!(sample.utilization_pct, 20.0);
        assert_eq!(sample.kernel_pct, 10.0);
        assert_eq!(sample.per_core_pct, vec![25.0, 15.0]);
        assert_eq!(sample.per_core_kernel_pct, vec![5.0, 15.0]);
    }

    #[test]
    fn a_counter_reset_is_unknown() {
        let mut sampler = CpuStatSampler::default();
        assert!(sampler.sample(SECOND).is_none());
        assert!(sampler.sample(FIRST).is_none());
    }

    #[test]
    fn a_cpu_set_change_is_unknown_until_the_next_window() {
        let mut sampler = CpuStatSampler::default();
        assert!(sampler.sample(FIRST).is_none());
        let changed = "\
cpu 130 10 80 980 0 0 0 0 0 0
cpu0 80 0 15 1005 0 0 0 0 0 0
cpu2 50 10 65 975 0 0 0 0 0 0
";
        assert!(sampler.sample(changed).is_none());
        let changed_next = "\
cpu 150 10 90 1050 0 0 0 0 0 0
cpu0 100 0 20 1080 0 0 0 0 0 0
cpu2 70 10 75 1045 0 0 0 0 0 0
";
        assert!(sampler.sample(changed_next).is_some());
    }

    #[test]
    fn guest_time_is_not_double_counted() {
        let first = "cpu 100 0 50 50 0 0 0 0 80 20\n";
        let second = "cpu 120 0 60 70 0 0 0 0 90 20\n";
        let mut sampler = CpuStatSampler::default();
        assert!(sampler.sample(first).is_none());
        let sample = sampler.sample(second).expect("measured");
        // The guest fields are already included in user/nice.
        assert_eq!(sample.utilization_pct, 60.0);
        assert_eq!(sample.kernel_pct, 20.0);
    }
}
