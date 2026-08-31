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
    core_service, cpu_info, gpu, memory_info, net_etw, net_info, perfcounters, process_ops,
    threads_map, version, windows_enum,
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
use tm_core::i18n::{self, K};
use tm_core::model::*;

/// Refresh cadence for slow-changing per-process attributes. Time-based
/// (not tick-count based) so behavior stays identical at High/Normal/Low
/// update speeds; entries also invalidate when the process identity changes.
const ATTR_REFRESH_TTL: std::time::Duration = std::time::Duration::from_secs(10);
/// Efficiency mode (EcoQoS) is a live status column, not a slow-changing
/// attribute: browsers toggle it as tabs go background. Refresh it on its own
/// short TTL so the leaf is not up to [`ATTR_REFRESH_TTL`] stale.
const POWER_THROTTLE_REFRESH_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// CPU pseudo-rows ("System interrupts", "Terminated processes") only show
/// above this share of total capacity so an idle system gains no noise rows.
const PSEUDO_ROW_MIN_PCT: f32 = 0.5;
/// Ticks a pseudo row stays visible after it last exceeded the threshold, so
/// bursty churn between samples does not make the rows flicker on/off.
const PSEUDO_ROW_HOLD_TICKS: u32 = 5;
/// Sentinel pids for the CPU pseudo-rows. Real Windows pids are small
/// multiples of 4, so these can never collide; process actions must refuse
/// them (ProcessEntry::synthetic).
const PSEUDO_PID_INTERRUPTS: u32 = u32::MAX;
const PSEUDO_PID_TERMINATED: u32 = u32::MAX - 1;

/// Held display state of one CPU pseudo-row across ticks.
#[derive(Debug, Clone)]
struct HeldPseudoRow {
    pct: f32,
    /// Exited-image summary for the tooltip (empty for interrupts).
    detail: String,
    /// Number of exited processes (0 for interrupts).
    count: u32,
    ticks_left: u32,
}

/// Hold-decay slots for the two CPU pseudo-rows.
#[derive(Debug, Default, Clone)]
struct PseudoRowHold {
    interrupts: Option<HeldPseudoRow>,
    terminated: Option<HeldPseudoRow>,
}

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
    /// Efficiency mode flips whenever a browser backgrounds a tab, so it
    /// gets its own short TTL — a single cheap
    /// OpenProcess/GetProcessInformation pair, unlike the PEB and token
    /// reads that justify the long TTL for everything else.
    power_refreshed_at: Instant,
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
    /// Hold-decay state of the CPU pseudo-rows.
    pseudo: PseudoRowHold,
    /// Where per-process network counters come from this session.
    net_source: NetSource,
    /// Previous cumulative per-process byte counters, keyed by identity so a
    /// recycled PID cannot inherit a dead process's totals.
    prev_proc_net: HashMap<u32, ProcNetSample>,
}

/// Where per-process network counters come from.
///
/// Starting an ETW session needs administrator rights, which the GUI
/// deliberately does not have. The protected LocalSystem service hosts the
/// trace instead and answers a read-only query, so the ordinary unelevated GUI
/// still gets real numbers; running the trace under our own token is only the
/// fallback for when the service is absent and we happen to be elevated.
enum NetSource {
    /// Not probed yet (also the state after the UI stops asking).
    Undecided,
    /// The protected service answers.
    Broker,
    /// Our own token runs the trace.
    Local(net_etw::NetworkUsage),
    /// Neither works; keep reporting unknown, and re-probe occasionally so
    /// installing the service later starts working without an app restart.
    Unavailable { since: Instant },
}

/// How long to wait before re-probing after both sources failed.
const NET_SOURCE_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// One process's cumulative network counters at the previous tick.
#[derive(Debug, Clone, Copy)]
struct ProcNetSample {
    start_epoch_s: Option<i64>,
    bytes: net_etw::PidBytes,
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
            pseudo: PseudoRowHold::default(),
            net_source: NetSource::Undecided,
            prev_proc_net: HashMap::new(),
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
        if let Some(a) = self.attrs.get_mut(&pid)
            && a.refreshed_at.elapsed() < ATTR_REFRESH_TTL
            && a.start_epoch_s == start_epoch_s
        {
            if a.power_refreshed_at.elapsed() >= POWER_THROTTLE_REFRESH_TTL {
                a.power_throttled = process_ops::efficiency_mode_state(pid);
                a.power_refreshed_at = Instant::now();
            }
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
            power_refreshed_at: Instant::now(),
        };
        self.attrs.insert(pid, fresh.clone());
        fresh
    }

    /// Demand update from the UI (cheap atomic handoff upstream).
    pub fn set_demand(&mut self, d: TelemetryDemand) {
        self.demand = d;
    }

    /// Cumulative per-process counters for this tick, or `None` when the
    /// answer is genuinely unknown.
    ///
    /// Never returns an empty map to mean "no traffic": an unknown answer and
    /// a measured zero are different, and only the caller's `None` branch may
    /// leave the snapshot fields unset.
    fn net_totals(&mut self, live: &HashSet<u32>) -> Option<HashMap<u32, net_etw::PidBytes>> {
        if !self.demand.wants(TelemetryDemand::PROCESS_NET) {
            // Dropping a local session here stops it and joins its consumer.
            self.net_source = NetSource::Undecided;
            self.prev_proc_net.clear();
            return None;
        }
        if let NetSource::Unavailable { since } = &self.net_source {
            if since.elapsed() < NET_SOURCE_RETRY {
                return None;
            }
            self.net_source = NetSource::Undecided;
        }
        if matches!(self.net_source, NetSource::Undecided) {
            let (source, first) = probe_net_source();
            self.net_source = source;
            if first.is_some() {
                return first;
            }
        }
        match &self.net_source {
            NetSource::Broker => match core_service::brokered_process_network() {
                core_service::BrokeredNetwork::Sample(sample) if sample.active => {
                    Some(sample_to_map(&sample))
                }
                // The service is there but its own trace failed: unknown.
                core_service::BrokeredNetwork::Sample(_) => None,
                other => {
                    if let core_service::BrokeredNetwork::Rejected(detail) = &other {
                        tracing::warn!(%detail, "broker refused network counters");
                    }
                    self.net_source = NetSource::Unavailable {
                        since: Instant::now(),
                    };
                    None
                }
            },
            NetSource::Local(usage) => Some(usage.totals_pruned(live)),
            _ => None,
        }
    }

    /// Fill per-process network rates from the ETW totals.
    ///
    /// Every process gets a value while the trace runs — an idle process
    /// really did move zero bytes — and every process keeps `None` while it
    /// does not, because "unknown" and "zero" are different answers.
    fn apply_process_network(&mut self, processes: &mut [ProcessEntry], interval_s: f64) {
        let live: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
        let Some(totals) = self.net_totals(&live) else {
            return;
        };
        let mut next = HashMap::with_capacity(processes.len());
        for process in processes.iter_mut() {
            if process.synthetic {
                continue;
            }
            let bytes = totals.get(&process.pid).copied().unwrap_or_default();
            let previous = self
                .prev_proc_net
                .get(&process.pid)
                .filter(|prev| prev.start_epoch_s == process.start_epoch_s);
            let rate = |now: u64, before: u64| {
                // saturating_sub also covers the first tick after a pruned
                // PID reappears, where the counter restarts at zero.
                now.saturating_sub(before) as f64 / interval_s
            };
            let (recv_bps, sent_bps) = match previous {
                Some(prev) => (
                    rate(bytes.received, prev.bytes.received),
                    rate(bytes.sent, prev.bytes.sent),
                ),
                // First observation of this process: totals are known, but a
                // rate needs two points.
                None => (0.0, 0.0),
            };
            process.net_recv_total = Some(bytes.received);
            process.net_sent_total = Some(bytes.sent);
            process.net_recv_bps = Some(recv_bps);
            process.net_sent_bps = Some(sent_bps);
            next.insert(
                process.pid,
                ProcNetSample {
                    start_epoch_s: process.start_epoch_s,
                    bytes,
                },
            );
        }
        self.prev_proc_net = next;
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

        let window_owners = windows_enum::window_owners();
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
        // Unknown interrupt time must never be treated as zero; the pseudo-
        // row split below handles `None` by skipping the subtraction.
        let interrupt_pct = {
            let guard = self.pdh.get_mut().unwrap_or_else(|e| e.into_inner());
            guard.read_interrupt_pct()
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
            let has_window = window_owners.visible.contains(&pid_u);

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
            if self
                .cpu_load
                .is_suspended(pid_u, Some(p.start_time() as i64))
            {
                entry.status = ProcStatus::Suspended;
            } else if window_owners.not_responding.contains(&pid_u) {
                entry.status = ProcStatus::NotResponding;
            }
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

        // ---- per-process network (ETW) --------------------------------------------
        // Runs after the process list is final so pruning sees exactly the
        // live PIDs, and after categories so synthetic rows can be skipped.
        self.apply_process_network(&mut processes, interval_s);

        // ---- CPU attribution pseudo-rows ------------------------------------------
        // The per-core accumulators see ALL busy time; live processes cannot
        // be charged with interrupt/DPC servicing or with work of processes
        // that terminated during the window (compiler churn). Surface both
        // as synthetic rows so sorted-by-CPU pages never show load that no
        // row owns. Appended AFTER grouping so the classifier never touches
        // them; they carry fixed categories.
        let (interrupt_row, terminated_row) =
            self.update_pseudo_rows(load.as_deref(), interrupt_pct);
        append_pseudo_rows(
            &mut processes,
            interrupt_row.as_ref(),
            terminated_row.as_ref(),
        );

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
                ipv4: ai.and_then(|a| a.ipv4.clone()),
                ipv6: ai.and_then(|a| a.ipv6.clone()),
                signal_quality_pct: ai.and_then(|a| a.signal_quality_pct),
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

    /// Refresh the hold-decay slots of the CPU pseudo-rows and return the
    /// rows to display this tick.
    ///
    /// A measured interrupt value is authoritative (low → row hides
    /// immediately); only an UNKNOWN measurement (`None`, e.g. counter
    /// warming up or unavailable) keeps the held value decaying. The
    /// terminated-processes share is the unattributed CPU residual minus
    /// the measured interrupt share, so the same busy time is never shown
    /// twice; when interrupts are unknown the residual is shown whole
    /// rather than being silently dropped.
    fn update_pseudo_rows(
        &mut self,
        load: Option<&LoadSample>,
        interrupt_meas: Option<f32>,
    ) -> (Option<HeldPseudoRow>, Option<HeldPseudoRow>) {
        match interrupt_meas {
            Some(v) if v >= PSEUDO_ROW_MIN_PCT => {
                self.pseudo.interrupts = Some(HeldPseudoRow {
                    pct: v,
                    detail: String::new(),
                    count: 0,
                    ticks_left: PSEUDO_ROW_HOLD_TICKS,
                });
            }
            Some(_) => self.pseudo.interrupts = None,
            None => decay_pseudo(&mut self.pseudo.interrupts),
        }

        match load {
            Some(l) => {
                let terminated = (l.unattributed_pct - interrupt_meas.unwrap_or(0.0)).max(0.0);
                if terminated >= PSEUDO_ROW_MIN_PCT {
                    let detail = l
                        .exited_images
                        .iter()
                        .map(|e| format!("{} ×{}", e.name, e.count))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.pseudo.terminated = Some(HeldPseudoRow {
                        pct: terminated,
                        detail,
                        count: l.exited_count,
                        ticks_left: PSEUDO_ROW_HOLD_TICKS,
                    });
                } else {
                    decay_pseudo(&mut self.pseudo.terminated);
                }
            }
            None => decay_pseudo(&mut self.pseudo.terminated),
        }

        (
            self.pseudo.interrupts.clone(),
            self.pseudo.terminated.clone(),
        )
    }
}

/// Probe both sources once, returning the chosen one plus the sample it
/// already produced (so the probe does not cost an extra round trip).
fn probe_net_source() -> (NetSource, Option<HashMap<u32, net_etw::PidBytes>>) {
    match core_service::brokered_process_network() {
        core_service::BrokeredNetwork::Sample(sample) if sample.active => {
            (NetSource::Broker, Some(sample_to_map(&sample)))
        }
        // An explicit refusal is a policy decision; do not route around it.
        core_service::BrokeredNetwork::Rejected(detail) => {
            tracing::warn!(%detail, "broker refused network counters");
            (
                NetSource::Unavailable {
                    since: Instant::now(),
                },
                None,
            )
        }
        // No service (or one too old to know the request), and no trace of its
        // own: fall back to our token, which only works when elevated.
        _ => match net_etw::NetworkUsage::start(net_etw::TraceRole::App) {
            Some(usage) => (NetSource::Local(usage), None),
            None => (
                NetSource::Unavailable {
                    since: Instant::now(),
                },
                None,
            ),
        },
    }
}

fn sample_to_map(sample: &core_service::ProcessNetworkSample) -> HashMap<u32, net_etw::PidBytes> {
    sample
        .entries
        .iter()
        .map(|entry| {
            (
                entry.pid,
                net_etw::PidBytes {
                    received: entry.received,
                    sent: entry.sent,
                },
            )
        })
        .collect()
}

fn decay_pseudo(slot: &mut Option<HeldPseudoRow>) {
    if let Some(h) = slot {
        h.ticks_left -= 1;
        if h.ticks_left == 0 {
            *slot = None;
        }
    }
}

/// Append the CPU pseudo-rows as synthetic process entries. "System
/// interrupts" lives in the Windows-processes group (TM parity), the
/// terminated processes in Background — both sort and heat-map like any
/// other row, so high unattributable CPU is never invisible.
fn append_pseudo_rows(
    processes: &mut Vec<ProcessEntry>,
    interrupts: Option<&HeldPseudoRow>,
    terminated: Option<&HeldPseudoRow>,
) {
    if let Some(h) = interrupts {
        let mut e = ProcessEntry::new(PSEUDO_PID_INTERRUPTS, "System Interrupts");
        e.display = i18n::tr(K::SystemInterrupts).to_string();
        e.category = ProcCategory::System;
        e.cpu_pct = h.pct.clamp(0.0, 100.0);
        e.synthetic = true;
        e.description = Some(h.detail.clone());
        processes.push(e);
    }
    if let Some(h) = terminated {
        let mut e = ProcessEntry::new(PSEUDO_PID_TERMINATED, "Terminated Processes");
        // A residual can exist without observed exits (born-and-dead-inside-
        // one-window churn is never sampled alive, plus accounting tail) —
        // showing "(0)" would read as a bug, so the count is only shown
        // when there is one.
        e.display = if h.count > 0 {
            i18n::trf(K::TerminatedProcesses, &[&h.count.to_string()])
        } else {
            i18n::tr(K::TerminatedProcessesPlain).to_string()
        };
        e.category = ProcCategory::Background;
        e.cpu_pct = h.pct.clamp(0.0, 100.0);
        e.synthetic = true;
        e.description = Some(h.detail.clone());
        processes.push(e);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::cpu_load::ExitedImage;

    fn sample(unattributed_pct: f32, exited: u32) -> LoadSample {
        LoadSample {
            global_pct: 0.0,
            per_core_pct: Vec::new(),
            per_core_kernel_pct: Vec::new(),
            global_kernel_pct: 0.0,
            procs: HashMap::new(),
            unattributed_pct,
            exited_count: exited,
            exited_images: if exited > 0 {
                vec![ExitedImage {
                    name: "rustc.exe".into(),
                    count: exited,
                }]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn interrupt_measurement_is_authoritative_and_residual_never_double_counts() {
        let mut s = Sampler::new();

        // Measured high → row appears.
        let (irq, _) = s.update_pseudo_rows(Some(&sample(0.0, 0)), Some(7.0));
        assert_eq!(irq.unwrap().pct, 7.0);

        // Measured low → row hides immediately, no stale hold.
        let (irq, _) = s.update_pseudo_rows(None, Some(0.1));
        assert!(irq.is_none());

        // Unattributed residual is split against the measured interrupts:
        // 12 % residual − 4 % interrupts = 8 % terminated.
        let (_, term) = s.update_pseudo_rows(Some(&sample(12.0, 3)), Some(4.0));
        let t = term.expect("terminated row shown");
        assert!((t.pct - 8.0).abs() < 1e-4, "got {}", t.pct);
        assert_eq!(t.count, 3);
        assert!(t.detail.contains("rustc.exe"));

        // Unknown interrupt measurement must never be read as zero: the full
        // residual stays visible instead of being silently dropped.
        let (_, term) = s.update_pseudo_rows(Some(&sample(12.0, 3)), None);
        assert_eq!(term.unwrap().pct, 12.0);
    }

    #[test]
    fn pseudo_rows_hold_between_ticks_then_expire() {
        let mut s = Sampler::new();
        let (irq, term) = s.update_pseudo_rows(Some(&sample(9.0, 1)), None);
        assert!(irq.is_none(), "below threshold");
        assert_eq!(term.as_ref().unwrap().pct, 9.0);

        // Quiet ticks keep the rows visible while the hold lasts…
        for _ in 1..PSEUDO_ROW_HOLD_TICKS {
            let (_, term) = s.update_pseudo_rows(Some(&sample(0.0, 0)), None);
            assert!(term.is_some(), "row must not flicker off");
        }
        // …and disappear once the hold expires.
        for _ in 0..2 {
            let (_, term) = s.update_pseudo_rows(Some(&sample(0.0, 0)), None);
            if term.is_none() {
                return;
            }
        }
        panic!("terminated row never expired after hold");
    }

    #[test]
    fn appended_pseudo_rows_are_marked_and_categorized() {
        let irq = HeldPseudoRow {
            pct: 2.0,
            detail: String::new(),
            count: 0,
            ticks_left: 1,
        };
        let term = HeldPseudoRow {
            pct: 40.0,
            detail: "rustc.exe ×5".into(),
            count: 5,
            ticks_left: 1,
        };
        let mut processes: Vec<ProcessEntry> = Vec::new();
        append_pseudo_rows(&mut processes, Some(&irq), Some(&term));
        assert_eq!(processes.len(), 2);
        assert!(processes.iter().all(|p| p.synthetic));
        let by_pid = |pid: u32| processes.iter().find(|p| p.pid == pid).unwrap();
        let i = by_pid(PSEUDO_PID_INTERRUPTS);
        assert_eq!(i.category, ProcCategory::System);
        assert_eq!(i.cpu_pct, 2.0);
        let t = by_pid(PSEUDO_PID_TERMINATED);
        assert_eq!(t.category, ProcCategory::Background);
        assert_eq!(t.cpu_pct, 40.0);
        assert!(t.display.contains('5'), "count in label: {}", t.display);
        assert_eq!(t.description.as_deref(), Some("rustc.exe ×5"));
    }
}
