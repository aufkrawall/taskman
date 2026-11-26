//! macOS backend: sysinfo basics + launchd listing + LaunchAgents startup.
//! Honest scope: read-mostly; process control via signals.

mod services;
mod startup;

use crate::actions::*;
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
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
        // Bind each actionable row to the generation that spans the refresh.
        let before_refresh: HashMap<_, _> = self
            .sys
            .processes()
            .keys()
            .filter_map(|pid| {
                let pid = pid.as_u32();
                macos_proc_start_timestamp(pid).map(|start| (pid, start))
            })
            .collect();
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
            // Sysinfo's macOS cache distinguishes generations at whole-second
            // precision. Get the displayed image from the native observation
            // too, so same-second PID reuse cannot preserve a stale label.
            let observed = before_refresh.get(&pid_u).and_then(|&before| {
                let path = macos_process_path(pid_u);
                let info = bsd_info(pid_u)?;
                (macos_start_timestamp(info.pbi_start_tvsec, info.pbi_start_tvusec) == Some(before))
                    .then_some((before, path, info))
            });
            let name = observed.as_ref().map_or_else(
                || p.name().to_string_lossy().into_owned(),
                |(_, path, info)| {
                    path.as_deref()
                        .and_then(std::path::Path::file_name)
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| macos_process_name(info))
                },
            );

            let category = classify::classify(classify::ClassifyInput {
                platform: classify::ProcessPlatform::Unix,
                pid: pid_u,
                name: &name,
                has_window: None,
                critical: None,
                windows_owned: None,
                desktop_role: None,
            });

            let du = p.disk_usage();
            let mut entry = ProcessEntry::new(pid_u, name.clone());
            entry.display = name.clone();
            entry.ppid = p.parent().map(|x| x.as_u32());
            entry.category = category;
            // sysinfo returns percent of ONE core; normalize to share of
            // total machine capacity (TM-style, max 100).
            entry.cpu_pct = Some((p.cpu_usage() / nb_cpus).clamp(0.0, 100.0));
            entry.mem_bytes = p.memory();
            entry.working_set_bytes = Some(p.memory());
            entry.commit_bytes = Some(p.virtual_memory());
            entry.start_epoch_s = observed.as_ref().map(|(start, _, _)| *start);
            // macOS accumulated cpu time unit: microseconds in sysinfo? treat ms fallback.
            entry.cpu_time_s = Some(p.accumulated_cpu_time() as f64 / 1000.0);
            entry.disk_read_bps = du.read_bytes as f64 / interval_s;
            entry.disk_write_bps = du.written_bytes as f64 / interval_s;
            entry.disk_read_total = du.total_read_bytes;
            entry.disk_write_total = du.total_written_bytes;
            entry.user = None;
            if let Some((_, path, info)) = observed {
                entry.exe_path = path;
                entry.status = status_from_bsd(info.pbi_status);
            } else {
                entry.exe_path = p.exe().map(|e| e.to_path_buf());
                entry.status = bsd_run_state(pid_u)
                    .map_or_else(|| status_from_sysinfo(p.status()), status_from_bsd);
            }
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
                utilization_pct_available: true,
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
                        number: None,
                        mount: d.mount_point().to_string_lossy().to_string(),
                        label: String::new(),
                        media: MediaKind::Unknown,
                        total_bytes: d.total_space(),
                        free_bytes: d.available_space(),
                        // No per-disk busy-time source is wired up here yet;
                        // "unknown" is the honest answer, not a flat 0 %.
                        active_pct: None,
                        // The byte counters ARE measured once an interval
                        // exists, even though the busy-time source above does
                        // not exist at all.
                        rates_measured: self.first_tick_done,
                        read_bps: rate(io.read_bytes),
                        write_bps: rate(io.written_bytes),
                        avg_resp_ms: 0.0,
                        total_read_bytes: io.total_read_bytes,
                        total_written_bytes: io.total_written_bytes,
                        model: String::new(),
                        volumes: Vec::new(),
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
            open_services_app: false,
            startup_toggle: false,
            end_process_tree: false,
            users_sessions: false,
            user_disconnect: false,
            manage_user_accounts: false,
            end_process: true,
            suspend_resume: true,
            set_priority: true,
            set_affinity: false,
            efficiency_mode: false,
            run_new_task: true,
            run_elevated: false,
            per_process_network: false,
            process_modules: false,
            process_threads: false,
            process_connections: false,
            process_file_io: false,
            unload_module: false,
            start_with_system: false,
            elevation_status: true,
            relaunch_elevated: false,
            start_elevated: false,
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
        self.kill_process(pid, macos_proc_start_timestamp(pid), false)
    }

    fn suspend_process(&self, pid: u32, suspend: bool) -> Result<()> {
        self.suspend_process_checked(pid, macos_proc_start_timestamp(pid), suspend)
    }

    fn set_priority(&self, pid: u32, priority: PriorityClass) -> Result<()> {
        self.set_priority_checked(pid, macos_proc_start_timestamp(pid), priority)
    }

    fn kill_process(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        tree: bool,
    ) -> Result<()> {
        let _ = tree;
        send_signal_checked(pid, expected_start_epoch_s, libc::SIGTERM)
    }
    fn suspend_process_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        suspend: bool,
    ) -> Result<()> {
        send_signal_checked(
            pid,
            expected_start_epoch_s,
            if suspend {
                libc::SIGSTOP
            } else {
                libc::SIGCONT
            },
        )
    }
    fn set_priority_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        priority: PriorityClass,
    ) -> Result<()> {
        validate_mutation_target(pid)?;
        // An unknown class is not a request for a specific priority, and the
        // Linux backend refuses it too. Falling through to nice 10 below would
        // change a target nobody asked to change.
        if priority == PriorityClass::Unknown {
            return Err(tm_core::TmError::Unsupported("unknown priority class"));
        }
        verify_process_identity(pid, expected_start_epoch_s)?;
        let nice: i32 = match priority {
            PriorityClass::Realtime | PriorityClass::High => -5,
            PriorityClass::AboveNormal => -2,
            PriorityClass::Normal => 0,
            PriorityClass::BelowNormal => 5,
            PriorityClass::Low | PriorityClass::Unknown => 10,
        };
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
        if rc == 0 {
            return Ok(());
        }
        // `setpriority` fails for reasons other than the permission denied this
        // used to report unconditionally: ESRCH means the target is gone, and
        // only EPERM/EACCES mean the caller may not change it.
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Err(tm_core::TmError::ProcessNotFound { pid });
        }
        Err(tm_core::TmError::platform("setpriority", error.to_string()))
    }

    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        use std::process::{Command, Stdio};
        if elevate {
            return Err(tm_core::TmError::Unsupported(
                "elevated arbitrary task launch",
            ));
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command_line);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd
            .spawn()
            .map_err(|e| tm_core::TmError::platform("spawn", e.to_string()))?;
        let child = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
        let reaper_child = std::sync::Arc::clone(&child);
        let spawned = std::thread::Builder::new()
            .name("tm-macos-run-reaper".into())
            .spawn(move || {
                if let Some(mut child) = reaper_child
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    let _ = child.wait();
                }
            });
        if let Err(error) = spawned {
            if let Some(mut child) = child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err(tm_core::TmError::platform(
                "spawn reaper",
                error.to_string(),
            ));
        }
        Ok(())
    }
    fn run_new_task_probe(&self, _command_line: &str, _elevate: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported(
            "non-launching run probe on macOS",
        ))
    }
    fn is_elevated(&self) -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    fn backend_name(&self) -> &'static str {
        "macos"
    }
}

/// Refuse the two targets a signal or a priority change must never name.
///
/// macOS `SIGTERM` to `launchd` (pid 1) takes the whole machine into a restart,
/// and signalling this process would end the app from inside its own action.
/// The Linux backend refuses the same two targets.
fn validate_mutation_target(pid: u32) -> Result<()> {
    if pid <= 1 || pid == std::process::id() {
        return Err(tm_core::TmError::platform(
            "process mutation",
            format!("pid {pid} is TaskMan itself or the init process"),
        ));
    }
    Ok(())
}

fn send_signal(pid: u32, sig: i32) -> Result<()> {
    validate_mutation_target(pid)?;
    let Ok(pid_t) = libc::pid_t::try_from(pid) else {
        return Err(tm_core::TmError::ProcessNotFound { pid });
    };
    let rc = unsafe { libc::kill(pid_t, sig) };
    if rc == 0 {
        Ok(())
    } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Err(tm_core::TmError::ProcessNotFound { pid })
    } else {
        Err(tm_core::TmError::platform(
            "kill",
            format!("signal {sig} failed"),
        ))
    }
}

fn send_signal_checked(pid: u32, expected_start_epoch_s: Option<i64>, sig: i32) -> Result<()> {
    validate_mutation_target(pid)?;
    verify_process_identity(pid, expected_start_epoch_s)?;
    send_signal(pid, sig)
}

fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid_i32 = i32::try_from(pid).ok()?;
    let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
    let expected = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let rc = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            expected,
        )
    };
    (rc == expected).then_some(info)
}

fn macos_process_path(pid: u32) -> Option<PathBuf> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let written = unsafe {
        libc::proc_pidpath(
            i32::try_from(pid).ok()?,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
        )
    };
    let path = buf
        .get(..usize::try_from(written).ok()?)?
        .split(|&byte| byte == 0)
        .next()?;
    (!path.is_empty()).then(|| std::ffi::OsStr::from_bytes(path).into())
}

fn macos_process_name(info: &libc::proc_bsdinfo) -> String {
    let name = if info.pbi_name[0] == 0 {
        info.pbi_comm.as_slice()
    } else {
        info.pbi_name.as_slice()
    };
    let bytes: Vec<_> = name
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn macos_start_timestamp(tvsec: u64, tvusec: u64) -> Option<i64> {
    if tvsec == 0 && tvusec == 0 {
        return None;
    }
    let sec_ticks =
        i128::from(tvsec).checked_mul(i128::from(tm_core::model::HIGH_RES_TICKS_PER_SECOND))?;
    let usec_ticks = i128::from(tvusec).checked_mul(10)?;
    i64::try_from(sec_ticks.checked_add(usec_ticks)?).ok()
}

fn macos_proc_start_timestamp(pid: u32) -> Option<i64> {
    let info = bsd_info(pid)?;
    macos_start_timestamp(info.pbi_start_tvsec, info.pbi_start_tvusec)
}

fn verify_process_identity(pid: u32, expected_start_epoch_s: Option<i64>) -> Result<()> {
    let expected = expected_start_epoch_s.ok_or_else(|| {
        tm_core::TmError::platform(
            "verify process identity",
            "process creation identity unavailable",
        )
    })?;
    let Some(actual) = macos_proc_start_timestamp(pid) else {
        return Err(tm_core::TmError::ProcessNotFound { pid });
    };
    if expected != actual {
        return Err(tm_core::TmError::ProcessNotFound { pid });
    }
    Ok(())
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

    #[test]
    fn microsecond_timestamps_catch_same_second_reuse() {
        let ts_a = macos_start_timestamp(1_700_000_000, 100).unwrap();
        let ts_b = macos_start_timestamp(1_700_000_000, 200).unwrap();
        assert!(ts_a >= tm_core::model::HIGH_RES_EPOCH_THRESHOLD);
        assert!(!tm_core::model::creation_timestamps_match(ts_a, ts_b));
        assert_eq!(
            tm_core::model::epoch_timestamp_to_unix_seconds(ts_a),
            1_700_000_000
        );
    }

    #[test]
    fn capabilities_do_not_expose_windows_or_elevated_run_surfaces() {
        let capabilities = create_actions().capabilities();
        assert!(!capabilities.open_services_app);
        assert!(!capabilities.manage_user_accounts);
        assert!(!capabilities.start_with_system);
        assert!(!capabilities.run_elevated);
        assert!(!capabilities.relaunch_elevated);
        assert!(!capabilities.start_elevated);
        assert!(!capabilities.end_process_tree);
    }

    #[test]
    fn elevated_run_never_reaches_the_sudo_path() {
        let error = create_actions().run_new_task("true", true).unwrap_err();
        assert!(matches!(error, tm_core::TmError::Unsupported(_)));
    }
}
