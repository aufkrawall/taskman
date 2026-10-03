//! macOS backend: sysinfo basics + launchd listing + LaunchAgents startup.
//! Honest scope: read-mostly; process control via signals.

mod services;
mod startup;

use crate::actions::*;
use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{
    Disks, MemoryRefreshKind, Networks, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
    UpdateKind,
};
use tm_core::error::Result;
use tm_core::model::*;
use tm_core::{classify, engine::SystemCollector};

pub struct MacCollector {
    sys: System,
    disks: Disks,
    networks: Networks,
    prev_net_totals: HashMap<String, (u64, u64)>,
    last_tick: Option<Instant>,
    first_tick_done: bool,
}

impl SystemCollector for MacCollector {
    fn backend_name(&self) -> &'static str {
        "macos/sysinfo"
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

        let n_procs = self.sys.processes().len();
        let mut processes = Vec::with_capacity(n_procs);
        for (pid, p) in self.sys.processes() {
            let pid_u = pid.as_u32();
            let name = p.name().to_string_lossy().into_owned();

            let category = classify::classify(classify::ClassifyInput {
                pid: pid_u,
                name: &name,
                has_window: false,
                critical: None,
                windows_owned: None,
            });

            let du = p.disk_usage();
            let mut entry = ProcessEntry::new(pid_u, name.clone());
            entry.display = name.clone();
            entry.ppid = p.parent().map(|x| x.as_u32());
            entry.category = category;
            // sysinfo returns percent of ONE core; normalize to share of
            // total machine capacity (TM-style, max 100).
            entry.cpu_pct = (p.cpu_usage() / nb_cpus).clamp(0.0, 100.0);
            entry.mem_bytes = p.memory();
            entry.working_set_bytes = Some(p.memory());
            entry.commit_bytes = Some(p.virtual_memory());
            entry.start_epoch_s = Some(p.start_time() as i64);
            // macOS accumulated cpu time unit: microseconds in sysinfo? treat ms fallback.
            entry.cpu_time_s = Some(p.accumulated_cpu_time() as f64 / 1000.0);
            entry.disk_read_bps = du.read_bytes as f64 / interval_s;
            entry.disk_write_bps = du.written_bytes as f64 / interval_s;
            entry.disk_read_total = du.total_read_bytes;
            entry.disk_write_total = du.total_written_bytes;
            entry.exe_path = p.exe().map(|e| e.to_path_buf());
            entry.user = None;
            entry.status = bsd_run_state(pid_u)
                .map_or_else(|| status_from_sysinfo(p.status()), status_from_bsd);
            processes.push(entry);
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
                kind: if name.starts_with("en") {
                    "Ethernet".to_string()
                } else {
                    String::new()
                },
                oper_up: recv_total > 0 || sent_total > 0,
                recv_bps,
                sent_bps,
                total_recv_bytes: recv_total,
                total_sent_bytes: sent_total,
                link_bps: 0,
                ssid: None,
                ipv4: None,
                ipv6: None,
                signal_quality_pct: None,
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
                // macOS exposes no user/kernel split through sysinfo, so the
                // kernel overlay stays honestly empty (rendered as "—").
                per_core_kernel_pct: Vec::new(),
                kernel_pct: 0.0,
                freq_mhz: freq,
                freq_base_mhz: 0.0,
                logical_count: logical,
                physical_cores: 0,
                sockets: 1,
                l1_kb: 0,
                l2_kb: 0,
                l3_kb: 0,
                virtualization: "Unknown".into(),
            },
            memory: MemoryInfo {
                total_bytes: self.sys.total_memory(),
                used_bytes: self.sys.used_memory(),
                available_bytes: self.sys.available_memory(),
                cached_bytes: 0,
                commit_total_bytes: 0,
                commit_used_bytes: 0,
                paged_pool_bytes: 0,
                non_paged_pool_bytes: 0,
                swap_total_bytes: self.sys.total_swap(),
                swap_used_bytes: self.sys.used_swap(),
                ..Default::default()
            },
            disks: self
                .disks
                .list()
                .iter()
                .map(|d| {
                    // IOBlockStorageDriver byte counters of the backing disk.
                    // A first sample has no measured interval, and the model
                    // has no "unknown" rate, so it reads 0 B/s.
                    let io = d.usage();
                    let rate = |bytes: u64| {
                        if self.first_tick_done {
                            bytes as f64 / interval_s
                        } else {
                            0.0
                        }
                    };
                    DiskInfo {
                        id: d.name().to_string_lossy().to_string(),
                        mount: d.mount_point().to_string_lossy().to_string(),
                        label: String::new(),
                        media: MediaKind::Unknown,
                        total_bytes: d.total_space(),
                        free_bytes: d.available_space(),
                        // No per-disk busy-time source is wired up here yet;
                        // "unknown" is the honest answer, not a flat 0 %.
                        active_pct: None,
                        read_bps: rate(io.read_bytes),
                        write_bps: rate(io.written_bytes),
                        avg_resp_ms: 0.0,
                        total_read_bytes: io.total_read_bytes,
                        total_written_bytes: io.total_written_bytes,
                    }
                })
                .collect(),
            networks: nets,
            gpus: Vec::new(), // no public per-frame GPU metrics without IOReport
            processes,
            system: SystemMisc {
                hostname: hostname(),
                os_name: System::name().unwrap_or_else(|| "macOS".into()),
                os_version: System::os_version().unwrap_or_default(),
                kernel_version: System::kernel_version().unwrap_or_default(),
                uptime_s: System::uptime(),
                boot_epoch_s: System::boot_time() as i64,
                process_count: n_procs,
                thread_count: threads_total(&self.sys),
                handle_count: open_file_count(),
            },
        };
        self.first_tick_done = true;
        Ok(snap)
    }
}

fn hostname() -> String {
    System::host_name().unwrap_or_default()
}

/// BSD run state (`p_stat`), which SIGSTOP/SIGCONT flip. sysinfo samples
/// `pbi_status` only when it first sees a process and afterwards derives
/// the status from the first thread's Mach run state, so a process
/// suspended after first sight need not read as stopped and would never
/// offer Resume. `None` when the kernel refuses the query; the caller then
/// falls back to sysinfo's status.
fn bsd_run_state(pid: u32) -> Option<u32> {
    // SAFETY: proc_bsdshortinfo is plain integer data; all-zero is valid.
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    // SAFETY: `info` is writable and exactly `size` bytes long.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (written == size).then_some(info.pbsi_status)
}

fn status_from_bsd(p_stat: u32) -> ProcStatus {
    if p_stat == libc::SSTOP {
        ProcStatus::Suspended
    } else {
        ProcStatus::Running
    }
}

fn status_from_sysinfo(status: sysinfo::ProcessStatus) -> ProcStatus {
    match status {
        sysinfo::ProcessStatus::Stop => ProcStatus::Suspended,
        _ => ProcStatus::Running,
    }
}

/// System-wide open files (`kern.num_files`, XNU's `nfiles`), the macOS
/// analog of the Windows handle total. The model has no "unknown" for this
/// counter, so a failed sysctl still yields 0.
fn open_file_count() -> usize {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated, `value`/`len` describe a writable
    // c_int, and no new value is passed.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.num_files".as_ptr(),
            (&raw mut value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && len == std::mem::size_of::<libc::c_int>() {
        usize::try_from(value).unwrap_or(0)
    } else {
        0
    }
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

pub struct MacActions;

impl PlatformActions for MacActions {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            services_list: true,
            services_control: true,
            startup_toggle: false,
            users_sessions: false,
            user_disconnect: false,
            end_process: true,
            suspend_resume: true,
            set_priority: true,
            set_affinity: false,
            efficiency_mode: false,
            run_new_task: true,
            per_process_network: false,
            process_modules: false,
            unload_module: false,
            start_with_windows: false,
        }
    }

    fn list_services(&self) -> Result<Vec<ServiceInfo>> {
        services::list_launchctl()
    }
    fn control_service(&self, name: &str, action: ServiceAction) -> Result<()> {
        services::control_label(name, action)
    }
    fn list_startup(&self) -> Result<Vec<StartupItem>> {
        Ok(startup::list_plists())
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

    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(if elevate { "sudo" } else { "sh" });
        if elevate {
            cmd.arg("sh");
        }
        cmd.arg("-c").arg(command_line);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| tm_core::TmError::platform("spawn", e.to_string()))
    }

    fn backend_name(&self) -> &'static str {
        "macos"
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

pub fn create_collector() -> MacCollector {
    MacCollector {
        sys: System::new_all(),
        disks: Disks::new_with_refreshed_list(),
        networks: Networks::new_with_refreshed_list(),
        prev_net_totals: HashMap::new(),
        last_tick: None,
        first_tick_done: false,
    }
}

pub fn create_actions() -> MacActions {
    MacActions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_process_reads_as_suspended() {
        assert_eq!(status_from_bsd(libc::SSTOP), ProcStatus::Suspended);
        assert_eq!(status_from_bsd(libc::SRUN), ProcStatus::Running);
        assert_eq!(
            status_from_sysinfo(sysinfo::ProcessStatus::Stop),
            ProcStatus::Suspended
        );
        assert_eq!(
            status_from_sysinfo(sysinfo::ProcessStatus::Sleep),
            ProcStatus::Running
        );
    }

    #[test]
    fn own_process_run_state_is_readable() {
        assert_eq!(
            bsd_run_state(std::process::id()).map(status_from_bsd),
            Some(ProcStatus::Running)
        );
    }
}
