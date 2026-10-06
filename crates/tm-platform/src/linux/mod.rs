//! Linux backend: sysinfo + procfs/sysfs + systemd + XDG autostart.

mod diskstats;
pub(crate) mod services;
mod startup;

use crate::actions::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{
    Disks, MemoryRefreshKind, Networks, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
    UpdateKind,
};
use tm_core::classify;
use tm_core::engine::SystemCollector;
use tm_core::error::Result;
use tm_core::model::*;

pub struct LinuxCollector {
    sys: System,
    disks: Disks,
    networks: Networks,
    users: sysinfo::Users,
    prev_net_totals: HashMap<String, (u64, u64)>,
    last_tick: Option<Instant>,
    first_tick_done: bool,
    /// L1/L2/L3 totals in KB; cache geometry is static, so the sysfs walk
    /// over every CPU runs once instead of every tick.
    cache_kb: [u64; 3],
}

impl SystemCollector for LinuxCollector {
    fn backend_name(&self) -> &'static str {
        "linux/sysinfo+procfs+sysfs"
    }

    fn sample(&mut self, started: Instant) -> Result<Snapshot> {
        let interval_s = self
            .last_tick
            .map(|t| started.duration_since(t).as_secs_f64())
            .unwrap_or(1.0)
            .clamp(0.05, 3600.0);
        self.last_tick = Some(started);

        self.sys.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(
                    sysinfo::CpuRefreshKind::nothing()
                        .with_cpu_usage()
                        .with_frequency(),
                )
                .with_memory(MemoryRefreshKind::nothing().with_ram().with_swap()),
        );
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_disk_usage()
                .with_user(UpdateKind::OnlyIfNotSet)
                .with_exe(UpdateKind::OnlyIfNotSet),
        );
        self.disks.refresh(true);
        self.networks.refresh(true);

        let cpus = self.sys.cpus();
        let logical = cpus.len().max(1);
        let utilization = cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / logical as f32;
        let freq = cpus.iter().map(|c| c.frequency() as f32).sum::<f32>() / logical as f32;
        let nb_cpus = logical as f32;
        let (physical_cores, sockets) = cpu_topology();

        // Wayland intentionally prevents arbitrary clients from enumerating
        // every other client's top-level windows. Keep classification honest
        // rather than introducing compositor-specific unsafe heuristics.
        let window_owners: HashSet<u32> = HashSet::new();

        let n_procs = self
            .sys
            .processes()
            .values()
            .filter(|p| p.thread_kind().is_none())
            .count();
        let mut processes = Vec::with_capacity(n_procs);
        for (pid, p) in self.sys.processes() {
            // sysinfo 0.39 keeps `tasks` enabled even in
            // `ProcessRefreshKind::nothing()`, so every Linux task (thread) is
            // inserted into the process map as its own entry. Windows Task
            // Manager never lists threads as processes, and each thread entry
            // repeats the parent's memory while splitting its CPU. Skip them;
            // `p.tasks()` on the real process still feeds the Details thread
            // count.
            if p.thread_kind().is_some() {
                continue;
            }
            let pid_u = pid.as_u32();
            let name = p.name().to_string_lossy().into_owned();

            let has_window = window_owners.contains(&pid_u);
            let category = classify::classify(classify::ClassifyInput {
                pid: pid_u,
                name: &name,
                has_window,
                critical: None,
                windows_owned: None,
            });

            let du = p.disk_usage();
            let mut entry = ProcessEntry::new(pid_u, name.clone());
            entry.display = name.clone();
            entry.ppid = p.parent().map(|x| x.as_u32());
            entry.category = category;
            entry.cpu_pct = (p.cpu_usage() / nb_cpus).clamp(0.0, 100.0);
            entry.mem_bytes = p.memory();
            entry.working_set_bytes = Some(p.memory());
            entry.commit_bytes = Some(p.virtual_memory());
            entry.peak_mem_bytes = proc_status_kb(pid_u, "VmHWM");
            entry.start_epoch_s = Some(p.start_time() as i64);
            entry.cpu_time_s = Some(p.accumulated_cpu_time() as f64 / 1000.0);
            entry.disk_read_bps = du.read_bytes as f64 / interval_s;
            entry.disk_write_bps = du.written_bytes as f64 / interval_s;
            entry.disk_read_total = du.total_read_bytes;
            entry.disk_write_total = du.total_written_bytes;
            entry.has_window = has_window;
            entry.exe_path = p.exe().map(|e| e.to_path_buf());
            entry.user = p
                .user_id()
                .and_then(|uid| username_for_uid(&self.users, uid));
            entry.status = match p.status() {
                sysinfo::ProcessStatus::Stop => ProcStatus::Suspended,
                _ => ProcStatus::Running,
            };
            // sysinfo's task set excludes the leader; Windows' count includes
            // it, so add the main thread back for parity.
            entry.threads = p.tasks().map(|tasks| tasks.len() as u32 + 1);
            entry.handles = fd_count(pid_u);
            entry.command_line = proc_cmdline(pid_u);
            entry.priority = proc_priority(pid_u);
            entry.elevated = entry.user.as_deref().map(|u| u == "root");
            processes.push(entry);
        }

        let diskstats = diskstats::read();
        let mut disks = Vec::new();
        for d in self.disks.list() {
            let mount = d.mount_point().to_string_lossy().to_string();
            let dev = d.name().to_string_lossy().to_string();
            let block = block_device_name(&dev);
            let parent = parent_block_device(&block);
            let media = match d.kind() {
                sysinfo::DiskKind::SSD => MediaKind::Ssd,
                sysinfo::DiskKind::HDD => MediaKind::Hdd,
                _ => MediaKind::Unknown,
            };
            let ds = diskstats
                .iter()
                .find(|s| s.device == block || parent.as_deref() == Some(s.device.as_str()));
            disks.push(DiskInfo {
                id: dev.clone(),
                mount,
                label: String::new(),
                media,
                total_bytes: d.total_space(),
                free_bytes: d.available_space(),
                active_pct: ds.and_then(|s| s.active_pct(interval_s)),
                // The model has no "unknown" byte rate; an unmatched device
                // or a first sample is reported as 0 B/s (active time above
                // stays None, so the page still shows it as unmeasured).
                read_bps: ds.and_then(|s| s.read_bps(interval_s)).unwrap_or(0.0),
                write_bps: ds.and_then(|s| s.write_bps(interval_s)).unwrap_or(0.0),
                avg_resp_ms: 0.0,
                total_read_bytes: ds.map(|s| s.read_sectors * 512).unwrap_or(0),
                total_written_bytes: ds.map(|s| s.write_sectors * 512).unwrap_or(0),
            });
        }

        let mut nets = Vec::new();
        for (name, data) in &self.networks {
            let recv_total = data.total_received();
            let sent_total = data.total_transmitted();
            let (recv_bps, sent_bps) = match (
                self.prev_net_totals.get(name.as_str()).copied(),
                self.first_tick_done,
            ) {
                (Some((pr, ps)), true) => (
                    recv_total.saturating_sub(pr) as f64 / interval_s,
                    sent_total.saturating_sub(ps) as f64 / interval_s,
                ),
                _ => (0.0, 0.0),
            };
            let meta = net_meta(name);
            nets.push(NetworkInfo {
                name: name.to_string(),
                desc: meta.desc,
                kind: meta.kind,
                oper_up: meta.oper_up,
                recv_bps,
                sent_bps,
                total_recv_bytes: recv_total,
                total_sent_bytes: sent_total,
                link_bps: meta.link_bps,
                ssid: None,
                ipv4: None,
                ipv6: None,
                signal_quality_pct: None,
                wifi_access_denied: false,
            });
        }
        self.prev_net_totals = nets
            .iter()
            .map(|n| (n.name.clone(), (n.total_recv_bytes, n.total_sent_bytes)))
            .collect();

        let snap = Snapshot {
            timestamp_ms: now_ms(),
            sample_duration_ms: started.elapsed().as_millis() as u64,
            cpu: CpuInfo {
                brand: cpus
                    .first()
                    .map(|c| c.brand().to_string())
                    .unwrap_or_default(),
                vendor: cpus
                    .first()
                    .map(|c| c.vendor_id().to_string())
                    .unwrap_or_default(),
                architecture: std::env::consts::ARCH.into(),
                utilization_pct: utilization.clamp(0.0, 100.0),
                per_core_pct: cpus
                    .iter()
                    .map(|c| c.cpu_usage().clamp(0.0, 100.0))
                    .collect(),
                per_core_kernel_pct: Vec::new(),
                kernel_pct: 0.0,
                freq_mhz: freq,
                freq_base_mhz: base_freq_from_cpufreq(),
                logical_count: logical,
                physical_cores,
                sockets,
                l1_kb: self.cache_kb[0],
                l2_kb: self.cache_kb[1],
                l3_kb: self.cache_kb[2],
                virtualization: "Unknown".into(),
            },
            memory: MemoryInfo {
                total_bytes: self.sys.total_memory(),
                used_bytes: self.sys.used_memory(),
                available_bytes: self.sys.available_memory(),
                cached_bytes: proc_meminfo_field("Cached")
                    .saturating_add(proc_meminfo_field("SReclaimable")),
                commit_total_bytes: proc_meminfo_field("CommitLimit"),
                commit_used_bytes: proc_meminfo_field("Committed_AS"),
                paged_pool_bytes: 0,
                non_paged_pool_bytes: 0,
                swap_total_bytes: self.sys.total_swap(),
                swap_used_bytes: self.sys.used_swap(),
                ..Default::default()
            },
            disks,
            networks: nets,
            gpus: drm_gpus(),
            processes,
            system: SystemMisc {
                hostname: hostname(),
                os_name: System::name().unwrap_or_else(|| "Linux".into()),
                os_version: System::os_version().unwrap_or_default(),
                kernel_version: System::kernel_version().unwrap_or_default(),
                uptime_s: System::uptime(),
                boot_epoch_s: System::boot_time() as i64,
                process_count: n_procs,
                thread_count: threads_total(&self.sys),
                handle_count: open_file_handles(),
            },
        };
        self.first_tick_done = true;
        Ok(snap)
    }
}

fn username_for_uid(users: &sysinfo::Users, uid: &sysinfo::Uid) -> Option<String> {
    users
        .list()
        .iter()
        .find(|u| u.id() == uid)
        .map(|u| u.name().to_string())
}

fn proc_meminfo_field(field: &str) -> u64 {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            if line.split(':').next() == Some(field) {
                let num: String = line
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(kb) = num.parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    0
}

fn proc_status_kb(pid: u32, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if line.split(':').next() == Some(field) {
            let kb = line
                .split_whitespace()
                .find_map(|part| part.parse::<u64>().ok())?;
            return Some(kb * 1024);
        }
    }
    None
}

fn fd_count(pid: u32) -> Option<u32> {
    let count = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count();
    u32::try_from(count).ok()
}

fn proc_cmdline(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let parts: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Linux nice value from /proc/<pid>/stat field 19, mapped onto the portable
/// Task Manager priority classes. Parsing after the final ')' avoids spaces
/// and parentheses in process names corrupting positional fields.
fn proc_priority(pid: u32) -> PriorityClass {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return PriorityClass::Unknown;
    };
    let Some(end_name) = stat.rfind(')') else {
        return PriorityClass::Unknown;
    };
    let fields: Vec<&str> = stat[end_name + 1..].split_whitespace().collect();
    // After comm, field[0] is stat field 3. Nice is stat field 19 => index 16.
    let Some(nice) = fields.get(16).and_then(|s| s.parse::<i32>().ok()) else {
        return PriorityClass::Unknown;
    };
    priority_for_nice(nice)
}

/// The nice value `set_priority` writes for each class, and the anchors
/// `priority_for_nice` reads back against: one table, so a class that was
/// set always reads back as itself.
const NICE_TABLE: [(PriorityClass, i32); 5] = [
    (PriorityClass::High, -5),
    (PriorityClass::AboveNormal, -2),
    (PriorityClass::Normal, 0),
    (PriorityClass::BelowNormal, 5),
    (PriorityClass::Low, 10),
];

/// Nice value for a requested class. Nice has no realtime level, so Realtime
/// lands on High, the same silent downgrade Windows applies to callers that
/// lack the increase-base-priority privilege. `Unknown` is not a target.
fn nice_for_priority(priority: PriorityClass) -> Option<i32> {
    let class = match priority {
        PriorityClass::Realtime => PriorityClass::High,
        other => other,
    };
    NICE_TABLE
        .iter()
        .find(|(c, _)| *c == class)
        .map(|&(_, nice)| nice)
}

/// Class for an observed nice value: the most extreme table entry on the
/// same side of 0 that the value reaches. Values short of every entry on
/// their side (e.g. -1) still read as that side's mildest class, so a
/// raised or lowered process never reads as Normal.
fn priority_for_nice(nice: i32) -> PriorityClass {
    if nice == 0 {
        return PriorityClass::Normal;
    }
    let same_side = || {
        NICE_TABLE
            .iter()
            .filter(move |(_, v)| v.signum() == nice.signum())
    };
    same_side()
        .filter(|(_, v)| v.abs() <= nice.abs())
        .max_by_key(|(_, v)| v.abs())
        .or_else(|| same_side().min_by_key(|(_, v)| v.abs()))
        .map_or(PriorityClass::Unknown, |&(class, _)| class)
}

/// Thread ids of `pid`, leader first. Despite their names,
/// `setpriority(PRIO_PROCESS)` and `sched_setaffinity` act on the single
/// thread whose id they are given on Linux, so a process-wide change has to
/// visit every task. Falls back to the leader alone when the task list
/// cannot be read.
fn process_tids(pid: u32) -> Vec<u32> {
    let tids = std::fs::read_dir(format!("/proc/{pid}/task"))
        .map(|dir| {
            dir.flatten()
                .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    leader_first(pid, tids)
}

fn leader_first(pid: u32, tids: impl IntoIterator<Item = u32>) -> Vec<u32> {
    let mut out = vec![pid];
    out.extend(tids.into_iter().filter(|&tid| tid != pid));
    out
}

/// Applies a per-thread call to every thread in `tids` (leader first). The
/// leader's failure is the process's failure and leaves the other threads
/// untouched; the rest are best effort, and a thread that exited between
/// listing and the call (ESRCH) is not a miss.
fn apply_to_threads(
    op: &'static str,
    tids: &[u32],
    mut apply: impl FnMut(u32) -> std::io::Result<()>,
) -> Result<()> {
    let Some((&leader, rest)) = tids.split_first() else {
        return Err(tm_core::TmError::platform(op, "no thread to update"));
    };
    apply(leader).map_err(|e| tm_core::TmError::platform(op, e.to_string()))?;
    let missed = rest
        .iter()
        .filter(|&&tid| apply(tid).is_err_and(|e| e.raw_os_error() != Some(libc::ESRCH)))
        .count();
    if missed > 0 {
        tracing::warn!(
            pid = leader,
            op,
            missed,
            threads = tids.len(),
            "process-wide change did not reach every thread"
        );
    }
    Ok(())
}

fn cpu_topology() -> (usize, usize) {
    let mut cores: HashSet<(u32, u32)> = HashSet::new();
    let mut packages: HashSet<u32> = HashSet::new();
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let topo = entry.path().join("topology");
        let package = read_u32(topo.join("physical_package_id"));
        let core = read_u32(topo.join("core_id"));
        if let (Some(package), Some(core)) = (package, core) {
            packages.insert(package);
            cores.insert((package, core));
        }
    }
    (cores.len(), packages.len())
}

fn read_u32(path: impl AsRef<Path>) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// One `/sys/devices/system/cpu/cpuN/cache/indexM` entry. Unreadable fields
/// stay `None` so the level's total reads as unknown rather than silently
/// undercounting.
struct CacheIndex {
    level: u32,
    /// "Data", "Instruction" or "Unified".
    kind: Option<String>,
    shared_cpu_list: Option<String>,
    size_kb: Option<u64>,
}

fn read_cache_indexes() -> Vec<CacheIndex> {
    let Ok(cpus) = std::fs::read_dir("/sys/devices/system/cpu") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for cpu in cpus.flatten() {
        let name = cpu.file_name().to_string_lossy().to_string();
        if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(indexes) = std::fs::read_dir(cpu.path().join("cache")) else {
            continue;
        };
        for index in indexes.flatten() {
            if !index.file_name().to_string_lossy().starts_with("index") {
                continue;
            }
            let path = index.path();
            let Some(level) = read_u32(path.join("level")) else {
                continue;
            };
            let read = |file: &str| {
                std::fs::read_to_string(path.join(file))
                    .ok()
                    .map(|s| s.trim().to_string())
            };
            out.push(CacheIndex {
                level,
                kind: read("type"),
                shared_cpu_list: read("shared_cpu_list"),
                size_kb: read("size").and_then(|s| parse_cache_size(&s)),
            });
        }
    }
    out
}

/// L1/L2/L3 totals in KB, summing every distinct cache instance the way the
/// Windows collector totals each cache it enumerates. sysfs lists a cache
/// once per logical CPU that shares it, so an instance is identified by
/// (level, type, shared_cpu_list): SMT siblings collapse onto one core cache
/// while L1d and L1i, which share a CPU list, stay separate. A level with an
/// unreadable instance reports 0 (unknown).
fn cache_totals_kb(indexes: &[CacheIndex]) -> [u64; 3] {
    let mut seen = HashSet::new();
    let mut totals = [Some(0u64); 3];
    for index in indexes {
        let Some(total) = (index.level as usize)
            .checked_sub(1)
            .and_then(|i| totals.get_mut(i))
        else {
            continue;
        };
        match (&index.kind, &index.shared_cpu_list, index.size_kb) {
            (Some(kind), Some(shared), Some(size)) => {
                if seen.insert((index.level, kind.as_str(), shared.as_str())) {
                    *total = total.map(|t| t.saturating_add(size));
                }
            }
            _ => *total = None,
        }
    }
    totals.map(|t| t.unwrap_or(0))
}

fn parse_cache_size(text: &str) -> Option<u64> {
    let t = text.trim().to_ascii_lowercase();
    if let Some(num) = t.strip_suffix('k') {
        num.trim().parse().ok()
    } else if let Some(num) = t.strip_suffix('m') {
        num.trim().parse::<u64>().ok().map(|n| n * 1024)
    } else {
        None
    }
}

fn base_freq_from_cpufreq() -> f32 {
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu/cpufreq") else {
        return 0.0;
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("policy"))
        .filter_map(|e| {
            std::fs::read_to_string(e.path().join("base_frequency"))
                .ok()?
                .trim()
                .parse::<f32>()
                .ok()
        })
        .map(|khz| khz / 1000.0)
        .fold(0.0f32, f32::max)
}

struct NetMeta {
    kind: String,
    oper_up: bool,
    link_bps: u64,
    desc: String,
}

fn net_meta(name: &str) -> NetMeta {
    let base = Path::new("/sys/class/net").join(name);
    let kind = if name == "lo" {
        "Loopback"
    } else if base.join("wireless").exists() {
        "Wi-Fi"
    } else if base.join("device").exists() {
        "Ethernet"
    } else {
        "Virtual"
    }
    .to_string();
    let operstate = std::fs::read_to_string(base.join("operstate"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let carrier = std::fs::read_to_string(base.join("carrier"))
        .ok()
        .is_some_and(|s| s.trim() == "1");
    let oper_up = matches!(operstate.as_str(), "up" | "unknown") && (carrier || name == "lo");
    let link_bps = std::fs::read_to_string(base.join("speed"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&mbps| mbps > 0 && mbps < 1_000_000)
        .map_or(0, |mbps| mbps * 1_000_000);
    let desc = std::fs::read_to_string(base.join("device/uevent"))
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("DRIVER=").map(str::to_string))
        })
        .unwrap_or_default();
    NetMeta {
        kind,
        oper_up,
        link_bps,
        desc,
    }
}

/// Kernel block device name for a mount source, as /proc/diskstats lists
/// it. LVM/LUKS volumes mount as `/dev/mapper/<name>` symlinks to
/// `/dev/dm-N`, and diskstats only knows `dm-N`, so resolve links first.
fn block_device_name(dev: &str) -> String {
    let resolved = std::fs::canonicalize(dev).ok();
    resolved
        .as_deref()
        .unwrap_or(Path::new(dev))
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| dev.to_string())
}

fn parent_block_device(dev: &str) -> Option<String> {
    let class = Path::new("/sys/class/block").join(dev);
    if !class.join("partition").exists() {
        return None;
    }
    let target = std::fs::canonicalize(class).ok()?;
    target
        .parent()?
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_default())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn threads_total(sys: &System) -> usize {
    sys.processes()
        .values()
        .filter(|p| p.thread_kind().is_none())
        .map(|p| p.tasks().map_or(1, |tasks| tasks.len() + 1))
        .sum()
}

/// System-wide open file handles: allocated `struct file`s (every fd,
/// socket and pipe) from /proc/sys/fs/file-nr, the Linux analog of the
/// Windows handle total. The model has no "unknown" for this counter, so an
/// unreadable file still yields 0.
fn open_file_handles() -> usize {
    std::fs::read_to_string("/proc/sys/fs/file-nr")
        .ok()
        .and_then(|text| parse_file_nr(&text))
        .unwrap_or(0)
}

/// First field of file-nr ("allocated  free  max").
fn parse_file_nr(text: &str) -> Option<usize> {
    text.split_whitespace().next()?.parse().ok()
}

fn drm_gpus() -> Vec<GpuInfo> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        let mut cards: Vec<_> = entries
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with("card") && n[4..].chars().all(|c| c.is_ascii_digit())
            })
            .collect();
        cards.sort_by_key(|e| e.file_name());
        for card in cards {
            let device = card.path().join("device");
            let read = |file: &str| {
                std::fs::read_to_string(device.join(file))
                    .ok()
                    .map(|t| t.trim().to_string())
            };
            let gpu = drm_card_gpu(
                out.len(),
                read("gpu_busy_percent").and_then(|t| t.parse().ok()),
                read("mem_info_vram_used").and_then(|t| t.parse().ok()),
                read("mem_info_vram_total").and_then(|t| t.parse().ok()),
            );
            out.extend(gpu);
        }
    }
    out
}

/// One DRM card's adapter entry, or `None` when the driver exposes no
/// utilization or VRAM-usage source. Only amdgpu publishes
/// `gpu_busy_percent`/`mem_info_vram_*`; i915, xe, nouveau, nvidia and
/// simpledrm publish neither, and the model has no "unknown" for
/// utilization or dedicated memory, so such a card is left out rather than
/// shown as an idle, empty GPU. A missing VRAM total stays 0, which the
/// page already treats as unknown.
fn drm_card_gpu(
    id: usize,
    busy_pct: Option<f32>,
    vram_used: Option<u64>,
    vram_total: Option<u64>,
) -> Option<GpuInfo> {
    let busy = busy_pct?;
    let vram_used = vram_used?;
    Some(GpuInfo {
        id,
        name: format!("GPU {id}"),
        driver_version: String::new(),
        util_pct: busy,
        mem_used_bytes: vram_used,
        mem_total_bytes: vram_total.unwrap_or(0),
        dedicated_used_bytes: vram_used,
        shared_used_bytes: 0,
        temperature_c: None,
        luid: None,
        engines: vec![GpuEngine {
            name: "3D".into(),
            util_pct: busy,
        }],
    })
}

// ------------------------------------------------------------------ actions

pub struct LinuxActions;

impl PlatformActions for LinuxActions {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            services_list: true,
            services_control: true,
            startup_toggle: true,
            users_sessions: false,
            user_disconnect: false,
            end_process: true,
            suspend_resume: true,
            set_priority: true,
            set_affinity: true,
            efficiency_mode: false,
            run_new_task: true,
            per_process_network: false,
            process_modules: false,
            unload_module: false,
            start_with_windows: false,
        }
    }

    fn list_services(&self) -> Result<Vec<ServiceInfo>> {
        services::list_systemd_units()
    }
    fn control_service(&self, name: &str, action: ServiceAction) -> Result<()> {
        services::control_unit(name, action)
    }
    fn list_startup(&self) -> Result<Vec<StartupItem>> {
        Ok(startup::list_autostart())
    }
    fn set_startup_enabled(&self, item_id: &str, _location: &str, enabled: bool) -> Result<()> {
        startup::set_enabled(item_id, enabled)
    }

    fn kill_single(&self, pid: u32) -> Result<()> {
        send_signal(pid, libc::SIGTERM)
    }
    fn suspend_process(&self, pid: u32, suspend: bool) -> Result<()> {
        send_signal(
            pid,
            if suspend {
                libc::SIGSTOP
            } else {
                libc::SIGCONT
            },
        )
    }
    fn set_priority(&self, pid: u32, priority: PriorityClass) -> Result<()> {
        let Some(nice) = nice_for_priority(priority) else {
            return Err(tm_core::TmError::platform(
                "setpriority",
                "an explicit priority class is required",
            ));
        };
        apply_to_threads("setpriority", &process_tids(pid), |tid| {
            let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        })
    }
    fn get_affinity_mask(&self, pid: u32) -> Result<u64> {
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(pid as i32, std::mem::size_of::<libc::cpu_set_t>(), &mut set)
                != 0
            {
                return Err(tm_core::TmError::platform("sched_getaffinity", "failed"));
            }
            let mut mask = 0u64;
            for cpu in 0usize..64 {
                if libc::CPU_ISSET(cpu, &set) {
                    mask |= 1 << cpu;
                }
            }
            Ok(mask)
        }
    }
    fn system_affinity_mask(&self) -> Result<u64> {
        self.get_affinity_mask(std::process::id())
    }
    fn set_affinity_mask(&self, pid: u32, mask: u64) -> Result<()> {
        let set = unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            for cpu in 0usize..64 {
                if mask & (1u64 << cpu) != 0 {
                    libc::CPU_SET(cpu, &mut set);
                }
            }
            set
        };
        apply_to_threads("sched_setaffinity", &process_tids(pid), |tid| {
            let rc = unsafe {
                libc::sched_setaffinity(
                    tid as libc::pid_t,
                    std::mem::size_of::<libc::cpu_set_t>(),
                    &set,
                )
            };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        })
    }

    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        use std::process::{Command, Stdio};
        let mut cmd = if elevate {
            let mut c = Command::new("pkexec");
            c.arg("sh").arg("-c").arg(command_line);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command_line);
            c
        };
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| tm_core::TmError::platform("spawn", e.to_string()))
    }

    fn backend_name(&self) -> &'static str {
        "linux"
    }
}

fn send_signal(pid: u32, sig: i32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if rc == 0 {
        Ok(())
    } else {
        Err(tm_core::TmError::platform(
            "kill",
            format!("signal {sig} failed"),
        ))
    }
}

pub fn create_collector() -> LinuxCollector {
    LinuxCollector {
        sys: System::new_all(),
        disks: Disks::new_with_refreshed_list(),
        networks: Networks::new_with_refreshed_list(),
        users: sysinfo::Users::new_with_refreshed_list(),
        prev_net_totals: HashMap::new(),
        last_tick: None,
        first_tick_done: false,
        cache_kb: cache_totals_kb(&read_cache_indexes()),
    }
}

pub fn create_actions() -> LinuxActions {
    LinuxActions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_settable_priority_reads_back_as_itself() {
        for class in [
            PriorityClass::High,
            PriorityClass::AboveNormal,
            PriorityClass::Normal,
            PriorityClass::BelowNormal,
            PriorityClass::Low,
        ] {
            let nice = nice_for_priority(class).expect("settable class");
            assert_eq!(priority_for_nice(nice), class, "nice {nice}");
        }
        // Realtime has no nice level: it is written as High and reads so.
        assert_eq!(
            nice_for_priority(PriorityClass::Realtime),
            nice_for_priority(PriorityClass::High)
        );
        assert_eq!(nice_for_priority(PriorityClass::Unknown), None);
    }

    #[test]
    fn nice_between_anchors_rounds_toward_normal_but_never_to_it() {
        assert_eq!(priority_for_nice(-20), PriorityClass::High);
        assert_eq!(priority_for_nice(-4), PriorityClass::AboveNormal);
        assert_eq!(priority_for_nice(-1), PriorityClass::AboveNormal);
        assert_eq!(priority_for_nice(1), PriorityClass::BelowNormal);
        assert_eq!(priority_for_nice(9), PriorityClass::BelowNormal);
        assert_eq!(priority_for_nice(19), PriorityClass::Low);
    }

    #[test]
    fn process_wide_change_visits_every_thread_leader_first() {
        let tids = leader_first(100, [102, 100, 101]);
        assert_eq!(tids, [100, 102, 101]);

        let mut visited = Vec::new();
        let gone = |tid| (tid == 102).then(|| std::io::Error::from_raw_os_error(libc::ESRCH));
        apply_to_threads("test", &tids, |tid| {
            visited.push(tid);
            gone(tid).map_or(Ok(()), Err)
        })
        .expect("a thread that exited mid-change is not a failure");
        assert_eq!(visited, [100, 102, 101]);
    }

    #[test]
    fn leader_failure_fails_the_change_and_spares_other_threads() {
        let mut visited = Vec::new();
        let result = apply_to_threads("test", &[100, 101], |tid| {
            visited.push(tid);
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        });
        assert!(result.is_err());
        assert_eq!(visited, [100]);
    }

    fn cache(level: u32, kind: &str, shared: &str, size_kb: u64) -> CacheIndex {
        CacheIndex {
            level,
            kind: Some(kind.into()),
            shared_cpu_list: Some(shared.into()),
            size_kb: Some(size_kb),
        }
    }

    /// 8 SMT cores (16 logical CPUs): per-core 32K L1d + 32K L1i and 512K
    /// L2, one 32M L3; each listed under every logical CPU, as sysfs does.
    fn smt_8_core_caches() -> Vec<CacheIndex> {
        let mut out = Vec::new();
        for cpu in 0..16 {
            let core = cpu % 8;
            let siblings = format!("{core},{}", core + 8);
            out.push(cache(1, "Data", &siblings, 32));
            out.push(cache(1, "Instruction", &siblings, 32));
            out.push(cache(2, "Unified", &siblings, 512));
            out.push(cache(3, "Unified", "0-15", 32 * 1024));
        }
        out
    }

    #[test]
    fn cache_totals_sum_every_distinct_instance() {
        assert_eq!(cache_totals_kb(&smt_8_core_caches()), [512, 4096, 32768]);
    }

    #[test]
    fn cache_level_with_unreadable_instance_is_unknown() {
        let mut caches = smt_8_core_caches();
        caches[2].size_kb = None; // one L2 instance
        assert_eq!(cache_totals_kb(&caches), [512, 0, 32768]);
    }

    #[test]
    fn cache_size_parses_sysfs_units() {
        assert_eq!(parse_cache_size("48K\n"), Some(48));
        assert_eq!(parse_cache_size("32M"), Some(32 * 1024));
        assert_eq!(parse_cache_size("garbage"), None);
    }

    #[test]
    fn mapper_mount_resolves_to_kernel_dm_name() {
        let dir = std::env::temp_dir().join(format!("tm-blockdev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mapper")).unwrap();
        std::fs::write(dir.join("dm-3"), b"").unwrap();
        let link = dir.join("mapper/vg-root");
        std::os::unix::fs::symlink("../dm-3", &link).unwrap();
        let name = block_device_name(&link.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(name, "dm-3");
        assert_eq!(block_device_name("/nonexistent/sda1"), "sda1");
    }

    #[test]
    fn gpu_without_utilization_or_vram_source_is_not_reported() {
        assert!(drm_card_gpu(0, None, None, None).is_none());
        assert!(drm_card_gpu(0, None, Some(1), Some(2)).is_none());
        assert!(drm_card_gpu(0, Some(5.0), None, Some(2)).is_none());

        let gpu = drm_card_gpu(1, Some(37.0), Some(512 << 20), None).expect("amdgpu-like card");
        assert_eq!(gpu.id, 1);
        assert_eq!(gpu.util_pct, 37.0);
        assert_eq!(gpu.dedicated_used_bytes, 512 << 20);
        assert_eq!(gpu.mem_total_bytes, 0);
    }

    #[test]
    fn file_nr_first_field_is_the_open_handle_count() {
        assert_eq!(
            parse_file_nr("12345\t0\t9223372036854775807\n"),
            Some(12345)
        );
        assert_eq!(parse_file_nr(""), None);
    }
}
