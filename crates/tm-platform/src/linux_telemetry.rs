//! Host facts the Linux backend derives from `/proc` and `/sys`, and the
//! telemetry-demand selection it samples under.
//!
//! Parsing and derivation live outside the `#[cfg(target_os = "linux")]`
//! backend on purpose: only the Linux collector calls them, but compiling them
//! everywhere lets their unit tests run on the Windows dev box and CI. Every
//! function takes the text or the tree root it inspects, so a test can hand it
//! a fake sysfs layout instead of the machine's own.

use std::collections::HashSet;
use std::path::Path;
use tm_core::demand::TelemetryDemand;

/// Selected `/proc/meminfo` counters, in bytes.
///
/// A field the kernel does not publish stays `None`: the backend decides how an
/// unmeasured counter renders, and the parser never invents a zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Meminfo {
    pub cached_bytes: Option<u64>,
    pub sreclaimable_bytes: Option<u64>,
    pub commit_limit_bytes: Option<u64>,
    pub committed_as_bytes: Option<u64>,
}

/// Every counter the backend reports from one `/proc/meminfo` read.
pub fn parse_meminfo(text: &str) -> Meminfo {
    let mut out = Meminfo::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let slot = match key.trim() {
            "Cached" => &mut out.cached_bytes,
            "SReclaimable" => &mut out.sreclaimable_bytes,
            "CommitLimit" => &mut out.commit_limit_bytes,
            "Committed_AS" => &mut out.committed_as_bytes,
            _ => continue,
        };
        *slot = parse_kib(value);
    }
    out
}

/// `/proc/meminfo` values are KiB despite the "kB" suffix.
fn parse_kib(value: &str) -> Option<u64> {
    let digits: String = value
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u64>().ok().map(|kb| kb.saturating_mul(1024))
}

/// Nice value (stat field 19) of a `/proc/<pid>/stat` record.
///
/// Field 2 (comm) may contain spaces, parentheses and non-UTF-8 bytes, so the
/// positional fields are counted from the record's final `')'`. `None` when the
/// record is truncated or the field is not a number.
pub fn parse_proc_stat_nice(stat: &[u8]) -> Option<i32> {
    let after_comm = stat.iter().rposition(|&byte| byte == b')')? + 1;
    let nice = stat[after_comm..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .nth(16)?;
    std::str::from_utf8(nice).ok()?.parse().ok()
}

/// Distinct physical-core and package counts from a CPU topology tree
/// (`/sys/devices/system/cpu`).
///
/// sysfs repeats both ids under every logical CPU, so a core is counted once
/// per `(physical_package_id, core_id)` pair and SMT siblings collapse onto it.
/// A CPU whose ids cannot be read contributes nothing, and an unreadable tree
/// reports `(0, 0)` — unknown, never a guessed topology.
pub fn cpu_topology(cpu_root: &Path) -> (usize, usize) {
    let mut cores: HashSet<(u32, u32)> = HashSet::new();
    let mut packages: HashSet<u32> = HashSet::new();
    let Ok(entries) = std::fs::read_dir(cpu_root) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = name.strip_prefix("cpu") else {
            continue;
        };
        if index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let topology = entry.path().join("topology");
        let package = read_u32(topology.join("physical_package_id"));
        let core = read_u32(topology.join("core_id"));
        if let (Some(package), Some(core)) = (package, core) {
            packages.insert(package);
            cores.insert((package, core));
        }
    }
    (cores.len(), packages.len())
}

/// Highest base clock (MHz) any cpufreq policy reports, 0 when the kernel
/// exposes none. A base clock the kernel does not publish is unknown, not an
/// estimate: the caller renders 0 as unavailable.
pub fn base_frequency_mhz(cpufreq_root: &Path) -> f32 {
    let Ok(entries) = std::fs::read_dir(cpufreq_root) else {
        return 0.0;
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("policy"))
        .filter_map(|entry| {
            std::fs::read_to_string(entry.path().join("base_frequency"))
                .ok()?
                .trim()
                .parse::<f32>()
                .ok()
        })
        .map(|khz| khz / 1000.0)
        .fold(0.0f32, f32::max)
}

fn read_u32(path: impl AsRef<Path>) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Which optional providers the current [`TelemetryDemand`] keeps switched on.
///
/// This mirrors the Windows sampler bit for bit, for the providers Linux
/// actually has: the process table and adapter metadata are always sampled
/// (Windows samples them at every demand too), GPU utilization and VRAM are
/// only *measured* while a GPU bit is set, `/proc/diskstats` is only read while
/// `DISK_RATE` is set, and the per-CPU current frequency — a `scaling_cur_freq`
/// read per CPU, falling back to the whole `/proc/cpuinfo` per CPU — is only
/// probed while `CPU_SPEED` is set. Bits whose provider Linux does not have
/// (per-process network/disk tracing, WLAN details, token security) select
/// nothing, and the columns they would fill stay unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Providers {
    /// Measure GPU utilization, VRAM use and temperature.
    pub measure_gpus: bool,
    /// Read `/proc/diskstats` and publish disk rates and active time.
    pub disk_rates: bool,
    /// Refresh the per-CPU current frequency.
    pub cpu_speed: bool,
}

pub fn providers(demand: TelemetryDemand) -> Providers {
    Providers {
        measure_gpus: demand.any_gpu(),
        disk_rates: demand.wants(TelemetryDemand::DISK_RATE),
        cpu_speed: demand.wants(TelemetryDemand::CPU_SPEED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const MEMINFO: &str = "\
MemTotal:       32768 kB
MemFree:         1024 kB
Cached:          4096 kB
SReclaimable:     512 kB
CommitLimit:    16384 kB
Committed_AS:   12345 kB
";

    #[test]
    fn meminfo_reports_every_field_from_one_parse() {
        let meminfo = parse_meminfo(MEMINFO);
        assert_eq!(meminfo.cached_bytes, Some(4096 * 1024));
        assert_eq!(meminfo.sreclaimable_bytes, Some(512 * 1024));
        assert_eq!(meminfo.commit_limit_bytes, Some(16384 * 1024));
        assert_eq!(meminfo.committed_as_bytes, Some(12345 * 1024));
    }

    #[test]
    fn a_meminfo_field_the_kernel_omits_stays_unmeasured() {
        let meminfo = parse_meminfo("MemTotal: 32768 kB\nCached: 2048 kB\n");
        assert_eq!(meminfo.cached_bytes, Some(2048 * 1024));
        assert_eq!(meminfo.sreclaimable_bytes, None);
        assert_eq!(meminfo.commit_limit_bytes, None);
        assert_eq!(meminfo.committed_as_bytes, None);
        assert_eq!(parse_meminfo("Cached: unreadable kB\n").cached_bytes, None);
        assert_eq!(parse_meminfo(""), Meminfo::default());
    }

    /// `42 (Web Content (x))` with nice -5 (stat field 19): comm with spaces
    /// and both parenthesis kinds must not shift the positional fields.
    const PID_STAT: &[u8] =
        b"42 (Web Content (\xff)) S 1 42 42 0 -1 4194304 10 0 2 0 1 0 0 0 20 -5 4 0 12345 1000 50";

    #[test]
    fn nice_is_read_from_the_sample_loop_stat_record() {
        assert_eq!(parse_proc_stat_nice(PID_STAT), Some(-5));
        // A record that stops before field 19, and one whose field 19 is not a
        // number, are unknown — never a defaulted nice.
        assert_eq!(
            parse_proc_stat_nice(b"7 (a) S 1 7 7 0 -1 4194304 0 0 0 0 0 0 0 0 20"),
            None
        );
        assert_eq!(
            parse_proc_stat_nice(b"7 (a) S 1 7 7 0 -1 4194304 0 0 0 0 0 0 0 0 20 x"),
            None
        );
        assert_eq!(parse_proc_stat_nice(b""), None);
    }

    struct Sysfs {
        root: PathBuf,
    }

    impl Sysfs {
        fn new(name: &str) -> Self {
            let dir = format!("tm-linux-{name}-{}", std::process::id());
            let root = std::env::temp_dir().join(dir);
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path
        }

        fn mkdir(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Sysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn topology_counts_smt_siblings_as_one_core() {
        let sysfs = Sysfs::new("topology");
        // Two packages; cpu0/cpu1 are SMT siblings of one core, cpu2 is a
        // second core of package 0, cpu3 sits on package 1.
        for (cpu, package, core) in [(0, 0, 0), (1, 0, 0), (2, 0, 1), (3, 1, 0)] {
            sysfs.write(
                &format!("cpu{cpu}/topology/physical_package_id"),
                &format!("{package}\n"),
            );
            sysfs.write(&format!("cpu{cpu}/topology/core_id"), &format!("{core}\n"));
        }
        // Non-CPU entries of the same directory and a CPU without topology.
        sysfs.mkdir("cpufreq");
        sysfs.mkdir("cpuidle");
        sysfs.write("cpu", "0-15\n");
        sysfs.write("cpu4/topology/physical_package_id", "0\n");
        assert_eq!(cpu_topology(&sysfs.root), (3, 2));
    }

    #[test]
    fn unreadable_topology_is_unknown_instead_of_zero_cores_of_one_package() {
        let sysfs = Sysfs::new("topology-missing");
        assert_eq!(cpu_topology(&sysfs.root.join("absent")), (0, 0));
        assert_eq!(cpu_topology(&sysfs.root), (0, 0));
    }

    #[test]
    fn base_frequency_is_the_highest_reported_policy() {
        let sysfs = Sysfs::new("cpufreq");
        sysfs.write("policy0/base_frequency", "2200000\n");
        sysfs.write("policy8/base_frequency", "3600000\n");
        sysfs.mkdir("policy4");
        sysfs.write("boost", "1\n");
        assert_eq!(base_frequency_mhz(&sysfs.root), 3600.0);
        assert_eq!(base_frequency_mhz(&sysfs.root.join("absent")), 0.0);
        let empty = Sysfs::new("cpufreq-empty");
        assert_eq!(base_frequency_mhz(&empty.root), 0.0);
    }

    #[test]
    fn demand_selects_exactly_the_providers_windows_skips() {
        let hidden = providers(TelemetryDemand::hidden());
        assert_eq!(hidden, Providers::default());
        // Core demand keeps the process table and adapter metadata warm but
        // does not measure GPUs, disk rates or the CPU clock.
        assert_eq!(providers(TelemetryDemand::core()), Providers::default());
        assert_eq!(
            providers(TelemetryDemand::all()),
            Providers {
                measure_gpus: true,
                disk_rates: true,
                cpu_speed: true,
            }
        );

        for gpu in [
            TelemetryDemand::GPU_ADAPTER,
            TelemetryDemand::PROCESS_GPU,
            TelemetryDemand::PROCESS_GPU_MEMORY,
        ] {
            assert_eq!(
                providers(gpu),
                Providers {
                    measure_gpus: true,
                    ..Providers::default()
                },
                "every GPU bit measures the adapters"
            );
        }
        assert_eq!(
            providers(TelemetryDemand::DISK_RATE),
            Providers {
                disk_rates: true,
                ..Providers::default()
            }
        );
        assert_eq!(
            providers(TelemetryDemand::CPU_SPEED),
            Providers {
                cpu_speed: true,
                ..Providers::default()
            }
        );
        // Providers Linux does not have, and the always-on baseline: nothing
        // extra is selected, and no table is emptied by selecting it.
        for bit in [
            TelemetryDemand::CORE_PROCESS,
            TelemetryDemand::NET_ADAPTER_RATE,
            TelemetryDemand::TOKEN_SECURITY,
            TelemetryDemand::PROCESS_NET,
            TelemetryDemand::PROCESS_DISK,
            TelemetryDemand::WIFI_DETAILS,
        ] {
            assert_eq!(providers(bit), Providers::default());
        }
    }
}
