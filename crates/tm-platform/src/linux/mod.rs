//! Linux backend: sysinfo + procfs/sysfs + systemd desktop roles + XDG
//! autostart + X11/EWMH window evidence.

pub mod activation;
mod autostart;
pub mod core_service;
mod cpu_stat;
mod desktop_session;
mod diskstats;
mod drm;
mod launch;
mod net_meta;
pub mod polkit;
mod process_connections;
mod process_cpu;
mod process_io;
mod process_modules;
mod process_threads;
mod process_tree;
mod sensors;
pub(crate) mod services;
mod sessions;
mod shell;
mod startup;
mod x11_windows;

use self::core_service::client::{self, ClientError};
use crate::actions::*;
use crate::linux_action_support::PrivilegedAction;
use crate::linux_core_bridge::{
    client_error, connections_from_wire, core_service_state, file_io_from_wire, module_from_wire,
    response_label, service_verb, threads_from_wire,
};
use crate::linux_core_service::{ActionRequest, MANIFEST_PATH, Request, Response};
use crate::linux_telemetry;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{
    Disks, MemoryRefreshKind, Networks, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
    UpdateKind,
};
use tm_core::classify;
use tm_core::demand::TelemetryDemand;
use tm_core::engine::SystemCollector;
use tm_core::error::Result;
use tm_core::model::*;

pub struct LinuxCollector {
    sys: System,
    disks: Disks,
    networks: Networks,
    users: sysinfo::Users,
    cpu_stats: cpu_stat::CpuStatSampler,
    process_cpu: process_cpu::ProcessCpuSampler,
    prev_net_totals: HashMap<String, (u64, u64)>,
    last_tick: Option<Instant>,
    first_tick_done: bool,
    /// Which optional providers the UI currently needs. Cheap collectors stay
    /// on at every demand, exactly like the Windows sampler's core set.
    demand: TelemetryDemand,
    /// Command lines keyed by pid, with the start ticks they were read for.
    cmdlines: HashMap<u32, (i64, Option<String>)>,
    /// L1/L2/L3 totals in KB; cache geometry is static, so the sysfs walk
    /// over every CPU runs once instead of every tick.
    cache_kb: [u64; 3],
    window_observer: x11_windows::WindowObserver,
    desktop_evidence: desktop_session::DesktopEvidenceCache,
}

impl SystemCollector for LinuxCollector {
    fn backend_name(&self) -> &'static str {
        "linux/sysinfo+procfs+sysfs+systemd"
    }

    fn set_demand(&mut self, demand: TelemetryDemand) {
        self.demand = demand;
    }

    fn sample(&mut self, started: Instant) -> Result<Snapshot> {
        let interval_s = self
            .last_tick
            .map(|t| started.duration_since(t).as_secs_f64())
            .unwrap_or(1.0)
            .clamp(0.05, 3600.0);
        self.last_tick = Some(started);
        let providers = linux_telemetry::providers(self.demand);

        // The per-CPU current-frequency probe is the slow half of this refresh
        // (a `scaling_cur_freq` read per CPU, or the whole `/proc/cpuinfo` per
        // CPU when cpufreq is absent), and only the Performance page's CPU card
        // shows it, so it follows `CPU_SPEED`.
        let memory = MemoryRefreshKind::nothing().with_ram().with_swap();
        let mut refresh = RefreshKind::nothing().with_memory(memory);
        if providers.cpu_speed {
            refresh = refresh.with_cpu(sysinfo::CpuRefreshKind::nothing().with_frequency());
        }
        self.sys.refresh_specifics(refresh);
        // Bracket sysinfo's observation. An identity read only after the
        // refresh could belong to a later occupant of the same PID.
        let before_refresh: HashMap<_, _> = self
            .sys
            .processes()
            .values()
            .filter(|p| p.thread_kind().is_none())
            .filter_map(|p| {
                let pid = p.pid().as_u32();
                linux_proc_start_ticks(pid).map(|ticks| (pid, ticks))
            })
            .collect();
        // No `.with_cpu()`: every CPU number comes from `/proc` above
        // (`cpu_stat` for the machine, `process_cpu` for each process). Asking
        // sysinfo for CPU as well would keep a second, differently-windowed
        // accumulator whose gate skips refreshes closer than 200 ms.
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

        let cpus = self.sys.cpus();
        let proc_stat = std::fs::read_to_string("/proc/stat").ok();
        let cpu_sample = proc_stat
            .as_deref()
            .and_then(|text| self.cpu_stats.sample(text));
        // `/proc/stat` carries one `cpuN` line per logical processor, so its line count is the
        // live count. sysinfo's CPU list is a snapshot from construction that never shrinks and
        // only grows through `refresh_cpu_list`, so it is the fallback, not the source: a CPU
        // removed at runtime would otherwise be counted for the rest of the run, and a count
        // published as 1 would scale CPU time by the core count in App History and the threads
        // view.
        let logical = proc_stat
            .as_deref()
            .map(linux_telemetry::cpu_count)
            .filter(|count| *count > 0)
            .or_else(|| (!cpus.is_empty()).then_some(cpus.len()))
            .unwrap_or(1);
        // Zero when the current speed is not demanded, which the UI renders as
        // unavailable instead of reusing a stale or nominal clock.
        //
        // Averaged over the CPUs actually summed: a CPU hot-added at runtime raises `logical`
        // while sysinfo's list stays as it was.
        let freq = if providers.cpu_speed {
            let summed = cpus.iter().map(|c| c.frequency() as f32).sum::<f32>();
            summed / cpus.len().max(1) as f32
        } else {
            0.0
        };
        let (physical_cores, sockets) = cpu_topology();
        // The per-process window is the same two `/proc/stat` reads, so a
        // process' share and the machine's utilization cannot drift apart.
        self.process_cpu
            .start(cpu_sample.as_ref().map(|sample| sample.window_total_ticks));

        // EWMH makes the X11 client inventory available when DISPLAY is
        // usable. Native Wayland has no portable whole-window inventory, so a
        // Wayland session observes at most its XWayland clients. Per-process
        // systemd desktop-role evidence below supplies the native-Wayland
        // fallback without inventing window facts.
        let window_observation = self.window_observer.observe();

        let n_procs = self
            .sys
            .processes()
            .values()
            .filter(|p| p.thread_kind().is_none())
            .count();
        let sessions_available = sessions::available();
        let btime_and_hz = proc_stat
            .as_deref()
            .and_then(parse_proc_stat_btime)
            .zip(linux_clk_tck());
        let mut processes = Vec::with_capacity(n_procs);
        let mut live_pids = HashSet::with_capacity(n_procs);
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
            live_pids.insert(pid_u);
            let name = p.name().to_string_lossy().into_owned();

            // One `/proc/<pid>/stat` read feeds both the identity bracket and
            // the CPU accountant, so the counters can only describe the record
            // the bracket just validated.
            let stat_bytes = std::fs::read(format!("/proc/{pid_u}/stat")).ok();
            let boot_start_ticks = before_refresh.get(&pid_u).copied().filter(|&before| {
                stat_bytes.as_deref().and_then(parse_proc_stat_starttime) == Some(before)
            });
            let proc_cpu = stat_bytes
                .as_deref()
                .and_then(process_cpu::parse_pid_cpu_stat)
                .map(|stat| self.process_cpu.observe(pid_u, stat));
            let desktop_evidence = boot_start_ticks
                .map_or_else(desktop_session::DesktopEvidence::default, |ticks| {
                    self.desktop_evidence.evidence(pid_u, ticks, started)
                });
            // Only a process whose generation this tick bracketed has an
            // authoritative role. `None` tells the classifier that nothing was
            // observed, so it may still use its userspace-name fallback.
            let desktop_role = boot_start_ticks.map(|_| desktop_evidence.role_marker());
            let has_window = window_evidence(
                window_observation.owners.get(&pid_u).copied(),
                boot_start_ticks,
                window_observation.coverage,
            );
            let category = classify_collected_process(pid_u, &name, has_window, desktop_role);

            let du = p.disk_usage();
            let mut entry = ProcessEntry::new(pid_u, name.clone());
            entry.display = name.clone();
            entry.ppid = p.parent().map(|x| x.as_u32());
            entry.category = category;
            entry.desktop_role = desktop_role;
            entry.cpu_pct = proc_cpu.and_then(|cpu| cpu.pct);
            entry.mem_bytes = p.memory();
            entry.working_set_bytes = Some(p.memory());
            entry.commit_bytes = Some(p.virtual_memory());
            entry.peak_mem_bytes = proc_status_kb(pid_u, "VmHWM");
            entry.boot_start_ticks = boot_start_ticks;
            entry.start_epoch_s =
                entry
                    .boot_start_ticks
                    .zip(btime_and_hz)
                    .and_then(|(ticks, (btime_s, clk_tck))| {
                        linux_start_timestamp(btime_s, ticks, clk_tck)
                    });
            entry.cpu_time_s = proc_cpu.and_then(|cpu| cpu.cpu_time_s);
            entry.disk_read_bps = du.read_bytes as f64 / interval_s;
            entry.disk_write_bps = du.written_bytes as f64 / interval_s;
            entry.disk_read_total = du.total_read_bytes;
            entry.disk_write_total = du.total_written_bytes;
            entry.has_window = has_window == Some(true);
            entry.app_group = if category == ProcCategory::App {
                desktop_evidence.app_group.clone()
            } else {
                None
            };
            entry.exe_path = p.exe().map(|e| e.to_path_buf());
            entry.user = p
                .user_id()
                .and_then(|uid| username_for_uid(&self.users, uid));
            entry.status = match p.status() {
                sysinfo::ProcessStatus::Stop => ProcStatus::Suspended,
                _ => ProcStatus::Running,
            };
            if sessions_available {
                entry.session_id = desktop_evidence.session_id;
            }
            // sysinfo's task set excludes the leader; Windows' count includes
            // it, so add the main thread back for parity.
            entry.threads = p.tasks().map(|tasks| tasks.len() as u32 + 1);
            entry.handles = fd_count(pid_u);
            entry.command_line = cached_cmdline(&mut self.cmdlines, pid_u, boot_start_ticks);
            entry.priority = proc_priority(stat_bytes.as_deref());
            entry.elevated = entry.user.as_deref().map(|u| u == "root");
            processes.push(entry);
        }
        self.desktop_evidence.retain_live(&live_pids);
        self.process_cpu.retain_live(&live_pids);
        self.cmdlines.retain(|pid, _| live_pids.contains(pid));
        mark_desktop_app_roots(&mut processes);

        // Disk rates and active time are one `/proc/diskstats` read per tick;
        // they follow the demand that shows them. Forgetting the previous
        // counters while it is off keeps the first tick back unmeasured instead
        // of dividing a whole off window by one tick.
        let diskstats = if providers.disk_rates {
            diskstats::read()
        } else {
            diskstats::forget();
            Vec::new()
        };
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
            // Both rates need the same previous sample, so they are measured
            // together or not at all.
            let read_bps = ds.and_then(|s| s.read_bps(interval_s));
            let write_bps = ds.and_then(|s| s.write_bps(interval_s));
            disks.push(DiskInfo {
                id: dev.clone(),
                number: None,
                mount,
                label: String::new(),
                media,
                total_bytes: d.total_space(),
                free_bytes: d.available_space(),
                active_pct: ds.and_then(|s| s.active_pct(interval_s)),
                // The model has no "unknown" byte rate, so an unmatched device
                // or a first sample still writes 0 B/s below — but it says so
                // here, and the page renders a gap and "—" instead.
                rates_measured: read_bps.is_some() && write_bps.is_some(),
                read_bps: read_bps.unwrap_or(0.0),
                write_bps: write_bps.unwrap_or(0.0),
                avg_resp_ms: 0.0,
                total_read_bytes: ds.map(|s| s.read_sectors * 512).unwrap_or(0),
                total_written_bytes: ds.map(|s| s.write_sectors * 512).unwrap_or(0),
            });
        }

        let network_metadata = net_meta::collect();
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
            let meta = network_metadata
                .get(name.as_str())
                .cloned()
                .unwrap_or_default();
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
                ipv4: meta.ipv4,
                ipv6: meta.ipv6,
                signal_quality_pct: None,
                wifi_access_denied: false,
            });
        }
        self.prev_net_totals = nets
            .iter()
            .map(|n| (n.name.clone(), (n.total_recv_bytes, n.total_sent_bytes)))
            .collect();

        // Every memory counter below comes from this one `/proc/meminfo` read;
        // a counter the kernel omits stays 0 in the model, which has no
        // "unknown" for it.
        let meminfo = std::fs::read_to_string("/proc/meminfo")
            .map(|text| linux_telemetry::parse_meminfo(&text))
            .unwrap_or_default();

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
                utilization_pct_available: cpu_sample.is_some(),
                utilization_pct: cpu_sample
                    .as_ref()
                    .map_or(0.0, |sample| sample.utilization_pct),
                per_core_pct: cpu_sample
                    .as_ref()
                    .map_or_else(Vec::new, |sample| sample.per_core_pct.clone()),
                per_core_kernel_pct: cpu_sample
                    .as_ref()
                    .map_or_else(Vec::new, |sample| sample.per_core_kernel_pct.clone()),
                kernel_pct: cpu_sample.as_ref().map_or(0.0, |sample| sample.kernel_pct),
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
                cached_bytes: meminfo
                    .cached_bytes
                    .unwrap_or(0)
                    .saturating_add(meminfo.sreclaimable_bytes.unwrap_or(0)),
                commit_total_bytes: meminfo.commit_limit_bytes.unwrap_or(0),
                commit_used_bytes: meminfo.committed_as_bytes.unwrap_or(0),
                paged_pool_bytes: 0,
                non_paged_pool_bytes: 0,
                swap_total_bytes: self.sys.total_swap(),
                swap_used_bytes: self.sys.used_swap(),
                ..Default::default()
            },
            disks,
            networks: nets,
            gpus: drm::gpus(providers.measure_gpus),
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

fn mark_desktop_app_roots(processes: &mut [ProcessEntry]) {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, process) in processes.iter().enumerate() {
        if process.category != ProcCategory::App {
            continue;
        }
        if let Some(group) = process
            .app_group
            .as_deref()
            .filter(|group| !group.is_empty())
        {
            groups.entry(group.to_owned()).or_default().push(index);
        }
    }
    for members in groups.values() {
        let Some(&root) = members.iter().min_by_key(|&&index| {
            let process = &processes[index];
            (
                !process.has_window,
                process.creation_identity().unwrap_or(i64::MAX),
                process.pid,
            )
        }) else {
            continue;
        };
        processes[root].app_root = true;
    }
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

/// Command line of one process generation.
///
/// Read once per (pid, start ticks) instead of on every tick. Two honest limits: `execve`
/// keeps the creation ticks, so a process that re-execs itself keeps the command line it was
/// first seen with, and a `setproctitle`/`PR_SET_MM_ARG_START` rewrite is not observed until
/// it exits — Windows makes the same trade. A failed read is deliberately not remembered:
/// `hidepid` and a fork that has not filled `argv` yet both clear, so the next tick retries
/// instead of blanking that row for the rest of the session. A kernel thread has no command
/// line at all, so it stays uncached and is re-read each tick — which is what the pre-cache
/// code did for every process.
fn cached_cmdline(
    cache: &mut HashMap<u32, (i64, Option<String>)>,
    pid: u32,
    start_ticks: Option<i64>,
) -> Option<String> {
    let Some(ticks) = start_ticks else {
        return proc_cmdline(pid);
    };
    let known = cache
        .get(&pid)
        .is_some_and(|(cached_ticks, _)| *cached_ticks == ticks);
    if known {
        return cache.get(&pid).and_then(|(_, cmdline)| cmdline.clone());
    }
    let cmdline = proc_cmdline(pid);
    if cmdline.is_some() {
        cache.insert(pid, (ticks, cmdline.clone()));
    }
    cmdline
}

/// Priority class for the `/proc/<pid>/stat` record the sample loop already
/// read. Parsing after the final ')' avoids spaces and parentheses in process
/// names corrupting positional fields.
fn proc_priority(stat_bytes: Option<&[u8]>) -> PriorityClass {
    stat_bytes
        .and_then(linux_telemetry::parse_proc_stat_nice)
        .map_or(PriorityClass::Unknown, priority_for_nice)
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
/// untouched. A thread that exited between listing and the call (ESRCH) is
/// not a miss; any other failure is reported so a permission denial can
/// reach the fail-closed elevation policy instead of being hidden.
fn apply_to_threads(
    op: &'static str,
    tids: &[u32],
    mut apply: impl FnMut(u32) -> std::io::Result<()>,
) -> Result<()> {
    let Some((&leader, rest)) = tids.split_first() else {
        return Err(tm_core::TmError::platform(op, "no thread to update"));
    };
    apply(leader).map_err(|error| action_io_error(op, error))?;
    let mut failed = 0usize;
    let mut first_failure = None;
    for &tid in rest {
        match apply(tid) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => {
                failed += 1;
                first_failure.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_failure {
        tracing::warn!(
            pid = leader,
            op,
            failed,
            threads = tids.len(),
            "process-wide change did not reach every thread"
        );
        return Err(action_io_error(op, error));
    }
    Ok(())
}

fn action_io_error(context: &'static str, error: std::io::Error) -> tm_core::TmError {
    if error.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES))
    {
        tm_core::TmError::platform(context, format!("permission denied: {error}"))
    } else {
        tm_core::TmError::platform(context, error.to_string())
    }
}

/// Physical core and package counts. Both ids describe static hardware, so the
/// sysfs walk runs once per process; an unreadable tree stays `(0, 0)`.
fn cpu_topology() -> (usize, usize) {
    static TOPOLOGY: OnceLock<(usize, usize)> = OnceLock::new();
    let root = Path::new("/sys/devices/system/cpu");
    *TOPOLOGY.get_or_init(|| linux_telemetry::cpu_topology(root))
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

/// Highest cpufreq `base_frequency` in MHz. The base clock describes static
/// hardware, so it is derived once; 0 (unknown) when the kernel publishes none.
fn base_freq_from_cpufreq() -> f32 {
    static BASE_MHZ: OnceLock<f32> = OnceLock::new();
    let root = Path::new("/sys/devices/system/cpu/cpufreq");
    *BASE_MHZ.get_or_init(|| linux_telemetry::base_frequency_mhz(root))
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

// ------------------------------------------------------------------ actions

pub struct LinuxActions;

impl PlatformActions for LinuxActions {
    fn capabilities(&self) -> Capabilities {
        // The process read/action flags describe surfaces this backend
        // delivers with the user's own token through procfs and syscalls, and
        // for other users' processes through the broker when it is installed.
        // They stay independent of the broker's presence on purpose: a missing
        // service must neither blank a tab nor disable an action the direct
        // path still delivers, and no flag here promises something no path can
        // carry (module unloading, a whole-machine network view).
        Capabilities {
            services_list: services::available(),
            services_control: services::available(),
            open_services_app: false,
            startup_toggle: true,
            end_process_tree: true,
            users_sessions: sessions::available(),
            user_disconnect: false,
            manage_user_accounts: false,
            end_process: true,
            suspend_resume: true,
            set_priority: true,
            set_affinity: true,
            efficiency_mode: false,
            run_new_task: true,
            run_elevated: false,
            per_process_network: false,
            process_modules: true,
            process_threads: true,
            process_connections: true,
            process_file_io: true,
            unload_module: false,
            start_with_system: autostart::supported(),
            elevation_status: true,
            relaunch_elevated: false,
            start_elevated: false,
        }
    }

    fn list_services(&self) -> Result<Vec<ServiceInfo>> {
        services::list_systemd_units()
    }
    fn control_service(&self, name: &str, action: ServiceAction) -> Result<()> {
        let direct = services::control_unit_direct(name, action);
        if direct.is_ok() {
            return direct;
        }
        // Only a real authorization failure is worth asking anyone else: a
        // missing unit or a broken systemctl must not become a prompt.
        if !direct
            .as_ref()
            .err()
            .is_some_and(polkit::is_permission_denied)
        {
            return direct;
        }
        let request = Request::Action {
            action: ActionRequest::Service {
                unit: name.to_string(),
                verb: service_verb(action),
            },
        };
        match client::request(&request) {
            // The broker runs as root, so its answer is the privileged one.
            Ok(Response::Done) => Ok(()),
            Ok(other) => Err(unexpected_response(request.label(), &other)),
            // No broker installed or answering: today's Polkit path, unchanged.
            Err(error) if error.may_fall_back() => polkit::direct_or_elevated(
                direct,
                PrivilegedAction::Service {
                    unit: name.to_string(),
                    action,
                },
            ),
            // Root itself declined. A Polkit prompt would only ask the user to
            // repeat what the privileged path already refused.
            Err(error) => Err(client_error(request.label(), &error)),
        }
    }
    fn list_startup(&self) -> Result<Vec<StartupItem>> {
        Ok(startup::list_autostart())
    }
    fn set_startup_enabled(&self, item_id: &str, _location: &str, enabled: bool) -> Result<()> {
        startup::set_enabled(item_id, enabled)
    }
    fn list_user_sessions(&self) -> Result<Vec<UserSession>> {
        sessions::list_sessions()
    }
    fn set_start_with_system(&self, enabled: bool, start_minimized: bool) -> Result<()> {
        if enabled && start_minimized {
            tracing::debug!(
                "Linux XDG autostart has no supported hidden-start mode; starting visibly"
            );
        }
        autostart::set_enabled(enabled)
    }

    fn kill_single(&self, pid: u32) -> Result<()> {
        self.kill_process(pid, linux_proc_start_ticks(pid), false)
    }

    fn suspend_process(&self, pid: u32, suspend: bool) -> Result<()> {
        self.suspend_process_checked(pid, linux_proc_start_ticks(pid), suspend)
    }

    fn set_priority(&self, pid: u32, priority: PriorityClass) -> Result<()> {
        self.set_priority_checked(pid, linux_proc_start_ticks(pid), priority)
    }

    fn get_affinity_mask(&self, pid: u32) -> Result<u64> {
        self.get_affinity_mask_checked(pid, linux_proc_start_ticks(pid))
    }

    fn set_affinity_mask(&self, pid: u32, mask: u64) -> Result<()> {
        self.set_affinity_mask_checked(pid, linux_proc_start_ticks(pid), mask)
    }

    fn kill_process(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        tree: bool,
    ) -> Result<()> {
        validate_mutation_target(pid)?;
        let (direct, delivered) =
            process_tree::kill_process_with_delivery(pid, expected_start_epoch_s, tree);
        // The broker is the only path that can signal another user's process,
        // and it re-reads the creation ticks itself. The tree case is included:
        // the service walks the descendants as root, which a portable,
        // user-writable GUI image can never do safely.
        //
        // A walk that already signalled something is NOT retried: it exits part
        // way through (`Ok` for one descendant, EPERM for the next), and asking
        // the service to signal the whole tree again would deliver a second
        // SIGTERM to the part that is left standing.
        if !delivered
            && let Some(result) = brokered_action(&direct, expected_start_epoch_s, |start_ticks| {
                ActionRequest::Kill {
                    pid,
                    start_ticks,
                    tree,
                }
            })
        {
            return result;
        }
        match (expected_start_epoch_s, tree) {
            (Some(start_ticks), false) => {
                polkit::direct_or_elevated(direct, PrivilegedAction::Kill { pid, start_ticks })
            }
            // Portable builds cannot safely execute their user-writable image
            // as root to reproduce the tree walk. Preserve the direct
            // identity-checked result instead of killing only the root.
            _ => direct,
        }
    }
    fn suspend_process_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        suspend: bool,
    ) -> Result<()> {
        validate_mutation_target(pid)?;
        let signal = if suspend {
            libc::SIGSTOP
        } else {
            libc::SIGCONT
        };
        let direct = send_signal_checked(pid, expected_start_epoch_s, signal);
        if let Some(result) = brokered_action(&direct, expected_start_epoch_s, |start_ticks| {
            if suspend {
                ActionRequest::Suspend { pid, start_ticks }
            } else {
                ActionRequest::Resume { pid, start_ticks }
            }
        }) {
            return result;
        }
        match (expected_start_epoch_s, suspend) {
            (Some(start_ticks), true) => {
                polkit::direct_or_elevated(direct, PrivilegedAction::Suspend { pid, start_ticks })
            }
            (Some(start_ticks), false) => {
                polkit::direct_or_elevated(direct, PrivilegedAction::Resume { pid, start_ticks })
            }
            (None, _) => direct,
        }
    }
    fn set_priority_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        priority: PriorityClass,
    ) -> Result<()> {
        validate_mutation_target(pid)?;
        let direct = set_priority_direct(pid, expected_start_epoch_s, priority);
        if let Some(result) = brokered_action(&direct, expected_start_epoch_s, |start_ticks| {
            ActionRequest::SetPriority {
                pid,
                start_ticks,
                priority,
            }
        }) {
            return result;
        }
        match expected_start_epoch_s {
            Some(start_ticks) => polkit::direct_or_elevated(
                direct,
                PrivilegedAction::SetPriority {
                    pid,
                    start_ticks,
                    priority,
                },
            ),
            None => direct,
        }
    }
    fn get_affinity_mask_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<u64> {
        verify_process_identity(pid, expected_start_epoch_s)?;
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(pid as i32, std::mem::size_of::<libc::cpu_set_t>(), &mut set)
                != 0
            {
                return Err(action_io_error(
                    "sched_getaffinity",
                    std::io::Error::last_os_error(),
                ));
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
    fn set_affinity_mask_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        mask: u64,
    ) -> Result<()> {
        validate_mutation_target(pid)?;
        let direct = set_affinity_direct(pid, expected_start_epoch_s, mask);
        if let Some(result) = brokered_action(&direct, expected_start_epoch_s, |start_ticks| {
            ActionRequest::SetAffinity {
                pid,
                start_ticks,
                mask,
            }
        }) {
            return result;
        }
        match expected_start_epoch_s {
            Some(start_ticks) => polkit::direct_or_elevated(
                direct,
                PrivilegedAction::SetAffinity {
                    pid,
                    start_ticks,
                    mask,
                },
            ),
            None => direct,
        }
    }

    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        polkit::run_new_task(command_line, elevate)
    }

    // Deliberately no `run_new_task_probe` override: that contract is
    // non-launching on Linux. The Run dialog uses `run_new_task` above.
    fn is_elevated(&self) -> bool {
        polkit::is_elevated()
    }

    fn relaunch_elevated(&self) -> Result<()> {
        polkit::relaunch_elevated()
    }

    fn core_service_state(&self) -> CoreServiceState {
        // Both inputs are cheap: the manifest is one `stat`, and `status()` is a
        // cache read that refreshes on its own thread, so this cannot block the
        // render path it is called from.
        let installed = manifest_installed();
        // A manifest-less machine has nothing to ask, so the probe is skipped
        // entirely and the reason travels with the state.
        let status = if installed {
            client::status()
        } else {
            Err(ClientError::Unavailable(
                "the core service is not installed".into(),
            ))
        };
        core_service_state(installed, status)
    }

    fn open_file_location(&self, path: &str) -> Result<()> {
        shell::open_file_location(path)
    }

    fn open_properties(&self, path: &str) -> Result<()> {
        shell::open_properties(path)
    }

    fn open_url(&self, url: &str) -> Result<()> {
        shell::open_url(url)
    }

    fn list_process_modules(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<Vec<ProcessModule>> {
        let before = process_start_for_read(pid, expected_start_epoch_s)?;
        match process_modules::list_process_modules(pid) {
            Ok(modules) => {
                ensure_process_generation(pid, before)?;
                Ok(modules)
            }
            // `/proc/<pid>/maps` is readable only by the process' own user, so
            // another user's process is exactly the case the broker answers.
            Err(direct_error) if polkit::is_permission_denied(&direct_error) => {
                let request = Request::ProcessModules {
                    pid,
                    start_ticks: before,
                };
                let modules = brokered_read(&request, direct_error, |response| match response {
                    Response::Modules(wire) => Ok(wire.into_iter().map(module_from_wire).collect()),
                    other => Err(other),
                })?;
                // The service re-checked the generation inside the request; our
                // own bracket then proves the answer still describes the process
                // this read asked about.
                ensure_process_generation(pid, before)?;
                Ok(modules)
            }
            Err(error) => Err(error),
        }
    }

    fn list_process_threads(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<ProcessThreads> {
        let before = process_start_for_read(pid, expected_start_epoch_s)?;
        match process_threads::list_process_threads(pid, Some(before)) {
            Ok(threads) => {
                ensure_process_generation(pid, before)?;
                Ok(threads)
            }
            Err(direct_error) if polkit::is_permission_denied(&direct_error) => {
                let request = Request::ProcessThreads {
                    pid,
                    start_ticks: before,
                };
                let threads = brokered_read(&request, direct_error, |response| match response {
                    Response::Threads(wire) => Ok(threads_from_wire(wire)),
                    other => Err(other),
                })?;
                ensure_process_generation(pid, before)?;
                Ok(threads)
            }
            Err(error) => Err(error),
        }
    }

    fn list_process_connections(&self, pid: u32) -> Result<Vec<ProcessConnection>> {
        let before = process_start_for_read(pid, None)?;
        match process_connections::list_process_connections(pid) {
            Ok(connections) => {
                ensure_process_generation(pid, before)?;
                Ok(connections)
            }
            Err(direct_error) if polkit::is_permission_denied(&direct_error) => {
                // The connection table carries no creation time, so the sampled
                // generation travels with the request and is closed again below.
                let request = Request::ProcessConnections {
                    pid,
                    start_ticks: before,
                };
                let mapping = brokered_read(&request, direct_error, |response| match response {
                    Response::Connections(wire) => Ok(connections_from_wire(wire)),
                    other => Err(other),
                })?;
                if !mapping.unparsable.is_empty() && broker_diagnostic_allowed() {
                    // Dropping the row keeps the rest of the answer usable, but
                    // a defect must not be invisible: name the endpoint.
                    tracing::debug!(
                        pid,
                        dropped = mapping.unparsable.len(),
                        first = mapping.unparsable.first().map(String::as_str),
                        "core service answered endpoints this build cannot parse"
                    );
                }
                ensure_process_generation(pid, before)?;
                Ok(mapping.connections)
            }
            Err(error) => Err(error),
        }
    }

    fn backend_name(&self) -> &'static str {
        "linux"
    }

    fn process_icon_rgba(&self, exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
        // Adhere to the desktop's icon theme: resolve the exe through its
        // .desktop entry into the current icon theme (XDG spec), exactly what
        // the shell shows for the app.
        crate::xdg_icons::process_icon_rgba(exe_path)
    }

    fn default_app_icon_rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
        // Rows without an icon of their own (kernel threads, an app with no
        // desktop entry) keep the same theme: the standard executable icon.
        crate::xdg_icons::default_app_icon_rgba()
    }

    fn list_process_file_io(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<crate::actions::ProcessFileIoSnapshot> {
        let before = process_start_for_read(pid, expected_start_epoch_s)?;
        let direct = process_io::list_process_file_io(pid, Some(before));
        // There are two ways the direct walk is useless for another user's
        // process: `/proc/<pid>/fd` is owner-only and comes back as an empty,
        // incomplete snapshot, or another step fails outright with EPERM. Both
        // are the case the broker exists for; a snapshot that saw handles is a
        // real answer and never pays a round trip.
        let escalate = match &direct {
            Ok(snapshot) => !snapshot.accounting.handles_complete && snapshot.files.is_empty(),
            Err(error) => polkit::is_permission_denied(error),
        };
        if !escalate {
            return direct;
        }
        let request = Request::ProcessFileIo {
            pid,
            start_ticks: before,
        };
        let brokered = match client::request(&request) {
            Ok(Response::FileIo(wire)) => Some(file_io_from_wire(*wire)),
            Ok(other) => return Err(unexpected_response(request.label(), &other)),
            // No broker: the local answer, or its error, is the honest one.
            Err(error) if error.may_fall_back() => None,
            Err(error) => return Err(client_error(request.label(), &error)),
        };
        let Some(snapshot) = brokered else {
            return direct;
        };
        // The request pins the sampled generation, and closing our own bracket
        // as well proves the answer still describes the process asked about.
        ensure_process_generation(pid, before)?;
        Ok(snapshot)
    }
}

// -------------------------------------------------------- core service bridge
//
// The broker is the only path that can reach another user's or a protected
// process: `/proc/<pid>/maps` is readable only by its owner, `/proc/<pid>/fd`
// and `/proc/<pid>/task` need CAP_SYS_PTRACE for a foreign process, and
// signalling one needs the same. Every call site above therefore keeps the
// direct procfs/syscall attempt first — a direct attempt that worked never pays
// a round trip — and asks the broker only where the kernel answered
// EPERM/EACCES.
//
// Two rules hold everywhere:
//
// - Only a permission failure escalates. A vanished process, a malformed record
//   or an exhausted budget is the same answer under either identity.
// - The broker's answer ends the decision. A refusal is returned with the
//   service's own reason; it never becomes a Polkit prompt or a local retry.

/// Whether a failed direct attempt may be handed to the broker.
///
/// Both conditions are necessary: the kernel refused for lack of privilege, and
/// the caller knows the creation identity the service has to re-check. An
/// action without one could address a recycled pid, so it stays local.
fn may_escalate(direct: &Result<()>, start_ticks: Option<i64>) -> bool {
    start_ticks.is_some()
        && direct
            .as_ref()
            .err()
            .is_some_and(polkit::is_permission_denied)
}

/// Ask the broker for an action the direct attempt could not perform.
///
/// `None` means "continue with the pre-service path": no other identity would
/// change the outcome, or no broker is installed and answering. `Some(result)`
/// is final — the service either did it or gave its reason, and a reason must
/// never degrade into a second privileged attempt.
///
/// The caller decides whether a *partial* attempt may be retried: suspend,
/// resume, priority and affinity write absolute values, so re-applying one
/// cannot be observed twice, while a kill is gated on
/// [`process_tree::kill_process_with_delivery`] reporting that nothing was
/// delivered.
fn brokered_action(
    direct: &Result<()>,
    start_ticks: Option<i64>,
    action: impl FnOnce(i64) -> ActionRequest,
) -> Option<Result<()>> {
    if !may_escalate(direct, start_ticks) {
        return None;
    }
    // `start_ticks` is known here; `?` is the compiler's proof of it.
    let start_ticks = start_ticks?;
    let request = Request::Action {
        action: action(start_ticks),
    };
    match client::request(&request) {
        Ok(Response::Done) => Some(Ok(())),
        Ok(other) => Some(Err(unexpected_response(request.label(), &other))),
        // The broker never answered, so the caller's own path may still deliver
        // the operation the way it did before this service existed.
        Err(error) if error.may_fall_back() => None,
        // The context is the verb that failed, not a generic label.
        Err(error) => Some(Err(client_error(request.label(), &error))),
    }
}

/// Ask the broker for a per-process read the kernel refused.
///
/// A broker that never answered leaves the direct reason in place: it is the
/// honest one for this token, and the broker's absence is not a measurement. A
/// broker answer ends the decision — a refusal names itself instead of looking
/// like an empty result.
fn brokered_read<T>(
    request: &Request,
    direct_error: tm_core::TmError,
    decode: impl FnOnce(Response) -> std::result::Result<T, Response>,
) -> Result<T> {
    match client::request(request) {
        Ok(response) => match decode(response) {
            Ok(value) => Ok(value),
            Err(unexpected) => Err(unexpected_response(request.label(), &unexpected)),
        },
        Err(ClientError::Unavailable(_)) => Err(direct_error),
        Err(error) => Err(client_error(request.label(), &error)),
    }
}

fn unexpected_response(context: &'static str, response: &Response) -> tm_core::TmError {
    tm_core::TmError::platform(
        context,
        format!(
            "the core service answered with an unexpected {} payload",
            response_label(response)
        ),
    )
}

/// Whether a broker diagnostic may be logged now.
///
/// A per-process tab can ask several times per second and a defective answer
/// repeats identically, so one line per window is enough to diagnose it without
/// filling the log.
fn broker_diagnostic_allowed() -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    const WINDOW_MS: u64 = 30_000;
    static START: OnceLock<Instant> = OnceLock::new();
    static LAST_MS: AtomicU64 = AtomicU64::new(0);

    let now = START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX - 1)) as u64
        + 1;
    let mut previous = LAST_MS.load(Ordering::Relaxed);
    loop {
        if previous != 0 && now.saturating_sub(previous) < WINDOW_MS {
            return false;
        }
        match LAST_MS.compare_exchange_weak(previous, now, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => previous = actual,
        }
    }
}

/// Whether the installer wrote the protected manifest. Its absence is the only
/// proof this machine has no core service; the socket alone could be stale.
fn manifest_installed() -> bool {
    std::fs::metadata(MANIFEST_PATH).is_ok()
}

fn set_priority_direct(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
    priority: PriorityClass,
) -> Result<()> {
    verify_process_identity(pid, expected_start_epoch_s)?;
    let Some(nice) = nice_for_priority(priority) else {
        return Err(tm_core::TmError::platform(
            "setpriority",
            "an explicit priority class is required",
        ));
    };
    let tids = process_tids(pid);
    verify_process_identity(pid, expected_start_epoch_s)?;
    apply_to_threads("setpriority", &tids, |tid| {
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })
}

fn set_affinity_direct(pid: u32, expected_start_epoch_s: Option<i64>, mask: u64) -> Result<()> {
    verify_process_identity(pid, expected_start_epoch_s)?;
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
    let tids = process_tids(pid);
    verify_process_identity(pid, expected_start_epoch_s)?;
    apply_to_threads("sched_setaffinity", &tids, |tid| {
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

fn send_signal(pid: u32, sig: i32) -> Result<()> {
    let Ok(pid_t) = libc::pid_t::try_from(pid) else {
        return Err(tm_core::TmError::ProcessNotFound { pid });
    };
    let rc = unsafe { libc::kill(pid_t, sig) };
    if rc == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Err(tm_core::TmError::ProcessNotFound { pid })
        } else {
            Err(action_io_error("kill", error))
        }
    }
}

fn validate_mutation_target(pid: u32) -> Result<()> {
    if pid <= 1 || pid == std::process::id() {
        return Err(tm_core::TmError::platform(
            "process mutation",
            format!("pid {pid} is TaskMan itself or the init process"),
        ));
    }
    Ok(())
}

pub(super) fn send_signal_checked(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
    sig: i32,
) -> Result<()> {
    if pid == 0 {
        return Err(tm_core::TmError::platform("kill", "system process"));
    }
    let expected = expected_start_epoch_s.ok_or_else(|| {
        tm_core::TmError::platform("kill", "process creation identity unavailable")
    })?;
    let Ok(pid_t) = libc::pid_t::try_from(pid) else {
        return Err(tm_core::TmError::ProcessNotFound { pid });
    };

    struct OwnedFd(libc::c_int);
    impl Drop for OwnedFd {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.0);
            }
        }
    }

    // Acquire a pidfd first so the kernel object identity is pinned across the
    // /proc/<pid>/stat verification and the signal delivery itself.
    let raw_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid_t, 0 as libc::c_uint) };
    if raw_fd >= 0 {
        let fd = OwnedFd(raw_fd as libc::c_int);
        verify_process_identity(pid, Some(expected))?;
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.0,
                sig as libc::c_int,
                std::ptr::null::<libc::c_void>(),
                0 as libc::c_uint,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ESRCH) => Err(tm_core::TmError::ProcessNotFound { pid }),
            Some(libc::EPERM | libc::EACCES) => Err(action_io_error("pidfd_send_signal", error)),
            _ => Err(tm_core::TmError::platform(
                "pidfd_send_signal",
                format!("signal {sig} failed"),
            )),
        };
    }

    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Err(tm_core::TmError::ProcessNotFound { pid }),
        // Old kernels (< 5.1) or restrictive seccomp profiles may block pidfd_open:
        // verify start time against /proc before falling back to kill(2).
        Some(libc::ENOSYS | libc::EPERM | libc::EACCES) => {
            verify_process_identity(pid, Some(expected))?;
            send_signal(pid, sig)
        }
        other => Err(tm_core::TmError::platform(
            "pidfd_open",
            format!("pidfd_open failed: {other:?}"),
        )),
    }
}

fn parse_proc_stat_btime(proc_stat: &str) -> Option<u64> {
    for line in proc_stat.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            return rest.split_ascii_whitespace().next()?.parse().ok();
        }
    }
    None
}

fn parse_proc_stat_starttime(pid_stat: &[u8]) -> Option<i64> {
    // Field 2 (comm) can contain spaces, right parentheses and non-UTF-8 bytes.
    let after_comm = pid_stat.iter().rposition(|&byte| byte == b')')? + 1;
    let ticks = pid_stat[after_comm..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .nth(19)?;
    std::str::from_utf8(ticks).ok()?.parse().ok()
}

fn linux_start_timestamp(btime_s: u64, ticks_since_boot: i64, clk_tck: u64) -> Option<i64> {
    if btime_s == 0 || clk_tck == 0 {
        return None;
    }
    let ticks_per_sec = i128::from(HIGH_RES_TICKS_PER_SECOND);
    let boot_ticks = i128::from(btime_s) * ticks_per_sec;
    let offset_ticks = i128::from(ticks_since_boot) * ticks_per_sec / i128::from(clk_tck);
    i64::try_from(boot_ticks + offset_ticks).ok()
}

fn linux_proc_start_ticks(pid: u32) -> Option<i64> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    parse_proc_stat_starttime(&stat)
}

fn read_proc_start_ticks(pid: u32) -> Result<i64> {
    let path = format!("/proc/{pid}/stat");
    let stat = match std::fs::read(&path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(tm_core::TmError::ProcessNotFound { pid });
        }
        Err(error) => {
            return Err(tm_core::TmError::platform(
                "read process identity",
                format!("{path}: {error}"),
            ));
        }
    };
    parse_proc_stat_starttime(&stat).ok_or_else(|| {
        tm_core::TmError::platform(
            "read process identity",
            format!("{path}: malformed process record"),
        )
    })
}

fn linux_clk_tck() -> Option<u64> {
    let raw = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(raw).ok().filter(|&hz| hz > 0)
}

fn window_evidence(
    owner_start_ticks: Option<i64>,
    current_start_ticks: Option<i64>,
    coverage: x11_windows::WindowCoverage,
) -> Option<bool> {
    match (owner_start_ticks, current_start_ticks) {
        (Some(owner), Some(current)) if owner == current => Some(true),
        // The owner existed but belongs to another process generation.
        (Some(_), _) => None,
        (None, Some(_)) if coverage == x11_windows::WindowCoverage::Complete => Some(false),
        _ => None,
    }
}

/// Classify with the desktop role this tick actually observed.
///
/// `desktop_role` is `Some` only when the sample bracketed the process's
/// generation, and then it is authoritative even when it is Unknown (an
/// app.slice unit with no graphical evidence, or a cgroup this build cannot
/// read). `None` means nothing was observed — the process appeared after the
/// bracket, or its `/proc/<pid>/stat` read failed — and only then may `classify`
/// take its Unix userspace-name fallback, which keeps a debuting
/// `dbus-daemon`/`systemd` from rendering as Unknown for a tick.
fn classify_collected_process(
    pid: u32,
    name: &str,
    has_window: Option<bool>,
    desktop_role: Option<classify::DesktopRoleEvidence>,
) -> ProcCategory {
    classify::classify(classify::ClassifyInput {
        platform: classify::ProcessPlatform::Unix,
        pid,
        name,
        has_window,
        critical: None,
        windows_owned: None,
        desktop_role,
    })
}

pub(super) fn verify_process_identity(pid: u32, expected_start_ticks: Option<i64>) -> Result<()> {
    let expected = expected_start_ticks.ok_or_else(|| {
        tm_core::TmError::platform(
            "verify process identity",
            "process creation identity unavailable",
        )
    })?;
    if read_proc_start_ticks(pid)? != expected {
        return Err(tm_core::TmError::ProcessNotFound { pid });
    }
    Ok(())
}

/// Read the process generation for a read-only procfs inspection. A supplied
/// sampled token must match exactly; it is a boot-relative start tick on
/// Linux, despite the shared trait's historical parameter name.
fn process_start_for_read(pid: u32, expected_start_ticks: Option<i64>) -> Result<i64> {
    let before = read_proc_start_ticks(pid)?;
    if expected_start_ticks.is_some_and(|expected| expected != before) {
        return Err(tm_core::TmError::ProcessNotFound { pid });
    }
    Ok(before)
}

fn ensure_process_generation(pid: u32, before: i64) -> Result<()> {
    if read_proc_start_ticks(pid)? != before {
        return Err(tm_core::TmError::ProcessNotFound { pid });
    }
    Ok(())
}

pub fn create_collector() -> LinuxCollector {
    LinuxCollector {
        sys: System::new_all(),
        disks: Disks::new_with_refreshed_list(),
        networks: Networks::new_with_refreshed_list(),
        users: sysinfo::Users::new_with_refreshed_list(),
        cpu_stats: cpu_stat::CpuStatSampler::default(),
        process_cpu: process_cpu::ProcessCpuSampler::new(linux_clk_tck()),
        prev_net_totals: HashMap::new(),
        last_tick: None,
        first_tick_done: false,
        demand: TelemetryDemand::core(),
        cmdlines: HashMap::new(),
        cache_kb: cache_totals_kb(&read_cache_indexes()),
        window_observer: x11_windows::WindowObserver::new(),
        desktop_evidence: desktop_session::DesktopEvidenceCache::new(),
    }
}

pub fn create_actions() -> LinuxActions {
    LinuxActions
}

/// Whether this Linux process has effective uid 0.
pub fn is_elevated() -> bool {
    polkit::is_elevated()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_inspection_capabilities_match_the_read_only_procfs_backend() {
        let capabilities = create_actions().capabilities();
        assert!(capabilities.process_modules);
        assert!(capabilities.process_threads);
        assert!(capabilities.process_connections);
        assert!(capabilities.end_process_tree);
        assert!(!capabilities.unload_module);
        assert!(capabilities.run_new_task);
        assert!(!capabilities.run_elevated);
        assert!(capabilities.elevation_status);
        assert!(!capabilities.relaunch_elevated);
        assert!(!capabilities.start_elevated);
        assert_eq!(capabilities.start_with_system, autostart::supported());
        assert!(!capabilities.open_services_app);
        assert!(!capabilities.manage_user_accounts);
    }

    #[test]
    fn elevated_run_is_rejected_before_spawning_anything() {
        let error = create_actions().run_new_task("true", true).unwrap_err();
        assert!(matches!(error, tm_core::TmError::Unsupported(_)));
    }

    #[test]
    fn x11_evidence_requires_identity_and_complete_coverage_for_negatives() {
        use x11_windows::WindowCoverage::{Complete, Partial, Unavailable};
        assert_eq!(window_evidence(Some(7), Some(7), Partial), Some(true));
        assert_eq!(window_evidence(Some(7), Some(8), Complete), None);
        assert_eq!(window_evidence(None, Some(7), Complete), Some(false));
        assert_eq!(window_evidence(None, Some(7), Partial), None);
        assert_eq!(window_evidence(None, Some(7), Unavailable), None);
    }

    #[test]
    fn app_role_survives_partial_or_unavailable_x11_coverage() {
        use x11_windows::WindowCoverage::{Complete, Partial, Unavailable};
        for coverage in [Partial, Unavailable] {
            let has_window = window_evidence(None, Some(7), coverage);
            assert_eq!(has_window, None);
            assert_eq!(
                classify::classify(classify::ClassifyInput {
                    platform: classify::ProcessPlatform::Unix,
                    pid: 3000,
                    name: "gnome-terminal",
                    has_window,
                    critical: None,
                    windows_owned: None,
                    desktop_role: Some(classify::DesktopRoleEvidence::App),
                }),
                ProcCategory::App
            );
        }
        let has_window = window_evidence(None, Some(7), Complete);
        assert_eq!(
            classify::classify(classify::ClassifyInput {
                platform: classify::ProcessPlatform::Unix,
                pid: 3000,
                name: "gnome-terminal",
                has_window,
                critical: None,
                windows_owned: None,
                desktop_role: Some(classify::DesktopRoleEvidence::App),
            }),
            ProcCategory::Background
        );
    }

    #[test]
    fn collector_classifier_preserves_authoritative_unknown_for_userspace_names() {
        let evidence = desktop_session::DesktopEvidence::default();
        assert_eq!(evidence.role, None, "the collector saw no positive role");
        for name in ["systemd", "dbus-daemon"] {
            assert_eq!(
                classify_collected_process(3000, name, None, Some(evidence.role_marker())),
                ProcCategory::Unknown,
                "a bracketed process must pass its authoritative Unknown marker for {name}, not None"
            );
            // The same row before a tick bracketed its generation: nothing was
            // observed, so the Unix userspace-name fallback still decides.
            assert_eq!(
                classify_collected_process(3000, name, None, None),
                ProcCategory::System,
                "an unobserved {name} must not render as Unknown"
            );
        }

        let mut background = desktop_session::DesktopEvidence::default();
        background.role = Some(classify::DesktopRoleEvidence::Background);
        assert_eq!(
            classify_collected_process(1, "systemd", None, Some(background.role_marker())),
            ProcCategory::Background,
            "an explicit user-session role must beat the Unix userspace-name fallback"
        );
    }

    #[test]
    fn desktop_app_roots_are_stable_within_one_scope_and_distinct_between_scopes() {
        let mut first = ProcessEntry::new(100, "app");
        first.category = ProcCategory::App;
        first.boot_start_ticks = Some(10);
        first.app_group = Some("scope-a".into());
        let mut second = ProcessEntry::new(101, "helper");
        second.category = ProcCategory::App;
        second.boot_start_ticks = Some(20);
        second.app_group = Some("scope-a".into());
        let mut other = ProcessEntry::new(200, "other");
        other.category = ProcCategory::App;
        other.boot_start_ticks = Some(30);
        other.app_group = Some("scope-b".into());
        let mut processes = vec![second, other, first];

        mark_desktop_app_roots(&mut processes);

        assert!(processes.iter().any(|p| p.pid == 100 && p.app_root));
        assert!(!processes.iter().any(|p| p.pid == 101 && p.app_root));
        assert!(processes.iter().any(|p| p.pid == 200 && p.app_root));
    }

    #[test]
    fn desktop_app_root_prefers_a_windowed_member_over_an_older_wrapper() {
        let mut wrapper = ProcessEntry::new(100, "wrapper");
        wrapper.category = ProcCategory::App;
        wrapper.boot_start_ticks = Some(10);
        wrapper.app_group = Some("scope-a".into());
        let mut windowed = ProcessEntry::new(101, "windowed-app");
        windowed.category = ProcCategory::App;
        windowed.boot_start_ticks = Some(20);
        windowed.has_window = true;
        windowed.app_group = Some("scope-a".into());
        let mut processes = vec![wrapper, windowed];

        mark_desktop_app_roots(&mut processes);

        assert!(
            !processes[0].app_root,
            "the short-lived wrapper is not the app"
        );
        assert!(
            processes[1].app_root,
            "the positively windowed member is the app"
        );
    }

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
    fn file_nr_first_field_is_the_open_handle_count() {
        assert_eq!(
            parse_file_nr("12345\t0\t9223372036854775807\n"),
            Some(12345)
        );
        assert_eq!(parse_file_nr(""), None);
    }

    #[test]
    fn parses_proc_stat_btime_and_comm_with_parens_and_spaces() {
        let stat = "cpu  1 2 3 4\nbtime 1700000000\n";
        assert_eq!(parse_proc_stat_btime(stat), Some(1_700_000_000));

        let pid_stat =
            b"42 (Web Content (\xff)) S 1 42 42 0 -1 4194304 10 0 2 0 1 0 0 0 20 0 4 0 12345 1000 50";
        assert_eq!(parse_proc_stat_starttime(pid_stat), Some(12_345));

        let ts_a = linux_start_timestamp(1_700_000_000, 12_345, 100).unwrap();
        let ts_b = linux_start_timestamp(1_700_000_000, 12_346, 100).unwrap();
        assert!(ts_a >= tm_core::model::HIGH_RES_EPOCH_THRESHOLD);
        assert!(!tm_core::model::creation_timestamps_match(ts_a, ts_b));
        assert_eq!(
            tm_core::model::epoch_timestamp_to_unix_seconds(ts_a),
            1_700_000_123
        );
    }

    #[test]
    fn linux_identity_uses_raw_ticks_and_requires_observed_evidence() {
        let pid = std::process::id();
        let ticks = linux_proc_start_ticks(pid).unwrap();
        assert!(verify_process_identity(pid, Some(ticks)).is_ok());
        assert!(verify_process_identity(pid, Some(ticks + 1)).is_err());
        assert!(verify_process_identity(pid, None).is_err());
        let mut process = ProcessEntry::new(pid, "test");
        process.boot_start_ticks = Some(ticks);
        process.start_epoch_s = linux_start_timestamp(1_700_000_000, ticks, 100);
        let corrected_time = linux_start_timestamp(1_700_000_010, ticks, 100);
        assert_ne!(process.start_epoch_s, corrected_time);
        process.start_epoch_s = corrected_time;
        assert_eq!(process.creation_identity(), Some(ticks));
    }

    #[test]
    fn only_a_permission_failure_with_a_known_identity_reaches_the_broker() {
        let denied: Result<()> = Err(tm_core::TmError::platform(
            "read /proc/pid/maps",
            "/proc/4242/maps: Permission denied (os error 13)",
        ));
        assert!(may_escalate(&denied, Some(9)));
        assert!(
            !may_escalate(&denied, None),
            "an action without a creation identity must stay local"
        );
        let io_denied: Result<()> = Err(tm_core::TmError::Io(std::io::Error::from_raw_os_error(
            libc::EPERM,
        )));
        assert!(may_escalate(&io_denied, Some(9)));
        assert!(
            !may_escalate(&Ok(()), Some(9)),
            "a direct success must never pay a round trip"
        );
        let vanished: Result<()> = Err(tm_core::TmError::ProcessNotFound { pid: 4242 });
        assert!(
            !may_escalate(&vanished, Some(9)),
            "a vanished target answers the same under any identity"
        );
    }
}
