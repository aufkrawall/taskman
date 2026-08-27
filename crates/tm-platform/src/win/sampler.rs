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

use crate::win::cpu_load::{CpuLoadAccountant, LoadSample};
use crate::win::{
    cpu_info, gpu, memory_info, net_info, perfcounters, process_ops, threads_map, version,
    windows_enum,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{
    Disks, MemoryRefreshKind, Networks, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
    UpdateKind, Users,
};
use tm_core::classify;
use tm_core::demand::TelemetryDemand;
use tm_core::engine::SystemCollector;
use tm_core::error::Result;
use tm_core::model::*;

/// Refresh cadence for slow-changing per-process attributes. Time-based
/// (not tick-count based) so behavior stays identical at High/Normal/Low
/// update speeds; entries also invalidate when the process identity changes.
const ATTR_REFRESH_TTL: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone)]
struct PidAttrs {
    session_id: Option<u32>,
    wow64: Option<bool>,
    priority: PriorityClass,
    handles: Option<u32>,
    elevated: Option<bool>,
    uac_virtualization: Option<UacVirtualization>,
    power_throttled: Option<bool>,
    /// Full command line (Details page); immutable after process start.
    command_line: Option<String>,
    /// Process identity guard against PID reuse.
    start_epoch_s: Option<i64>,
    /// When these values were last queried natively.
    refreshed_at: Instant,
}

/// Everything that carries state between ticks.
pub struct Sampler {
    sys: System,
    disks: Disks,
    networks: Networks,
    user_names: HashMap<sysinfo::Uid, String>,
    cpu_static: Option<cpu_info::CpuStatic>,
    prev_net_totals: HashMap<String, (u64, u64)>,
    last_tick: Option<Instant>,
    first_tick_done: bool,
    initialized: bool,
    tick_no: u64,
    attrs: HashMap<u32, PidAttrs>,
    gpu_adapters: Option<Vec<gpu::AdapterInfo>>,
    /// Static RAM hardware facts (SMBIOS), probed once.
    ram_static: Option<memory_info::RamStatic>,
    pdh: Mutex<perfcounters::PdhCounters>,
    /// Current telemetry demand from the UI (drives expensive providers).
    demand: TelemetryDemand,
    /// Cached native network adapter metadata with a wall-clock TTL so the
    /// SSID/description walk does not run every sampling tick.
    net_meta_cache: Option<(Instant, HashMap<String, net_info::AdapterInfo>)>,
    /// Time-based CPU accountant (see [`cpu_load`]); replaces sysinfo/PDH
    /// CPU usage which was noisy and mis-normalized.
    cpu_load: CpuLoadAccountant,
    /// Last valid CPU-load sample; held when a tick's window is unusable.
    last_load: Option<Arc<LoadSample>>,
}

impl Sampler {
    /// Cheap construction: no blocking system queries here. All heavy state
    /// (user map, disk/network lists, first full refresh) is built lazily on
    /// the first `sample()` call, which runs on the engine thread.
    pub fn new() -> Self {
        let mut sys = System::new();
        // Lightweight warmup only. Even the static CPU/SMBIOS probes are
        // deferred: they parse firmware tables and must not run on whatever
        // thread constructed us (implement.md §6.1).
        sys.refresh_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram().with_swap()),
        );

        Self {
            user_names: HashMap::new(),
            sys,
            disks: Disks::new(),
            networks: Networks::new(),
            cpu_static: None,
            prev_net_totals: HashMap::new(),
            last_tick: None,
            first_tick_done: false,
            initialized: false,
            tick_no: 0,
            attrs: HashMap::new(),
            gpu_adapters: None,
            ram_static: None,
            pdh: Mutex::new(perfcounters::PdhCounters::new()),
            demand: TelemetryDemand::core(),
            net_meta_cache: None,
            cpu_load: CpuLoadAccountant::new(),
            last_load: None,
        }
    }

    /// One-time heavy initialization on the engine thread.
    fn lazy_init(&mut self) {
        if self.initialized {
            return;
        }
        // Static hardware facts: SMBIOS parsing + CPUID topology probing.
        self.cpu_static = Some(cpu_info::CpuStatic::probe());
        self.ram_static = Some(memory_info::probe());
        // `new_with_refreshed_list` already performs the first enumeration.
        let users = Users::new_with_refreshed_list();
        self.user_names = build_user_map(&users);
        self.disks = Disks::new_with_refreshed_list();
        self.networks = Networks::new_with_refreshed_list();
        self.initialized = true;
    }

    fn refresh_raw(&mut self) {
        // NOTE: no `.with_cpu_usage()` / `.with_cpu()` here — all CPU load
        // values come from `cpu_load` (NtQuerySystemInformation deltas).
        // Besides correctness this saves sysinfo's per-process
        // GetProcessTimes/GetSystemTimes storm every tick.
        self.sys.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(sysinfo::CpuRefreshKind::nothing().with_frequency())
                .with_memory(MemoryRefreshKind::nothing().with_ram().with_swap()),
        );
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
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
    fn attrs_for(&mut self, pid: u32, start_epoch_s: Option<i64>) -> PidAttrs {
        if let Some(a) = self.attrs.get(&pid)
            && a.refreshed_at.elapsed() < ATTR_REFRESH_TTL
            && a.start_epoch_s == start_epoch_s
        {
            return a.clone();
        }
        let security = if self.demand.wants(TelemetryDemand::TOKEN_SECURITY) {
            process_ops::token_security(pid)
        } else {
            process_ops::TokenSecurity {
                elevated: None,
                virtualization: None,
            }
        };
        let fresh = PidAttrs {
            session_id: process_ops::session_id_of(pid),
            wow64: process_ops::is_wow64(pid),
            priority: process_ops::priority_class_of(pid),
            handles: process_ops::handle_count(pid),
            elevated: security.elevated,
            uac_virtualization: security.virtualization,
            power_throttled: process_ops::efficiency_mode_state(pid),
            command_line: process_ops::command_line_of(pid),
            start_epoch_s,
            refreshed_at: Instant::now(),
        };
        self.attrs.insert(pid, fresh.clone());
        fresh
    }

    /// Demand update from the UI (cheap atomic handoff upstream).
    pub fn set_demand(&mut self, d: TelemetryDemand) {
        self.demand = d;
    }
}

impl SystemCollector for Sampler {
    fn backend_name(&self) -> &'static str {
        "windows/sysinfo+pdh+nt-cpu"
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

        // ---- CPU load first ---------------------------------------------------
        // Sampled before everything else so its delta window stays tight and
        // independent of how long the rest of the tick takes. Time-based and
        // therefore immune to boost clocks (see win/cpu_load.rs). When a
        // window is too short to be trustworthy we keep the previous sample.
        if let Some(s) = self.cpu_load.sample() {
            self.last_load = Some(s);
        }
        let load = self.last_load.clone();

        // ---- refresh raw data --------------------------------------------------
        self.refresh_raw();

        let window_owners: HashSet<u32> =
            windows_enum::visible_window_owners().into_iter().collect();
        let thread_counts = threads_map::thread_counts();

        // ---- CPU -----------------------------------------------------------------
        // Static identity/frequency from sysinfo; load numbers exclusively
        // from the time-based accountant above.
        let (cpu_brand, cpu_vendor, sysinfo_freq_mhz) = {
            let cpus = self.sys.cpus();
            (
                cpus.first()
                    .map(|c| c.brand().to_string())
                    .unwrap_or_default(),
                cpus.first()
                    .map(|c| c.vendor_id().to_string())
                    .unwrap_or_default(),
                cpus.iter().map(|c| c.frequency() as f32).sum::<f32>() / cpus.len().max(1) as f32,
            )
        };
        let logical = load
            .as_ref()
            .map(|l| l.per_core_pct.len())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| self.sys.cpus().len())
            .max(1);
        let (utilization, per_core_pct) = match load.as_ref() {
            Some(l) => (l.global_pct.clamp(0.0, 100.0), l.per_core_pct.clone()),
            None => (0.0, vec![0.0; logical]),
        };

        // ---- memory ----------------------------------------------------------------
        let mem_total = self.sys.total_memory();
        let mem_used = self.sys.used_memory();
        let mem_avail = self.sys.available_memory();
        let win_mem = perfcounters::query_windows_memory();

        // ---- PDH: demand-gated groups, one collection per tick -----------------
        // The UI's TelemetryDemand decides which expensive providers stay
        // warm; each group keeps its own query and two-sample warm-up.
        {
            let demand = self.demand;
            let guard = self.pdh.get_mut().unwrap_or_else(|e| e.into_inner());
            guard.tick(demand);
        }
        let (gpu_engine_records, gpu_mem_records, disk_perf, cpu_perf_pct, cpu_pdh_failed) = {
            let guard = self.pdh.get_mut().unwrap_or_else(|e| e.into_inner());
            (
                guard.read_gpu_engines().unwrap_or_default(),
                guard.read_gpu_memory().unwrap_or_default(),
                guard.read_disks(),
                guard.read_cpu_perf_pct(),
                guard.cpu_counter_failed(),
            )
        };

        // Current speed, Task-Manager style: base clock × average
        // "% Processor Performance" across all cores. sysinfo's frequency
        // (CallNtPowerInformation CurrentMhz) reports the fixed nominal clock
        // on modern Windows, so it is only a fallback when the counter is
        // unavailable on this system. While the counter warms up we report
        // 0 — the UI renders that as "—" instead of pretending the nominal
        // clock were the measured speed.
        let freq_base_mhz = self.cpu_static.as_ref().map_or(0.0, |c| c.base_mhz);
        let freq = match cpu_perf_pct {
            Some(pct) if freq_base_mhz > 0.0 => freq_base_mhz * pct / 100.0,
            _ if cpu_pdh_failed => sysinfo_freq_mhz,
            _ => 0.0,
        };

        // ---- processes ------------------------------------------------------------
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
            // Time-based share of total machine capacity + absolute CPU time,
            // both straight from the kernel's accumulators (cpu_load.rs).
            let pc = load.as_ref().and_then(|l| l.procs.get(&pid_u));
            entry.cpu_pct = pc.map_or(0.0, |c| c.pct);
            entry.mem_bytes = p.memory();
            entry.commit_bytes = Some(p.virtual_memory());
            entry.start_epoch_s = Some(p.start_time() as i64);
            entry.cpu_time_s = pc.map(|c| c.total_time_100ns as f64 / 10_000_000.0);
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
            let a = self.attrs_for(p.pid, p.start_epoch_s);
            p.session_id = a.session_id;
            p.priority = a.priority;
            p.handles = a.handles;
            p.wow64 = a.wow64;
            p.elevated = a.elevated;
            p.uac_virtualization = a.uac_virtualization;
            p.power_throttled = a.power_throttled;
            p.command_line = a.command_line;
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
        // DXGI enumeration is skipped entirely until GPU telemetry is first
        // demanded so a default Processes page cannot wake a dormant dGPU.
        if (!gpu_engine_records.is_empty() || !gpu_mem_records.is_empty() || self.demand.any_gpu())
            && self.gpu_adapters.is_none()
        {
            self.gpu_adapters = Some(gpu::adapters());
        }
        let gpus = match self.gpu_adapters.clone() {
            Some(adapters) => gpu::merge(adapters, &gpu_engine_records, &gpu_mem_records),
            None => Vec::new(),
        };

        // Per-process values: busiest-engine utilization plus the dominant
        // engine label ("GPU 0 - 3D") and dedicated/shared memory.
        if !gpu_engine_records.is_empty() || !gpu_mem_records.is_empty() {
            let per_pid: HashMap<u32, gpu::ProcessGpuView> =
                gpu::process_gpu_view(&gpu_engine_records, &gpu_mem_records)
                    .into_iter()
                    .map(|v| (v.pid, v))
                    .collect();
            for e in processes.iter_mut() {
                if let Some(g) = per_pid.get(&e.pid) {
                    e.gpu_util_pct = Some(g.util_pct);
                    e.gpu_dedicated_bytes = Some(g.dedicated_bytes);
                    e.gpu_shared_bytes = Some(g.shared_bytes);
                    e.gpu_mem_bytes = Some(g.dedicated_bytes);
                    e.gpu_engine_label = g.dominant_engine.clone();
                }
            }
        }

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
        // Byte-rate counters run on the sampling cadence; the native adapter
        // metadata walk (desc/link/SSID) is cached for NET_META_TTL so it does
        // not run every tick (implement.md §6.5).
        const NET_META_TTL: std::time::Duration = std::time::Duration::from_secs(5);
        let adapter_info = match &self.net_meta_cache {
            Some((at, map)) if at.elapsed() < NET_META_TTL => map.clone(),
            _ => {
                let fresh = net_info::adapters();
                self.net_meta_cache = Some((Instant::now(), fresh.clone()));
                fresh
            }
        };
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
            let ai = adapter_info.get(name.as_str());
            nets.push(NetworkInfo {
                name: name.to_string(),
                desc: ai.map_or_else(String::new, |a| a.desc.clone()),
                kind: classify_adapter(name),
                oper_up: ai.is_some_and(|a| a.oper_up),
                recv_bps,
                sent_bps,
                total_recv_bytes: recv_total,
                total_sent_bytes: sent_total,
                link_bps: ai.map_or(0, |a| a.link_bps),
                ssid: ai.and_then(|a| a.ssid.clone()),
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
                per_core_kernel_pct: load
                    .as_ref()
                    .map_or_else(Vec::new, |l| l.per_core_kernel_pct.clone()),
                kernel_pct: load.as_ref().map_or(0.0, |l| l.global_kernel_pct),
                freq_mhz: freq,
                freq_base_mhz: self.cpu_static.as_ref().map_or(0.0, |c| c.base_mhz),
                logical_count: logical,
                physical_cores: self.cpu_static.as_ref().map_or(0, |c| c.physical_cores),
                sockets: self.cpu_static.as_ref().map_or(0, |c| c.sockets),
                l1_kb: self.cpu_static.as_ref().map_or(0, |c| c.l1_kb_total),
                l2_kb: self.cpu_static.as_ref().map_or(0, |c| c.l2_kb_total),
                l3_kb: self.cpu_static.as_ref().map_or(0, |c| c.l3_kb_total),
                virtualization: self
                    .cpu_static
                    .as_ref()
                    .map_or_else(String::new, |c| c.virtualization.clone()),
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
                installed_bytes: self.ram_static.as_ref().map_or(0, |r| r.installed_bytes),
                hw_reserved_bytes: self
                    .ram_static
                    .as_ref()
                    .map_or(0, |r| r.installed_bytes)
                    .saturating_sub(mem_total),
                speed_mts: self.ram_static.as_ref().map_or(0, |r| r.speed_mts),
                speed_max_mts: self.ram_static.as_ref().map_or(0, |r| r.speed_max_mts),
                slots_used: self.ram_static.as_ref().map_or(0, |r| r.slots_used),
                slots_total: self.ram_static.as_ref().map_or(0, |r| r.slots_total),
                form_factor: self
                    .ram_static
                    .as_ref()
                    .map_or_else(String::new, |r| r.form_factor.clone()),
                manufacturer: self
                    .ram_static
                    .as_ref()
                    .map_or_else(String::new, |r| r.manufacturer.clone()),
                part_number: self
                    .ram_static
                    .as_ref()
                    .map_or_else(String::new, |r| r.part_number.clone()),
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
