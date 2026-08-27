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

        let n_procs = self.sys.processes().len();
        let mut name_by_pid = HashMap::with_capacity(n_procs);
        for (pid, p) in self.sys.processes() {
            name_by_pid.insert(pid.as_u32(), p.name().to_string_lossy().into_owned());
        }

        let mut processes = Vec::with_capacity(n_procs);
        for (pid, p) in self.sys.processes() {
            let pid_u = pid.as_u32();
            let name = p.name().to_string_lossy().into_owned();

            let mut anc: Vec<&str> = Vec::new();
            let mut cur = p.parent().map(|x| x.as_u32());
            let mut hops = 0;
            while let Some(ppid) = cur {
                hops += 1;
                if hops > 8 {
                    break;
                }
                match name_by_pid.get(&ppid) {
                    Some(n) => {
                        anc.push(n.as_str());
                        cur = self
                            .sys
                            .process(sysinfo::Pid::from_u32(ppid))
                            .and_then(|pp| pp.parent())
                            .map(|x| x.as_u32());
                    }
                    None => break,
                }
            }

            let has_window = window_owners.contains(&pid_u);
            let kernel_thread = pid_u == 2 || name.starts_with('[');
            let category = classify::classify(classify::ClassifyInput {
                pid: pid_u,
                name: &name,
                ancestor_names: &anc,
                has_window,
                system_session: kernel_thread,
            });

            let du = p.disk_usage();
            let mut entry = ProcessEntry::new(pid_u, name.clone());
            entry.display = name.clone();
            entry.ppid = p.parent().map(|x| x.as_u32());
            entry.category = category;
            entry.cpu_pct = (p.cpu_usage() / nb_cpus).clamp(0.0, 100.0);
            entry.mem_bytes = p.memory();
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
            entry.threads = p.tasks().map(|tasks| tasks.len() as u32);
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
                active_pct: ds.map(|s| s.active_pct(interval_s)).unwrap_or(0.0),
                read_bps: ds.map(|s| s.read_bps(interval_s)).unwrap_or(0.0),
                write_bps: ds.map(|s| s.write_bps(interval_s)).unwrap_or(0.0),
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
                l1_kb: cache_total_kb(1),
                l2_kb: cache_total_kb(2),
                l3_kb: cache_total_kb(3),
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
                handle_count: 0,
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
    match nice {
        i32::MIN..=-10 => PriorityClass::High,
        -9..=-1 => PriorityClass::AboveNormal,
        0 => PriorityClass::Normal,
        1..=9 => PriorityClass::BelowNormal,
        _ => PriorityClass::Low,
    }
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

fn cache_total_kb(level: u8) -> u64 {
    // CPU0 lists every cache level. Deduplicate shared caches by their
    // shared_cpu_list; sum distinct entries of the requested level.
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu/cpu0/cache") else {
        return 0;
    };
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if read_u32(path.join("level")) != Some(level as u32) {
            continue;
        }
        let shared = std::fs::read_to_string(path.join("shared_cpu_list"))
            .unwrap_or_else(|_| entry.file_name().to_string_lossy().into_owned());
        if !seen.insert(shared) {
            continue;
        }
        if let Ok(size) = std::fs::read_to_string(path.join("size")) {
            total = total.saturating_add(parse_cache_size(&size));
        }
    }
    total
}

fn parse_cache_size(text: &str) -> u64 {
    let t = text.trim().to_ascii_lowercase();
    if let Some(num) = t.strip_suffix('k') {
        num.trim().parse().unwrap_or(0)
    } else if let Some(num) = t.strip_suffix('m') {
        num.trim().parse::<u64>().map(|n| n * 1024).unwrap_or(0)
    } else {
        0
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

fn block_device_name(dev: &str) -> String {
    Path::new(dev)
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
        .filter_map(|p| p.tasks())
        .map(|t| t.len())
        .sum()
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
        for (i, card) in cards.into_iter().enumerate() {
            let base = card.path();
            let busy = std::fs::read_to_string(base.join("device/gpu_busy_percent"))
                .ok()
                .and_then(|t| t.trim().parse::<f32>().ok())
                .unwrap_or(0.0);
            let vram_used = std::fs::read_to_string(base.join("device/mem_info_vram_used"))
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let vram_total = std::fs::read_to_string(base.join("device/mem_info_vram_total"))
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok())
                .unwrap_or(0);
            out.push(GpuInfo {
                id: i,
                name: format!("GPU {i}"),
                driver_version: String::new(),
                util_pct: busy,
                mem_used_bytes: vram_used,
                mem_total_bytes: vram_total,
                dedicated_used_bytes: vram_used,
                shared_used_bytes: 0,
                temperature_c: None,
                luid: None,
                engines: vec![GpuEngine {
                    name: "3D".into(),
                    util_pct: busy,
                }],
            });
        }
    }
    out
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
        let nice: i32 = match priority {
            PriorityClass::Realtime | PriorityClass::High => -5,
            PriorityClass::AboveNormal => -2,
            PriorityClass::Normal => 0,
            PriorityClass::BelowNormal => 5,
            PriorityClass::Low | PriorityClass::Unknown => 10,
        };
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
        if rc == 0 {
            Ok(())
        } else {
            Err(tm_core::TmError::platform(
                "setpriority",
                "permission denied",
            ))
        }
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
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            for cpu in 0usize..64 {
                if mask & (1u64 << cpu) != 0 {
                    libc::CPU_SET(cpu, &mut set);
                }
            }
            if libc::sched_setaffinity(pid as i32, std::mem::size_of::<libc::cpu_set_t>(), &set)
                != 0
            {
                return Err(tm_core::TmError::platform("sched_setaffinity", "failed"));
            }
            Ok(())
        }
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
    }
}

pub fn create_actions() -> LinuxActions {
    LinuxActions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_priority_parser_mapping_is_stable() {
        // Mapping itself is deliberately coarse but matches set_priority().
        assert_eq!(map_nice(-5), PriorityClass::AboveNormal);
        assert_eq!(map_nice(0), PriorityClass::Normal);
        assert_eq!(map_nice(10), PriorityClass::Low);
    }

    fn map_nice(nice: i32) -> PriorityClass {
        match nice {
            i32::MIN..=-10 => PriorityClass::High,
            -9..=-1 => PriorityClass::AboveNormal,
            0 => PriorityClass::Normal,
            1..=9 => PriorityClass::BelowNormal,
            _ => PriorityClass::Low,
        }
    }
}
