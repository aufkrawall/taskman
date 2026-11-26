//! Host facts the Linux backend derives from `/proc` and `/sys`, and the
//! telemetry-demand selection it samples under.
//!
//! Parsing and derivation live outside the `#[cfg(target_os = "linux")]`
//! backend on purpose: only the Linux collector calls them, but compiling them
//! everywhere lets their unit tests run on the Windows dev box and CI. Every
//! function takes the text or the tree root it inspects, so a test can hand it
//! a fake sysfs layout instead of the machine's own.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tm_core::demand::TelemetryDemand;

/// Number of logical processors `/proc/stat` lists.
///
/// The aggregate line is `cpu` followed by a space; every per-CPU line is `cpuN`. Counting
/// the suffixed lines gives the live count without a delta window — the per-core percentages
/// are unavailable on a first tick and whenever the core set changes, which is exactly when a
/// count is still wanted.
pub fn cpu_count(stat: &str) -> usize {
    stat.lines()
        .filter(|line| {
            line.strip_prefix("cpu")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count()
}

/// Whether a cached DRM card list still describes the cards sysfs lists now.
///
/// Entries are a card's sysfs path and the `device` symlink target read from it. Equal length
/// plus a matching pair for every card means no card was added, removed, renumbered, or
/// replaced by another PCI device under a reused number — the four ways a cached `cardN`
/// identity can silently go wrong.
pub fn card_identities_hold(
    cached: &[(PathBuf, Option<PathBuf>)],
    current: &[(PathBuf, Option<PathBuf>)],
) -> bool {
    cached.len() == current.len() && current.iter().all(|entry| cached.contains(entry))
}

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
    pub mem_free_bytes: Option<u64>,
    pub dirty_bytes: Option<u64>,
    pub writeback_bytes: Option<u64>,
    pub shmem_bytes: Option<u64>,
    pub buffers_bytes: Option<u64>,
}

/// Windows-style page lists derived from `/proc/meminfo`, for the memory
/// composition bar: `(modified, standby, free)` in bytes.
///
/// - Modified: dirty page cache and pages under writeback (`Dirty` +
///   `Writeback`) — data that must reach the disk before the page can be
///   reused, which is what Windows' modified list holds.
/// - Standby: the reclaimable cache (`Cached` + `Buffers` + `SReclaimable`)
///   minus the part
///   that cannot simply be dropped: shared memory/tmpfs (`Shmem`, counted in
///   `Cached` but only freeable by swapping) and the modified pages above.
/// - Free: `MemFree`.
///
/// `None` unless every counter it needs was published: a bar assembled from a
/// missing counter would claim an empty list.
pub fn page_lists(meminfo: &Meminfo) -> Option<(u64, u64, u64)> {
    let modified = meminfo
        .dirty_bytes?
        .saturating_add(meminfo.writeback_bytes?);
    let standby = meminfo
        .cached_bytes?
        .saturating_add(meminfo.buffers_bytes.unwrap_or(0))
        .saturating_add(meminfo.sreclaimable_bytes.unwrap_or(0))
        .saturating_sub(meminfo.shmem_bytes.unwrap_or(0))
        .saturating_sub(modified);
    Some((modified, standby, meminfo.mem_free_bytes?))
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
            "MemFree" => &mut out.mem_free_bytes,
            "Dirty" => &mut out.dirty_bytes,
            "Writeback" => &mut out.writeback_bytes,
            "Shmem" => &mut out.shmem_bytes,
            "Buffers" => &mut out.buffers_bytes,
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
    proc_stat_field(stat, 19)
}

/// Thread count (`num_threads`, field 20) from a `/proc/<pid>/stat` record,
/// the leader included -- the Windows count. Read from the record the sample
/// loop already holds, so no walk of `/proc/<pid>/task` is needed for it.
pub fn parse_proc_stat_num_threads(stat: &[u8]) -> Option<u32> {
    proc_stat_field(stat, 20)
}

/// Numeric field `field` (1-based, as in proc(5)) of a `/proc/<pid>/stat`
/// record; fields are counted from the record's final `')'`, past `comm`.
fn proc_stat_field<T: std::str::FromStr>(stat: &[u8], field: usize) -> Option<T> {
    let after_comm = stat.iter().rposition(|&byte| byte == b')')? + 1;
    let value = stat[after_comm..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .nth(field.checked_sub(3)?)?;
    std::str::from_utf8(value).ok()?.parse().ok()
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

/// Highest base clock (MHz) the kernel publishes for any CPU, 0 when it
/// publishes none. A base clock the kernel does not publish is unknown, not an
/// estimate: the caller renders 0 as unavailable.
///
/// The sources are tried in order of how directly they name the base clock:
///
/// 1. `cpufreq/policy*/base_frequency` (kHz) — intel_pstate.
/// 2. `cpufreq/policy*/amd_pstate_nominal_freq` (kHz) — amd-pstate on the
///    kernels that still publish it.
/// 3. `cpu*/acpi_cppc/nominal_freq` (MHz) — the firmware's CPPC nominal
///    performance level, which is the rated base clock. amd-pstate kernels
///    that dropped (2) and acpi-cpufreq on CPPC firmware publish only this.
///
/// `cpuinfo_max_freq` is deliberately NOT a source: with boost enabled it is
/// the boost ceiling, not the base clock.
pub fn base_frequency_mhz(cpu_root: &Path) -> f32 {
    let cpufreq = cpu_root.join("cpufreq");
    let from_policies = |file: &str| {
        max_over(&cpufreq, "policy", |dir| {
            read_f32(dir.join(file)).map(|khz| khz / 1000.0)
        })
    };
    let mhz = from_policies("base_frequency");
    if mhz > 0.0 {
        return mhz;
    }
    let mhz = from_policies("amd_pstate_nominal_freq");
    if mhz > 0.0 {
        return mhz;
    }
    max_over(cpu_root, "cpu", |dir| {
        read_f32(dir.join("acpi_cppc").join("nominal_freq"))
    })
}

/// The largest positive value `read` yields for the entries of `root` named
/// `<prefix><digits>`; 0 when none yields one.
fn max_over(root: &Path, prefix: &str, read: impl Fn(&Path) -> Option<f32>) -> f32 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0.0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix(prefix)
                .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|entry| read(&entry.path()))
        .filter(|mhz| mhz.is_finite() && *mhz > 0.0)
        .fold(0.0f32, f32::max)
}

fn read_f32(path: impl AsRef<Path>) -> Option<f32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_u32(path: impl AsRef<Path>) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether an ELF identification header names a 32-bit image on a 64-bit
/// host (`EI_CLASS` 1); `Some(false)` for a 64-bit image, `None` for a header
/// that is not ELF or a host that is not 64-bit.
pub fn elf_is_32bit(header: &[u8]) -> Option<bool> {
    if !cfg!(target_pointer_width = "64") || header.get(..4)? != b"\x7fELF" {
        return None;
    }
    match header.get(4)? {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// Hardware virtualization state from `/proc/cpuinfo`: `"Enabled"` when the
/// CPU advertises VT-x (`vmx`) or AMD-V (`svm`) — the kernel hides both flags
/// when the firmware disables the feature — and `"Unknown"` otherwise, since a
/// missing flag cannot tell "disabled in firmware" from "not supported".
pub fn virtualization_from_cpuinfo(cpuinfo: &str) -> &'static str {
    let enabled = cpuinfo
        .lines()
        .filter(|line| line.starts_with("flags"))
        .filter_map(|line| line.split_once(':'))
        .any(|(_, flags)| flags.split_whitespace().any(|f| f == "vmx" || f == "svm"));
    if enabled { "Enabled" } else { "Unknown" }
}

/// `TASK_COMM_LEN - 1`: the longest name `/proc/<pid>/stat` can carry.
const COMM_MAX: usize = 15;

/// The full process name a kernel `comm` was truncated from.
///
/// A `comm` shorter than [`COMM_MAX`] is complete. A full-length one is
/// replaced by the first candidate that starts with it: the executable's file
/// name, then the base names of the first two command-line words (an
/// interpreter's `comm` is its script's name, which is `argv[1]`). Anything
/// else keeps the kernel's name — a renamed thread or a process that set its
/// own `comm` is not guessed at.
pub fn untruncated_comm(comm: &str, exe: Option<&Path>, command_line: Option<&str>) -> String {
    if comm.len() < COMM_MAX {
        return comm.to_string();
    }
    let exe_name = exe
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned());
    let argv_names = command_line
        .into_iter()
        .flat_map(|line| line.split_whitespace().take(2))
        .filter_map(|word| Path::new(word).file_name())
        .map(|name| name.to_string_lossy().into_owned());
    exe_name
        .into_iter()
        .chain(argv_names)
        .find(|candidate| candidate.len() > comm.len() && candidate.starts_with(comm))
        .unwrap_or_else(|| comm.to_string())
}

/// Lifetime I/O counters of one process (`/proc/<pid>/io`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcIo {
    /// Bytes passed to read()/write()-family calls (files, pipes, sockets).
    pub rchar: u64,
    pub wchar: u64,
    /// Read / write system calls issued.
    pub syscr: u64,
    pub syscw: u64,
    /// Bytes this process caused to be fetched from / sent to the storage
    /// layer (block I/O), net of truncated dirty pages for writes.
    pub read_bytes: u64,
    pub write_bytes: u64,
}

/// Parse `/proc/<pid>/io`. `None` unless every counter used here parsed: a
/// partial record must not be differenced against a complete one.
pub fn parse_proc_io(text: &str) -> Option<ProcIo> {
    let mut fields: [Option<u64>; 7] = [None; 7];
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let index = match key.trim() {
            "rchar" => 0,
            "wchar" => 1,
            "syscr" => 2,
            "syscw" => 3,
            "read_bytes" => 4,
            "write_bytes" => 5,
            "cancelled_write_bytes" => 6,
            _ => continue,
        };
        fields[index] = value.trim().parse().ok();
    }
    let [
        rchar,
        wchar,
        syscr,
        syscw,
        read_bytes,
        write_bytes,
        cancelled,
    ] = fields;
    Some(ProcIo {
        rchar: rchar?,
        wchar: wchar?,
        syscr: syscr?,
        syscw: syscw?,
        read_bytes: read_bytes?,
        write_bytes: write_bytes?.saturating_sub(cancelled.unwrap_or(0)),
    })
}

/// Minor and major page-fault counts (stat fields 10 and 12) of a
/// `/proc/<pid>/stat` record; positional fields are counted from the final
/// `')'` because `comm` may contain anything.
pub fn parse_proc_stat_faults(stat: &[u8]) -> Option<(u64, u64)> {
    let after_comm = stat.iter().rposition(|&byte| byte == b')')? + 1;
    let mut fields = stat[after_comm..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    // Index 0 is field 3 (state): minflt is field 10, majflt field 12.
    let field =
        |raw: Option<&[u8]>| -> Option<u64> { std::str::from_utf8(raw?).ok()?.parse().ok() };
    let minflt = field(fields.nth(7))?;
    let majflt = field(fields.nth(1))?;
    Some((minflt, majflt))
}

/// Each process' share of the machine's disk time, estimated from block I/O.
///
/// Linux keeps no per-process disk service time (Windows measures it from
/// disk events). What it keeps per process is the bytes it caused at the
/// block layer, and per drive the bytes moved and the time spent busy. A
/// process' share of each busy drive is taken as its block bytes over the
/// window's block bytes — where the denominator is the larger of the bytes
/// all readable processes caused and the bytes the drives themselves moved,
/// so I/O of processes this token cannot read (root daemons, kernel
/// writeback) is never handed to the processes it can.
///
/// The model assumes a process' I/O spreads over the busy drives like the
/// machine's does (procfs does not say which drive a process used), so with
/// one busy drive the estimate is exact up to the byte/time relation, and
/// with several it attributes proportionally.
///
/// `process_bytes` is each process' block bytes in the window; `disks` is
/// `(disk number, active %, bytes moved)`. Returns, per input process, its
/// busiest drive share and the per-drive shares that are non-zero.
pub fn disk_time_shares(
    process_bytes: &[u64],
    disks: &[(u32, f32, u64)],
) -> Vec<(f32, Vec<(u32, f32)>)> {
    let readable_total: u64 = process_bytes.iter().copied().fold(0, u64::saturating_add);
    let disk_total: u64 = disks.iter().map(|d| d.2).fold(0, u64::saturating_add);
    let denominator = readable_total.max(disk_total);
    process_bytes
        .iter()
        .map(|&bytes| {
            if bytes == 0 || denominator == 0 {
                return (0.0, Vec::new());
            }
            let share = bytes as f64 / denominator as f64;
            let per_disk: Vec<(u32, f32)> = disks
                .iter()
                .filter(|(_, active, _)| *active > 0.0)
                .map(|(number, active, _)| (*number, (share * f64::from(*active)) as f32))
                .filter(|(_, pct)| *pct > 0.0)
                .collect();
            let busiest = per_disk.iter().map(|(_, pct)| *pct).fold(0.0, f32::max);
            (busiest, per_disk)
        })
        .collect()
}

/// Which optional providers the current [`TelemetryDemand`] keeps switched on.
///
/// The mapping follows the Windows sampler's *intent* for the providers Linux
/// actually has: the process table and adapter metadata are always sampled
/// (Windows samples them at every demand too), GPU utilization and VRAM are
/// only *measured* while a GPU bit is set, and `/proc/diskstats` is only read
/// while `DISK_RATE` is set. One honest difference, and one absent provider:
///
/// The per-CPU current frequency follows `CPU_SPEED`. Windows never gates its
/// sysinfo frequency refresh and falls back to it only when its measured counter
/// cannot be opened at all; Linux has no measured counter, so the gate covers the
/// whole reading and the CPU card shows "—" instead of a clock nobody asked for.
/// That is deliberate: the probe is a `scaling_cur_freq` read per CPU, and where
/// cpufreq is absent (VMs, containers) sysinfo re-reads the whole `/proc/cpuinfo`
/// once per CPU — its own comment calls that "very slow".
///
/// Per-process providers follow their Windows bits: `PROCESS_NET` runs the
/// `sock_diag` TCP accountant, `PROCESS_GPU`/`PROCESS_GPU_MEMORY` the per-process
/// GPU readers (DRM fdinfo, NVML), and `PROCESS_DISK` the per-process physical
/// I/O read and its disk-time attribution.
///
/// Bits whose provider Linux does not have (WLAN details, token security)
/// select nothing, and the columns they would fill stay unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Providers {
    /// Measure GPU utilization, VRAM use and temperature.
    pub measure_gpus: bool,
    /// Read `/proc/diskstats` and publish disk rates and active time.
    pub disk_rates: bool,
    /// Refresh the per-CPU current frequency.
    pub cpu_speed: bool,
    /// Per-process TCP throughput (`sock_diag`).
    pub process_net: bool,
    /// Per-process GPU engine time and memory.
    pub process_gpu: bool,
    /// Per-process physical disk I/O and disk-time share.
    pub process_disk: bool,
}

pub fn providers(demand: TelemetryDemand) -> Providers {
    Providers {
        measure_gpus: demand.any_gpu(),
        disk_rates: demand.wants(TelemetryDemand::DISK_RATE),
        cpu_speed: demand.wants(TelemetryDemand::CPU_SPEED),
        process_net: demand.wants(TelemetryDemand::PROCESS_NET),
        process_gpu: demand.wants(TelemetryDemand::PROCESS_GPU)
            || demand.wants(TelemetryDemand::PROCESS_GPU_MEMORY),
        process_disk: demand.wants(TelemetryDemand::PROCESS_DISK),
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
    fn page_lists_split_the_cache_into_modified_and_standby() {
        let meminfo = parse_meminfo(
            "MemFree: 1000 kB\nCached: 8000 kB\nSReclaimable: 500 kB\n\
             Shmem: 1500 kB\nDirty: 200 kB\nWriteback: 100 kB\nBuffers: 700 kB\n",
        );
        let (modified, standby, free) = page_lists(&meminfo).unwrap();
        assert_eq!(modified, 300 * 1024);
        // Regression: block-device buffers are cache too; leaving them out
        // put gigabytes of cache into "In use" on a machine with RAID/ext4
        // metadata traffic. 8000 + 700 + 500 - 1500 (shmem) - 300 (modified)
        assert_eq!(standby, 7400 * 1024);
        assert_eq!(free, 1000 * 1024);
        // A missing counter leaves the bar undrawn rather than claiming an
        // empty list.
        assert_eq!(page_lists(&parse_meminfo("Cached: 8000 kB\n")), None);
    }

    #[test]
    fn proc_io_reads_every_counter_and_nets_cancelled_writes() {
        let io = parse_proc_io(
            "rchar: 100\nwchar: 200\nsyscr: 3\nsyscw: 4\nread_bytes: 4096\n\
             write_bytes: 8192\ncancelled_write_bytes: 4096\n",
        )
        .unwrap();
        assert_eq!(
            io,
            ProcIo {
                rchar: 100,
                wchar: 200,
                syscr: 3,
                syscw: 4,
                read_bytes: 4096,
                write_bytes: 4096
            }
        );
        assert_eq!(parse_proc_io("rchar: 1\nwchar: 2\n"), None);
    }

    /// Regression: Linux rows read "DiscoverNotifie", "plasma-browser-" and
    /// "kactivitymanage" — the kernel's 15-byte `comm`, not the program name.
    #[test]
    fn truncated_comm_is_restored_from_the_image_or_argv() {
        let exe = Path::new("/usr/lib/DiscoverNotifier");
        assert_eq!(
            untruncated_comm("DiscoverNotifie", Some(exe), None),
            "DiscoverNotifier"
        );
        assert_eq!(
            untruncated_comm(
                "plasma-browser-",
                None,
                Some("/usr/bin/plasma-browser-integration-host /path")
            ),
            "plasma-browser-integration-host"
        );
        // An interpreter's comm is its script's name: argv[1].
        assert_eq!(
            untruncated_comm(
                "my-long-script-",
                Some(Path::new("/usr/bin/python3.13")),
                Some("/usr/bin/python3 /opt/tools/my-long-script-name.py")
            ),
            "my-long-script-name.py"
        );
        // Short names are complete; an unrelated candidate is not adopted.
        assert_eq!(
            untruncated_comm("bash", Some(Path::new("/usr/bin/zsh")), None),
            "bash"
        );
        assert_eq!(
            untruncated_comm(
                "worker-thread-x",
                Some(Path::new("/usr/bin/app")),
                Some("app")
            ),
            "worker-thread-x"
        );
    }

    #[test]
    fn virtualization_is_read_from_the_cpu_flags() {
        assert_eq!(
            virtualization_from_cpuinfo("flags\t\t: fpu vme svm lm\n"),
            "Enabled"
        );
        assert_eq!(virtualization_from_cpuinfo("flags\t: fpu vmx\n"), "Enabled");
        assert_eq!(
            virtualization_from_cpuinfo("flags\t: fpu vme lm\n"),
            "Unknown"
        );
        assert_eq!(virtualization_from_cpuinfo("model name : svm\n"), "Unknown");
    }

    #[test]
    fn elf_class_names_the_platform() {
        assert_eq!(elf_is_32bit(b"\x7fELF\x02"), Some(false));
        assert_eq!(elf_is_32bit(b"\x7fELF\x01"), Some(true));
        assert_eq!(elf_is_32bit(b"#!/bi"), None);
        assert_eq!(elf_is_32bit(b"\x7fEL"), None);
    }

    #[test]
    fn stat_faults_are_counted_after_a_hostile_comm() {
        let stat = b"42 (a) b) c) S 1 42 42 0 -1 4194304 77 0 9 0 5 6";
        assert_eq!(parse_proc_stat_faults(stat), Some((77, 9)));
        assert_eq!(parse_proc_stat_faults(b"42 (x) S 1"), None);
    }

    /// The share is of the drives' measured time, never of more than the
    /// drives moved: a user process writing 1 MiB while a root process this
    /// token cannot read moves 99 MiB gets 1 %, not 100 %, of the busy time.
    #[test]
    fn disk_time_is_shared_against_the_bytes_the_drives_moved() {
        let mib = 1024 * 1024;
        let shares = disk_time_shares(&[mib, 0], &[(0, 100.0, 100 * mib), (1, 0.0, 0)]);
        assert_eq!(shares[0].1, [(0, 1.0)]);
        assert_eq!(shares[0].0, 1.0);
        assert_eq!(shares[1], (0.0, Vec::new()));

        // Readable processes caused more than the drives reported (writes
        // still in the page cache): their own total is the denominator.
        let shares = disk_time_shares(&[3 * mib, mib], &[(0, 40.0, mib)]);
        assert_eq!(shares[0].0, 30.0);
        assert_eq!(shares[1].0, 10.0);
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

    #[test]
    fn thread_count_is_read_from_the_sample_loop_stat_record() {
        assert_eq!(parse_proc_stat_num_threads(PID_STAT), Some(4));
        assert_eq!(
            parse_proc_stat_num_threads(b"7 (a) S 1 7 7 0 -1 4194304 0 0 0 0 0 0 0 0 20 0"),
            None
        );
        assert_eq!(parse_proc_stat_num_threads(b""), None);
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
        sysfs.write("cpufreq/policy0/base_frequency", "2200000\n");
        sysfs.write("cpufreq/policy8/base_frequency", "3600000\n");
        sysfs.mkdir("cpufreq/policy4");
        sysfs.write("cpufreq/boost", "1\n");
        // A lower CPPC nominal clock never overrides the direct source.
        sysfs.write("cpu0/acpi_cppc/nominal_freq", "3000\n");
        assert_eq!(base_frequency_mhz(&sysfs.root), 3600.0);
        assert_eq!(base_frequency_mhz(&sysfs.root.join("absent")), 0.0);
        let empty = Sysfs::new("cpufreq-empty");
        assert_eq!(base_frequency_mhz(&empty.root), 0.0);
    }

    /// amd-pstate kernels that no longer publish `amd_pstate_nominal_freq` (and
    /// never publish `base_frequency`) still expose the firmware's CPPC nominal
    /// clock — the source a Ryzen 7 5700X reports its 3.4 GHz base through.
    /// The boost ceiling in `cpuinfo_max_freq` must never stand in for it.
    #[test]
    fn base_frequency_falls_back_to_amd_pstate_then_cppc_nominal() {
        let sysfs = Sysfs::new("cpufreq-amd");
        sysfs.write("cpufreq/policy0/cpuinfo_max_freq", "4866949\n");
        sysfs.write("cpufreq/policy0/amd_pstate_max_freq", "4866949\n");
        sysfs.write("cpu0/acpi_cppc/nominal_freq", "3401\n");
        sysfs.write("cpu1/acpi_cppc/nominal_freq", "3401\n");
        sysfs.write("cpu2/acpi_cppc/nominal_freq", "garbage\n");
        sysfs.write("cpufreq/policy0/amd_pstate_nominal_freq", "");
        assert_eq!(base_frequency_mhz(&sysfs.root), 3401.0);

        sysfs.write("cpufreq/policy0/amd_pstate_nominal_freq", "3400000\n");
        assert_eq!(base_frequency_mhz(&sysfs.root), 3400.0);
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
                process_net: true,
                process_gpu: true,
                process_disk: true,
            }
        );

        assert_eq!(
            providers(TelemetryDemand::GPU_ADAPTER),
            Providers {
                measure_gpus: true,
                ..Providers::default()
            }
        );
        for gpu in [
            TelemetryDemand::PROCESS_GPU,
            TelemetryDemand::PROCESS_GPU_MEMORY,
        ] {
            assert_eq!(
                providers(gpu),
                Providers {
                    measure_gpus: true,
                    process_gpu: true,
                    ..Providers::default()
                },
                "every GPU bit measures the adapters"
            );
        }
        assert_eq!(
            providers(TelemetryDemand::PROCESS_NET),
            Providers {
                process_net: true,
                ..Providers::default()
            }
        );
        assert_eq!(
            providers(TelemetryDemand::PROCESS_DISK),
            Providers {
                process_disk: true,
                ..Providers::default()
            }
        );
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
            TelemetryDemand::WIFI_DETAILS,
        ] {
            assert_eq!(providers(bit), Providers::default());
        }
    }

    /// The logical-processor count comes from the `/proc/stat` line count, not
    /// from a delta window: a first tick and a core-set change both leave the
    /// percentages unavailable while the machine still has N processors.
    #[test]
    fn cpu_count_counts_the_per_cpu_lines_only() {
        let stat = "cpu  1 2 3 4\ncpu0 1 2 3 4\ncpu1 1 2 3 4\nintr 5\nctxt 6\n";
        assert_eq!(cpu_count(stat), 2);
        assert_eq!(
            cpu_count("cpu  1 2 3\n"),
            0,
            "the aggregate line is not a CPU"
        );
        assert_eq!(cpu_count("cpu 1 2\ncpufreq 9\n"), 0);
        assert_eq!(cpu_count(""), 0);
        assert_eq!(cpu_count("cpu12 0 0\ncpu2 0 0\ncpu30 0 0\n"), 3);
    }

    /// A cached DRM identity survives exactly as long as sysfs still lists the
    /// same cards at the same paths with the same device behind them.
    #[test]
    fn card_identities_drop_on_add_remove_renumber_or_device_change() {
        let card = |name: &str, device: &str| {
            (
                PathBuf::from(name),
                Some(PathBuf::from(format!("../../../{device}"))),
            )
        };
        let cached = [card("/sys/class/drm/card0", "0000:01:00.0")];
        assert!(card_identities_hold(&cached, &cached));
        // A card was probed: the cached list no longer describes sysfs.
        assert!(!card_identities_hold(
            &cached,
            &[
                card("/sys/class/drm/card0", "0000:01:00.0"),
                card("/sys/class/drm/card1", "0000:02:00.0"),
            ]
        ));
        // The card is gone.
        assert!(!card_identities_hold(&cached, &[]));
        // The number was reused by another device.
        assert!(!card_identities_hold(
            &cached,
            &[card("/sys/class/drm/card0", "0000:02:00.0")]
        ));
        // A card without a device link matches only another card without one.
        let bare = [(PathBuf::from("/sys/class/drm/card0"), None)];
        assert!(card_identities_hold(&bare, &bare));
        assert!(!card_identities_hold(&cached, &bare));
    }
}
