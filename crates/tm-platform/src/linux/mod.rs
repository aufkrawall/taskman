//! Linux backend: sysinfo + /proc + systemctl + XDG autostart.

mod diskstats;
pub(crate) mod services;
mod startup;

use crate::actions::*;
use std::collections::{HashMap, HashSet};
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
        "linux/sysinfo+procfs"
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

        // Window ownership requires a display connection; without one we
        // classify by heuristics only.
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
            let kernel_thread = pid_u == 2 || name.starts_with('['); // kthreadd children show in brackets
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
            entry.cpu_pct = (p.cpu_usage() / nb_cpus * 100.0).clamp(0.0, 100.0);
            entry.mem_bytes = p.memory();
            entry.commit_bytes = Some(p.virtual_memory());
            entry.start_epoch_s = Some(p.start_time() as i64);
            // accumulated_cpu_time is milliseconds on Linux.
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
            processes.push(entry);
        }

        let diskstats = diskstats::read();
        let mut disks = Vec::new();
        for d in self.disks.list() {
            let mount = d.mount_point().to_string_lossy().to_string();
            let dev = d.name().to_string_lossy().to_string(); // e.g. "nvme0n1p2" or "sda2"
            let media = match d.kind() {
                sysinfo::DiskKind::SSD => MediaKind::Ssd,
                sysinfo::DiskKind::HDD => MediaKind::Hdd,
                _ => MediaKind::Unknown,
            };
            // Match the parent device for partitions ("sda2" -> "sda").
            let ds = diskstats.iter().find(|s| {
                dev == s.device
                    || dev.starts_with(&format!("{}/", s.device)) == false
                        && dev.trim_end_matches(char::is_numeric) == s.device
            });
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
            nets.push(NetworkInfo {
                name: name.to_string(),
                desc: String::new(),
                kind: if name.contains("wl") {
                    "Wi-Fi".into()
                } else if name == "lo" {
                    "Loopback".into()
                } else {
                    "Ethernet".into()
                },
                oper_up: recv_total > 0 || sent_total > 0,
                recv_bps,
                sent_bps,
                total_recv_bytes: recv_total,
                total_sent_bytes: sent_total,
                link_bps: 0,
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
                freq_mhz: freq,
                freq_base_mhz: base_freq_from_cpufreq(),
                logical_count: logical,
                physical_cores: physical_cores(),
                sockets: 1,
                l1_kb: 0,
                l2_kb: 0,
                l3_kb: cache_l3_kb(),
                virtualization: "Unknown".into(),
            },
            memory: MemoryInfo {
                total_bytes: self.sys.total_memory(),
                used_bytes: self.sys.used_memory(),
                available_bytes: self.sys.available_memory(),
                cached_bytes: proc_meminfo_field("Cached"),
                commit_total_bytes: 0,
                commit_used_bytes: 0,
                paged_pool_bytes: 0,
                non_paged_pool_bytes: 0,
                swap_total_bytes: self.sys.total_swap(),
                swap_used_bytes: self.sys.used_swap(),
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
            if line.starts_with(field) {
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

fn physical_cores() -> usize {
    if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
        let mut ids = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("core id") {
                if let Some(v) = rest
                    .trim_start_matches(['\t', ' ', ':'])
                    .trim()
                    .parse::<u32>()
                    .ok()
                {
                    ids.push(v);
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        return ids.len();
    }
    0
}

fn cache_l3_kb() -> u64 {
    std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cache/index3/size")
        .map(|t| parse_cache_size(&t))
        .unwrap_or(0)
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
    if let Ok(khz) = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/base_frequency")
    {
        if let Ok(k) = khz.trim().parse::<f32>() {
            return k / 1000.0;
        }
    }
    0.0
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

/// GPU list from DRM sysfs (amdgpu/i915 expose busy %).
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
                temperature_c: None,
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

pub fn create() -> (LinuxCollector, LinuxActions) {
    (
        LinuxCollector {
            sys: System::new_all(),
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            users: sysinfo::Users::new_with_refreshed_list(),
            prev_net_totals: HashMap::new(),
            last_tick: None,
            first_tick_done: false,
        },
        LinuxActions,
    )
}
