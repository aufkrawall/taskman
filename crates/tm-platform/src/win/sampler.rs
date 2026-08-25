//! sysinfo-driven sampler producing full `Snapshot`s on Windows.
//!
//! Performance notes:
//! * Construction is intentionally cheap — the expensive first refresh runs
//!   on the engine thread so process startup is not blocked behind it.
//! * PDH counters are collected exactly once per tick and read multiple times.
//! * DXGI adapter info is probed once (it never changes at runtime).
//! * Slow-changing per-process attributes (session id, WOW64, priority,
//!   handle count) are cached across ticks instead of issuing 4 native
//!   queries per process per tick.

use crate::win::{cpu_info, gpu, perfcounters, process_ops, threads_map, version, windows_enum};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{
    Disks, MemoryRefreshKind, Networks, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
    UpdateKind, Users,
};
use tm_core::classify;
use tm_core::engine::SystemCollector;
use tm_core::error::Result;
use tm_core::model::*;

/// Refresh cadence (in ticks) for slow-changing per-process attributes.
const ATTR_REFRESH_TICKS: u64 = 10;

#[derive(Clone)]
struct PidAttrs {
    session_id: Option<u32>,
    wow64: Option<bool>,
    priority: PriorityClass,
    handles: Option<u32>,
    /// Tick number when these values were last queried natively.
    refreshed_at_tick: u64,
}

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
    initialized: bool,
    tick_no: u64,
    attrs: HashMap<u32, PidAttrs>,
    gpu_adapters: Option<Vec<gpu::AdapterInfo>>,
    pdh: Mutex<perfcounters::Pdh>,
}

impl Sampler {
    /// Cheap construction: no blocking system queries here. All heavy state
    /// (user map, disk/network lists, first full refresh) is built lazily on
    /// the first `sample()` call, which runs on the engine thread.
    pub fn new() -> Self {
        let mut sys = System::new();
        // Lightweight warmup only: CPU/memory are quick and make the very
        // first rates valid sooner. Process enumeration happens in `sample`.
        sys.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(
                    sysinfo::CpuRefreshKind::nothing()
                        .with_cpu_usage()
                        .with_frequency(),
                )
                .with_memory(MemoryRefreshKind::nothing().with_ram().with_swap()),
        );

        Self {
            user_names: HashMap::new(),
            sys,
            disks: Disks::new(),
            networks: Networks::new(),
            pdh: Mutex::new(perfcounters::Pdh::new()),
            cpu_static: cpu_info::CpuStatic::probe(),
            prev_net_totals: HashMap::new(),
            last_tick: None,
            first_tick_done: false,
            initialized: false,
            tick_no: 0,
            attrs: HashMap::new(),
            gpu_adapters: None,
        }
    }

    /// One-time heavy initialization on the engine thread.
    fn lazy_init(&mut self) {
        if self.initialized {
            return;
        }
        // `new_with_refreshed_list` already performs the first enumeration.
        let users = Users::new_with_refreshed_list();
        self.user_names = build_user_map(&users);
        self.disks = Disks::new_with_refreshed_list();
        self.networks = Networks::new_with_refreshed_list();
        self.initialized = true;
    }

    fn refresh_raw(&mut self) {
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
    }

    /// Cached slow-changing attributes for one pid; refreshes them natively
    /// at most every [`ATTR_REFRESH_TICKS`] ticks (4 native calls instead of
    /// ~4 × processes per tick).
    fn attrs_for(&mut self, pid: u32) -> PidAttrs {
        let now_tick = self.tick_no;
        if let Some(a) = self.attrs.get(&pid)
            && now_tick - a.refreshed_at_tick < ATTR_REFRESH_TICKS
        {
            return a.clone();
        }
        let fresh = PidAttrs {
            session_id: process_ops::session_id_of(pid),
            wow64: process_ops::is_wow64(pid),
            priority: process_ops::priority_class_of(pid),
            handles: process_ops::handle_count(pid),
            refreshed_at_tick: now_tick,
        };
        self.attrs.insert(pid, fresh.clone());
        fresh
    }
}

impl SystemCollector for Sampler {
    fn backend_name(&self) -> &'static str {
        "windows/sysinfo+pdh"
    }

    fn sample(&mut self, started: Instant) -> Result<Snapshot> {
        self.lazy_init();
        self.tick_no += 1;
        self.sample_inner(started)
    }
}

impl Sampler {
    fn sample_inner(&mut self, started: Instant) -> Result<Snapshot> {
        let interval_s = self
            .last_tick
            .map_or(1.0, |t| started.duration_since(t).as_secs_f64())
            .clamp(0.05, 3600.0);
        self.last_tick = Some(started);

        // ---- refresh raw data --------------------------------------------------
        self.refresh_raw();

        let window_owners: HashSet<u32> =
            windows_enum::visible_window_owners().into_iter().collect();
        let thread_counts = threads_map::thread_counts();

        // ---- CPU -----------------------------------------------------------------
        // Extract everything needed from `sys.cpus()` up front so the
        // immutable sysinfo borrow ends before the mutable passes below.
        let (cpu_brand, cpu_vendor, per_core_pct, utilization, freq) = {
            let cpus = self.sys.cpus();
            let logical = cpus.len().max(1);
            (
                cpus.first()
                    .map(|c| c.brand().to_string())
                    .unwrap_or_default(),
                cpus.first()
                    .map(|c| c.vendor_id().to_string())
                    .unwrap_or_default(),
                cpus.iter()
                    .map(|c| c.cpu_usage().clamp(0.0, 100.0))
                    .collect::<Vec<f32>>(),
                cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / logical as f32,
                cpus.iter().map(|c| c.frequency() as f32).sum::<f32>() / logical as f32,
            )
        };

        // ---- memory ----------------------------------------------------------------
        let mem_total = self.sys.total_memory();
        let mem_used = self.sys.used_memory();
        let mem_avail = self.sys.available_memory();
        let win_mem = perfcounters::query_windows_memory();

        // ---- PDH: collect exactly once, read three times -----------------------
        let collected = {
            let pdh = &mut self.pdh;
            let guard = pdh.get_mut().unwrap_or_else(|e| e.into_inner());
            guard.collect_once()
        };
        let (gpu_per_process, gpu_engines, disk_perf) = {
            let pdh = &self.pdh;
            let mut guard = pdh.lock().unwrap_or_else(|e| e.into_inner());
            (
                if collected {
                    guard.read_gpu_process_stats()
                } else {
                    None
                },
                if collected {
                    guard.read_engine_stats()
                } else {
                    Vec::new()
                },
                if collected {
                    guard.read_disks()
                } else {
                    Vec::new()
                },
            )
        };

        // ---- processes ------------------------------------------------------------
        let logical = per_core_pct.len().max(1);
        let nb_cpus = logical as f32;
        let n_procs = self.sys.processes().len();

        // Purge attribute-cache entries of processes that no longer exist.
        if self.tick_no.is_multiple_of(16) {
            let alive: HashSet<u32> = self.sys.processes().keys().map(|p| p.as_u32()).collect();
            self.attrs.retain(|pid, _| alive.contains(pid));
        }

        let mut processes: Vec<ProcessEntry> = Vec::with_capacity(n_procs);

        for (pid, p) in self.sys.processes() {
            let pid_u = pid.as_u32();
            // Single owned copy of the name per process (reused everywhere).
            let name = p.name().to_string_lossy().into_owned();
            let exe_owned = p.exe().map(|e| e.to_path_buf());
            let has_window = window_owners.contains(&pid_u);

            let user = p
                .user_id()
                .and_then(|uid| self.user_names.get(uid).cloned());
            let du = p.disk_usage();

            let mut entry = ProcessEntry::new(pid_u, name.clone());
            // Friendly name (FileDescription) like TM: "Windows-Explorer".
            if let Some(exe) = exe_owned.as_ref() {
                let exe_str = exe.to_string_lossy();
                let ver = version::query(&exe_str);
                if !ver[0].is_empty() {
                    entry.display = ver[0].clone();
                }
                if !ver[1].is_empty() {
                    entry.company = Some(ver[1].clone());
                }
            }
            entry.ppid = p.parent().map(|x| x.as_u32());
            entry.status = map_status(p.status());
            entry.user = user;
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
            entry.exe_path = exe_owned;
            entry.threads = thread_counts.get(&pid_u).copied();
            processes.push(entry);
        }

        // ---- slow-changing native attributes (cached, TTL-based) --------------
        // Applied in a separate pass so the &mut self cache never overlaps
        // the immutable sysinfo iteration above.
        for p in processes.iter_mut() {
            let a = self.attrs_for(p.pid);
            p.session_id = a.session_id;
            p.priority = a.priority;
            p.handles = a.handles;
            p.wow64 = a.wow64;
            // System processes have no owning user; TM shows "SYSTEM".
            if p.user.is_none() && a.session_id.is_some_and(|sid| sid == 0) {
                p.user = Some("SYSTEM".to_string());
            }
        }

        // ---- classification refinement + App grouping (TM semantics) -------------
        // Every process with a visible window is an app root; windowless
        // ancestors below system boundaries fold in ("Steam (2)" absorbs
        // steamwebhelper's window; Terminal absorbs its shell children).
        // The ancestor walk stops at windowed processes and at system
        // processes (svchost/services/...), matching Task Manager.
        refine_categories_and_group_apps(&mut processes);

        // ---- GPU ---------------------------------------------------------------
        // Static adapter info is probed once; it does not change at runtime.
        if self.gpu_adapters.is_none() {
            self.gpu_adapters = Some(gpu::adapters());
        }
        let gpu_adapters = self.gpu_adapters.as_deref().unwrap_or(&[]).to_vec();

        for e in processes.iter_mut() {
            if let Some(g) = gpu_per_process.as_ref().and_then(|m| m.get(&e.pid)) {
                e.gpu_util_pct = Some(g.util_pct);
                e.gpu_mem_bytes = Some(g.mem_bytes);
            }
        }
        let gpus = gpu::merge(gpu_adapters, gpu_engines);

        // ---- disks -----------------------------------------------------------------
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
            let perf = disk_perf.iter().find(|x| x.matches_mount(&mount));
            let id = match perf {
                Some(x) => physical_disk_id(&x.instance, &mount),
                None => disk_id_for_mount(&mount),
            };
            disks.push(DiskInfo {
                id,
                mount: mount.clone(),
                label: String::new(),
                media,
                total_bytes: d.total_space(),
                free_bytes: d.available_space(),
                active_pct: perf.map_or(0.0, |x| x.active_pct),
                read_bps: perf.map_or(0.0, |x| x.read_bps),
                write_bps: perf.map_or(0.0, |x| x.write_bps),
                avg_resp_ms: perf.map_or(0.0, |x| x.avg_resp_ms),
                total_read_bytes: 0,
                total_written_bytes: 0,
            });
        }

        // ---- networks -----------------------------------------------------------------
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
        self.prev_net_totals.clear();
        for n in &nets {
            self.prev_net_totals
                .insert(n.name.clone(), (n.total_recv_bytes, n.total_sent_bytes));
        }

        // ---- assemble --------------------------------------------------------------------
        let (handles_global, threads_global) = perfcounters::global_handle_thread_count();
        let snap = Snapshot {
            timestamp_ms: now_ms(),
            sample_duration_ms: started.elapsed().as_millis() as u64,
            cpu: CpuInfo {
                brand: cpu_brand,
                vendor: cpu_vendor,
                architecture: std::env::consts::ARCH.to_string(),
                utilization_pct: utilization.clamp(0.0, 100.0),
                per_core_pct,
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

/// Walk ancestors for each process (bounded hops, zero extra allocations —
/// names are borrowed from the vec itself), then run the classifier with the
/// real ancestor chain and fold app subtrees together.
fn refine_categories_and_group_apps(processes: &mut [ProcessEntry]) {
    // pid -> index for O(1) parent lookups.
    let idx_by_pid: HashMap<u32, usize> = processes
        .iter()
        .enumerate()
        .map(|(i, p)| (p.pid, i))
        .collect();

    // --- refined classification with real ancestors -------------------------
    for i in 0..processes.len() {
        let mut anc: Vec<&str> = Vec::new();
        let mut cur_pid = processes[i].ppid;
        let mut hops = 0usize;
        while let Some(ppid) = cur_pid {
            hops += 1;
            if hops > 8 || ppid == processes[i].pid {
                break;
            }
            match idx_by_pid.get(&ppid) {
                Some(&j) => {
                    anc.push(processes[j].name.as_str());
                    cur_pid = processes[j].ppid;
                }
                None => break,
            }
        }
        let name = processes[i].name.clone();
        let input = classify::ClassifyInput {
            pid: processes[i].pid,
            name: &name,
            ancestor_names: &anc,
            has_window: processes[i].has_window,
            system_session: processes[i].session_id.is_some_and(|s| s == 0),
        };
        let cat = classify::classify(input);
        processes[i].category = cat;
    }

    // --- app-root grouping -----------------------------------------------
    let children: HashMap<usize, Vec<usize>> = {
        let mut m: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, p) in processes.iter().enumerate() {
            if let Some(ppid) = p.ppid
                && ppid != p.pid
                && let Some(&pi) = idx_by_pid.get(&ppid)
            {
                m.entry(pi).or_default().push(i);
            }
        }
        m
    };
    let is_boundary = |name: &str| -> bool {
        const SYSTEM: [&str; 12] = [
            "svchost.exe",
            "services.exe",
            "csrss.exe",
            "smss.exe",
            "wininit.exe",
            "winlogon.exe",
            "lsass.exe",
            "lsaiso.exe",
            "dwm.exe",
            "fontdrvhost.exe",
            "system",
            "registry",
        ];
        SYSTEM.iter().any(|s| name.eq_ignore_ascii_case(s))
    };
    // App roots: windowed processes walked up to the topmost
    // non-windowed, non-system ancestor.
    let mut roots: Vec<usize> = Vec::new();
    for (i, p) in processes.iter().enumerate() {
        if !p.has_window {
            continue;
        }
        let mut cur = i;
        loop {
            let next = processes[cur]
                .ppid
                .and_then(|pp| idx_by_pid.get(&pp).copied());
            match next {
                Some(pi)
                    if pi != cur
                        && !processes[pi].has_window
                        && !is_boundary(&processes[pi].name) =>
                {
                    cur = pi;
                }
                _ => break,
            }
        }
        if !roots.contains(&cur) {
            roots.push(cur);
        }
    }
    // Propagate App from roots down through descendants. Windowed
    // children are roots themselves and are not descended into.
    let root_set: HashSet<usize> = roots.iter().copied().collect();
    for &r in &root_set {
        processes[r].app_root = true;
    }
    let mut stack: Vec<usize> = roots;
    let mut seen: Vec<usize> = stack.clone();
    while let Some(i) = stack.pop() {
        processes[i].category = ProcCategory::App;
        if let Some(kids) = children.get(&i) {
            for &k in kids {
                if !root_set.contains(&k) && !seen.contains(&k) {
                    seen.push(k);
                    stack.push(k);
                }
            }
        }
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

/// TM-style disk id "0 (C:)" from the PDH instance "0 C:".
fn physical_disk_id(instance: &str, mount: &str) -> String {
    let num = instance.split_whitespace().next().unwrap_or("0");
    let letter = mount
        .trim_end_matches([char::from_u32(92).unwrap(), '/'])
        .to_uppercase();
    format!("{num} ({letter})")
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
        .map_or(0, |d| d.as_millis() as u64)
}
