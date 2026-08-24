//! sysinfo-driven sampler producing full `Snapshot`s on Windows.

use crate::win::{cpu_info, gpu, perfcounters, process_ops, threads_map, windows_enum};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{
    Disks, MemoryRefreshKind, Networks, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind,
    System, UpdateKind, Users,
};
use tm_core::classify;
use tm_core::engine::SystemCollector;
use tm_core::error::Result;
use tm_core::model::*;

/// Everything that carries state between ticks.
pub struct Sampler {
    sys: System,
    disks: Disks,
    networks: Networks,
    user_names: HashMap<sysinfo::Uid, String>,
    cpu_static: cpu_info::CpuStatic,
    prev_net_totals: HashMap<String, (u64, u64)>,
    last_tick: Option<Instant>,
    first_tick_done: bool,
    pdh: Mutex<perfcounters::Pdh>,
}

impl Sampler {
    pub fn new() -> Self {
        let mut users = Users::new_with_refreshed_list();
        let user_names = build_user_map(&users);
        users.refresh();

        let mut sys = System::new();
        sys.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(sysinfo::CpuRefreshKind::nothing().with_cpu_usage().with_frequency())
                .with_memory(MemoryRefreshKind::nothing().with_ram().with_swap()),
        );
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_disk_usage()
                .with_user(UpdateKind::Always)
                .with_exe(UpdateKind::OnlyIfNotSet),
        );

        Self {
            user_names,
            sys,
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            pdh: Mutex::new(perfcounters::Pdh::new()),
            cpu_static: cpu_info::CpuStatic::probe(),
            prev_net_totals: HashMap::new(),
            last_tick: None,
            first_tick_done: false,
        }
    }
}

impl SystemCollector for Sampler {
    fn backend_name(&self) -> &'static str {
        "windows/sysinfo+pdh"
    }

    fn sample(&mut self, started: Instant) -> Result<Snapshot> {
        self.sample_inner(started)
    }
}

impl Sampler {
    fn sample_inner(&mut self, started: Instant) -> Result<Snapshot> {
        let interval_s = self
            .last_tick
            .map(|t| started.duration_since(t).as_secs_f64())
            .unwrap_or(1.0)
            .clamp(0.05, 3600.0);
        self.last_tick = Some(started);

        // ---- refresh raw data --------------------------------------------------
        self.sys.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(sysinfo::CpuRefreshKind::nothing().with_cpu_usage().with_frequency())
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

        let window_owners: HashSet<u32> = windows_enum::visible_window_owners().into_iter().collect();
        let thread_counts = threads_map::thread_counts();

        // ---- CPU -----------------------------------------------------------------
        let cpus = self.sys.cpus();
        let logical = cpus.len().max(1);
        let utilization: f32 = cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / logical as f32;
        let freq: f32 = cpus.iter().map(|c| c.frequency() as f32).sum::<f32>() / logical as f32;

        // ---- memory ----------------------------------------------------------------
        let mem_total = self.sys.total_memory();
        let mem_used = self.sys.used_memory();
        let mem_avail = self.sys.available_memory();
        let win_mem = perfcounters::query_windows_memory();

        // ---- processes -----------------------------------------------------------------
        let nb_cpus = logical as f32;
        let n_procs = self.sys.processes().len();
        let mut processes = Vec::with_capacity(n_procs);

        let mut name_by_pid: HashMap<u32, String> = HashMap::with_capacity(n_procs);
        for (pid, p) in self.sys.processes() {
            name_by_pid.insert(pid.as_u32(), p.name().to_string_lossy().into_owned());
        }

        for (pid, p) in self.sys.processes() {
            let pid_u = pid.as_u32();
            let name = p.name().to_string_lossy().into_owned();

            // Walk ancestors (bounded) for classification.
            let mut anc: Vec<String> = Vec::new();
            let mut cur_pid = p.parent().map(|x| x.as_u32());
            let mut hops = 0;
            while let Some(ppid) = cur_pid {
                hops += 1;
                if hops > 8 {
                    break;
                }
                match name_by_pid.get(&ppid) {
                    Some(n) => {
                        anc.push(n.clone());
                        cur_pid = self
                            .sys
                            .process(Pid::from_u32(ppid))
                            .and_then(|pp| pp.parent())
                            .map(|x| x.as_u32());
                    }
                    None => break,
                }
            }

            let has_window = window_owners.contains(&pid_u);
            let session_id = process_ops::session_id_of(pid_u);

            let category = classify::classify(classify::ClassifyInput {
                pid: pid_u,
                name: &name,
                ancestor_names: &anc,
                has_window,
                system_session: session_id.map(|s| s == 0).unwrap_or(false),
            });

            let user = p
                .user_id()
                .and_then(|uid| self.user_names.get(uid).cloned());

            let du = p.disk_usage();

            let mut entry = ProcessEntry::new(pid_u, name.clone());
            entry.display = name.clone();
            entry.ppid = p.parent().map(|x| x.as_u32());
            entry.category = category;
            entry.status = map_status(p.status());
            entry.user = user;
            entry.session_id = session_id;
            entry.cpu_pct = (p.cpu_usage() / nb_cpus * 100.0).clamp(0.0, 100.0);
            entry.mem_bytes = p.memory();
            entry.commit_bytes = Some(p.virtual_memory());
            entry.start_epoch_s = Some(p.start_time() as i64);
            entry.cpu_time_s = Some(p.accumulated_cpu_time() as f64 / 1000.0);
            entry.disk_read_bps = du.read_bytes as f64 / interval_s;
            entry.disk_write_bps = du.written_bytes as f64 / interval_s;
            entry.disk_read_total = du.total_read_bytes;
            entry.disk_write_total = du.total_written_bytes;
            entry.has_window = has_window;
            entry.exe_path = p.exe().map(|e| e.to_path_buf());
            entry.priority = process_ops::priority_class_of(pid_u);
            entry.handles = process_ops::handle_count(pid_u);
            entry.wow64 = process_ops::is_wow64(pid_u);
            entry.threads = thread_counts.get(&pid_u).copied();
            processes.push(entry);
        }

        // ---- GPU ---------------------------------------------------------------
        let gpu_adapters = gpu::adapters();
        let (gpu_per_process, gpu_engines) = {
            let pdh = &mut self.pdh;
            let mut guard = pdh.lock();
            (guard.process_gpu_stats(), guard.engine_stats())
        };
        for e in processes.iter_mut() {
            if let Some(g) = gpu_per_process.as_ref().and_then(|m| m.get(&e.pid)) {
                e.gpu_util_pct = Some(g.util_pct);
                e.gpu_mem_bytes = Some(g.mem_bytes);
            }
        }
        let gpus = gpu::merge(gpu_adapters, gpu_engines);

        // ---- disks -----------------------------------------------------------------
        let disk_perf = {
            let pdh = &mut self.pdh;
            let mut guard = pdh.lock();
            guard.sample_disks()
        };
        let mut disks = Vec::new();
        for d in self.disks.list() {
            let mount = d.mount_point().to_string_lossy().to_string();
            let media = match d.kind() {
                sysinfo::DiskKind::SSD => {
                    if d.is_removable() {
                        MediaKind::Usb
                    } else {
                        MediaKind::Ssd
                    }
                }
                sysinfo::DiskKind::HDD => {
                    if d.is_removable() {
                        MediaKind::Usb
                    } else {
                        MediaKind::Hdd
                    }
                }
                _ => MediaKind::Unknown,
            };
            let id = disk_id_for_mount(&mount);
            let perf = disk_perf.iter().find(|x| x.matches_mount(&mount));
            disks.push(DiskInfo {
                id,
                mount: mount.clone(),
                label: String::new(),
                media,
                total_bytes: d.total_space(),
                free_bytes: d.available_space(),
                active_pct: perf.map(|x| x.active_pct).unwrap_or(0.0),
                read_bps: perf.map(|x| x.read_bps).unwrap_or(0.0),
                write_bps: perf.map(|x| x.write_bps).unwrap_or(0.0),
                avg_resp_ms: perf.map(|x| x.avg_resp_ms).unwrap_or(0.0),
                total_read_bytes: 0,
                total_written_bytes: 0,
            });
        }

        // ---- networks -----------------------------------------------------------------
        let mut nets = Vec::new();
        for (name, data) in &self.networks {
            let recv_total = data.total_received();
            let sent_total = data.total_transmitted();
            let (recv_bps, sent_bps) =
                match (self.prev_net_totals.get(name.as_str()).copied(), self.first_tick_done) {
                    (Some((pr, ps)), true) => (
                        recv_total.saturating_sub(pr) as f64 / interval_s,
                        sent_total.saturating_sub(ps) as f64 / interval_s,
                    ),
                    _ => (0.0, 0.0),
                };
            nets.push(NetworkInfo {
                name: name.to_string(),
                desc: String::new(),
                kind: classify_adapter(name),
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

        // ---- assemble --------------------------------------------------------------------
        let (handles_global, threads_global) = perfcounters::global_handle_thread_count();
        let snap = Snapshot {
            timestamp_ms: now_ms(),
            sample_duration_ms: started.elapsed().as_millis() as u64,
            cpu: CpuInfo {
                brand: cpus.first().map(|c| c.brand().to_string()).unwrap_or_default(),
                vendor: cpus.first().map(|c| c.vendor_id().to_string()).unwrap_or_default(),
                architecture: std::env::consts::ARCH.to_string(),
                utilization_pct: utilization.clamp(0.0, 100.0),
                per_core_pct: cpus.iter().map(|c| c.cpu_usage().clamp(0.0, 100.0)).collect(),
                freq_mhz: freq,
                freq_base_mhz: self.cpu_static.base_mhz,
                logical_count: logical,
                physical_cores: self.cpu_static.physical_cores,
                sockets: self.cpu_static.sockets,
                l1_kb: self.cpu_static.l1_kb_total,
                l2_kb: self.cpu_static.l2_kb_total,
                l3_kb: self.cpu_static.l3_kb_total,
                virtualization: self.cpu_static.virtualization.clone(),
            },
            memory: MemoryInfo {
                total_bytes: mem_total,
                used_bytes: mem_used,
                available_bytes: mem_avail,
                cached_bytes: win_mem.cached,
                commit_total_bytes: win_mem.commit_limit,
                commit_used_bytes: win_mem.commit_total,
                paged_pool_bytes: win_mem.paged_pool,
                non_paged_pool_bytes: win_mem.non_paged_pool,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
            },
            disks,
            networks: nets,
            gpus,
            processes,
            system: SystemMisc {
                hostname: hostname(),
                os_name: System::name().unwrap_or_else(|| "Windows".into()),
                os_version: System::long_os_version().unwrap_or_default(),
                kernel_version: System::kernel_version().unwrap_or_default(),
                uptime_s: System::uptime(),
                boot_epoch_s: System::boot_time() as i64,
                process_count: n_procs,
                thread_count: threads_global,
                handle_count: handles_global,
            },
        };
        self.first_tick_done = true;
        Ok(snap)
    }
}

fn build_user_map(users: &Users) -> HashMap<sysinfo::Uid, String> {
    let mut m = HashMap::new();
    for u in users.list() {
        m.insert(u.id().clone(), u.name().to_string());
    }
    m
}

fn map_status(s: sysinfo::ProcessStatus) -> ProcStatus {
    use sysinfo::ProcessStatus::*;
    match s {
        Stop | Zombie | Dead => ProcStatus::Suspended,
        _ => ProcStatus::Running,
    }
}

fn disk_id_for_mount(mount: &str) -> String {
    mount.trim_end_matches(['\\', '/']).to_uppercase()
}

fn classify_adapter(internal: &str) -> String {
    let lower = internal.to_ascii_lowercase();
    if lower.contains("loopback") || lower == "lo" {
        "Loopback".into()
    } else if lower.contains("wi-fi")
        || lower.contains("wifi")
        || lower.contains("wlan")
        || lower.contains("802.11")
    {
        "Wi-Fi".into()
    } else {
        "Ethernet".into()
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
